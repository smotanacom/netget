//! USB CDC ACM Serial server implementation.
//!
//! Exports a virtual serial port over USB/IP. A Linux host that imports it sees
//! `/dev/ttyACM0`; anything speaking USB/IP over TCP can drive it without a kernel module.
//!
//! The USB/IP session runs on the socket netget already accepted (`usbip::handler`) rather
//! than through `usbip::server()`, which would bind a second listener on a fixed port.
//!
//! Event flow:
//!
//! * `usb_serial_attached` — a host connected and the port is live.
//! * `usb_serial_data_received` — the host wrote to the port. `handle_urb` is synchronous and
//!   cannot call the LLM, so the handler forwards bytes over a channel and this module raises
//!   the event from the connection task.
//! * `usb_serial_detached` — the USB/IP session ended (host detached, or the socket closed).

pub mod actions;

#[cfg(feature = "usb-serial")]
pub mod handler;

// Re-export protocol struct for registration
#[cfg(feature = "usb-serial")]
pub use actions::UsbSerialProtocol;

#[cfg(feature = "usb-serial")]
use anyhow::Result;
#[cfg(feature = "usb-serial")]
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
#[cfg(feature = "usb-serial")]
use tokio::sync::{mpsc, Mutex};
#[cfg(feature = "usb-serial")]
use tracing::{debug, error, info, warn};

#[cfg(feature = "usb-serial")]
use crate::{
    console_error, llm::action_helper::call_llm, llm::OllamaClient, protocol::Event,
    server::connection::ConnectionId, state::app_state::AppState,
};

#[cfg(feature = "usb-serial")]
use actions::{
    USB_SERIAL_ATTACHED_EVENT, USB_SERIAL_DATA_RECEIVED_EVENT, USB_SERIAL_DETACHED_EVENT,
};

/// Most host-written bytes one `usb_serial_data_received` event will carry.
///
/// The loop below coalesces everything the host wrote while the previous LLM call was in
/// flight, so this is a bound on a backlog that grows at the host's line rate for the length
/// of a model round-trip -- and it was unbounded. The bytes were then copied a second time by
/// `from_utf8_lossy` and a third into the event JSON, all of which the model then has to read.
/// 8 KiB is well past any line a serial peer sends in one go; the remainder is reported to the
/// host as a CDC overrun, which is exactly what a real port says when its receive buffer fills.
#[cfg(feature = "usb-serial")]
const MAX_EVENT_BYTES: usize = 8 * 1024;

#[cfg(feature = "usb-serial")]
#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Idle,
    Processing,
}

#[cfg(feature = "usb-serial")]
struct ConnectionData {
    state: ConnectionState,
}

#[cfg(feature = "usb-serial")]
pub struct UsbSerialServer;

#[cfg(feature = "usb-serial")]
impl UsbSerialServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("USB Serial server listening on {}", local_addr);
        let _ = status_tx.send(format!("USB Serial server listening on {}", local_addr));

        let connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let protocol = Arc::new(UsbSerialProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        info!(
                            "USB/IP connection {} from {} (USB serial)",
                            connection_id, remote_addr
                        );

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: stream.local_addr().unwrap_or(local_addr),
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                                "state": "WaitingForImport",
                                "baud_rate": 115200
                            })),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let connections_clone = connections.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                stream,
                                connection_id,
                                remote_addr,
                                llm_client_clone,
                                app_state_clone,
                                status_tx_clone,
                                connections_clone,
                                protocol_clone,
                                server_id,
                            )
                            .await
                            {
                                error!("USB serial connection {} error: {}", connection_id, e);
                            }
                        });
                    }
                    Err(e) => {
                        // A persistent accept error (EMFILE, socket torn down) recurs
                        // immediately, so continuing spins a hot loop on an unbounded status
                        // channel. Give up the listener instead.
                        error!("USB serial accept failed, stopping accept loop: {}", e);
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Drive one USB/IP session and the events that hang off it.
    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        mut stream: tokio::net::TcpStream,
        connection_id: ConnectionId,
        remote_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        connections: Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        protocol: Arc<UsbSerialProtocol>,
        server_id: crate::state::ServerId,
    ) -> Result<()> {
        let local_addr = stream.local_addr().unwrap_or(remote_addr);

        connections.lock().await.insert(
            connection_id,
            ConnectionData {
                state: ConnectionState::Idle,
            },
        );

        // Host writes arrive on a synchronous URB callback, which cannot await an LLM call.
        // The channel is the seam between the two.
        let (rx_tx, mut rx_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let serial_handler = Arc::new(std::sync::Mutex::new(Box::new(
            handler::UsbCdcAcmSerialHandler::new(rx_tx),
        )
            as Box<dyn usbip::UsbInterfaceHandler + Send>));

        protocol.set_handler(connection_id, serial_handler.clone());

        let device = usbip::UsbDevice::new(0).with_interface(
            usbip::ClassCode::CDC as u8,
            usbip::cdc::CDC_ACM_SUBCLASS,
            0x01, // AT command protocol, what a CDC ACM port conventionally reports
            Some("NetGet Virtual Serial Port"),
            handler::UsbCdcAcmSerialHandler::endpoints(),
            serial_handler.clone(),
        );

        let usbip_server = Arc::new(usbip::UsbIpServer::new_simulated(vec![device]));

        let _ = status_tx.send(format!(
            "USB serial port ready on {} - run: sudo usbip attach -r {} -b 0-0-0",
            local_addr,
            local_addr.ip()
        ));

        let mut usbip_task = tokio::spawn(async move {
            match usbip::handler(&mut stream, usbip_server).await {
                Ok(()) => debug!(
                    "USB/IP session ended for serial connection {}",
                    connection_id
                ),
                Err(e) => debug!(
                    "USB/IP session for serial connection {} ended with error: {}",
                    connection_id, e
                ),
            }
        });

        Self::call_llm_for_event(
            connection_id,
            &llm_client,
            &app_state,
            &connections,
            &protocol,
            &status_tx,
            server_id,
            Event::new(
                &USB_SERIAL_ATTACHED_EVENT,
                serde_json::json!({ "connection_id": connection_id.to_string() }),
            ),
            "attach",
        )
        .await;

        // Serve host writes until the USB/IP session ends. This loop is why the detach event
        // exists at all: the rest of the USB family parks on `sleep(u64::MAX)` here and never
        // notices the host going away.
        loop {
            tokio::select! {
                received = rx_rx.recv() => {
                    let Some(mut data) = received else { break };

                    // Coalesce whatever else is already queued: while the previous LLM call
                    // was running the host may have written several times, and one event per
                    // URB would turn a paste into a burst of model round-trips.
                    //
                    // Bounded on append. What is being coalesced is precisely the backlog that
                    // accumulated while the model was busy, so an attached host writing at line
                    // rate decided the size of this buffer, of the `from_utf8_lossy` copy of it
                    // and of the event JSON built from that.
                    let mut overran = data.len() > MAX_EVENT_BYTES;
                    data.truncate(MAX_EVENT_BYTES);
                    while data.len() < MAX_EVENT_BYTES {
                        let Ok(more) = rx_rx.try_recv() else { break };
                        let room = MAX_EVENT_BYTES - data.len();
                        if more.len() > room {
                            data.extend_from_slice(&more[..room]);
                            overran = true;
                        } else {
                            data.extend_from_slice(&more);
                        }
                    }
                    if overran {
                        // Say so in CDC's own vocabulary rather than silently serving a
                        // truncated prefix to the model and nothing to the host.
                        warn!(
                            "USB serial connection {} clamped the host's backlog to {} bytes",
                            connection_id, MAX_EVENT_BYTES
                        );
                        if protocol.signal_overrun(connection_id) {
                            let _ = status_tx.send(format!(
                                "[ERROR] USB serial port {connection_id} signalled \
                                 SERIAL_STATE overrun: the host wrote more than \
                                 {MAX_EVENT_BYTES} bytes while the model was busy"
                            ));
                        }
                    }

                    let text = String::from_utf8_lossy(&data).to_string();
                    debug!(
                        "USB serial connection {} received {} byte(s) from host",
                        connection_id,
                        data.len()
                    );

                    Self::call_llm_for_event(
                        connection_id,
                        &llm_client,
                        &app_state,
                        &connections,
                        &protocol,
                        &status_tx,
                        server_id,
                        Event::new(
                            &USB_SERIAL_DATA_RECEIVED_EVENT,
                            serde_json::json!({
                                "connection_id": connection_id.to_string(),
                                "data": text,
                            }),
                        ),
                        "data_received",
                    )
                    .await;
                }
                _ = &mut usbip_task => break,
            }
        }

        info!(
            "USB serial host detached on connection {} from {}",
            connection_id, remote_addr
        );

        Self::call_llm_for_event(
            connection_id,
            &llm_client,
            &app_state,
            &connections,
            &protocol,
            &status_tx,
            server_id,
            Event::new(
                &USB_SERIAL_DETACHED_EVENT,
                serde_json::json!({ "connection_id": connection_id.to_string() }),
            ),
            "detach",
        )
        .await;

        // The session owned this handler; drop it so a later action cannot write to a port
        // that no longer exists.
        protocol.remove_handler(connection_id);
        connections.lock().await.remove(&connection_id);
        app_state
            .close_connection_on_server(server_id, connection_id)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        Ok(())
    }

    /// Raise one event with the LLM, guarding against overlapping calls on the same port.
    ///
    /// On failure the port answers in CDC's own vocabulary — see the `Err` arm.
    #[allow(clippy::too_many_arguments)]
    async fn call_llm_for_event(
        connection_id: ConnectionId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        connections: &Arc<Mutex<HashMap<ConnectionId, ConnectionData>>>,
        protocol: &Arc<UsbSerialProtocol>,
        status_tx: &mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        event: Event,
        what: &str,
    ) {
        {
            let mut conns = connections.lock().await;
            match conns.get_mut(&connection_id) {
                Some(conn) if conn.state != ConnectionState::Idle => {
                    debug!(
                        "USB serial connection {} already processing, skipping {} event",
                        connection_id, what
                    );
                    return;
                }
                Some(conn) => conn.state = ConnectionState::Processing,
                // Detach removes the entry before its own event; that is not an error.
                None => {}
            }
        }

        let result = call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await;

        match result {
            Ok(_) => info!(
                "USB serial LLM call completed for connection {} ({})",
                connection_id, what
            ),
            Err(e) => {
                console_error!(
                    status_tx,
                    "LLM call failed for USB serial connection {} ({}): {}",
                    connection_id,
                    what,
                    e
                );

                // The host wrote and nobody could answer, so those bytes are gone. CDC PSTN 1.2
                // has a word for that — `SERIAL_STATE` with `bOverRun` — and it is the only
                // thing a serial port *can* say, since the class has no request/response framing
                // to fail. Silence would be indistinguishable from a peer with nothing to send.
                //
                // Only `data_received` earns one: attach has had nothing written to it yet, and
                // by detach the port is gone.
                if what == "data_received" && protocol.signal_overrun(connection_id) {
                    let _ = status_tx.send(format!(
                        "[ERROR] USB serial port {connection_id} signalled SERIAL_STATE overrun: \
                         the host's data was dropped because the model could not be reached"
                    ));
                    warn!(
                        "USB serial connection {} queued a SERIAL_STATE overrun notification \
                         after the data_received handler failed",
                        connection_id
                    );
                }
            }
        }

        let mut conns = connections.lock().await;
        if let Some(conn) = conns.get_mut(&connection_id) {
            conn.state = ConnectionState::Idle;
        }
    }
}
