//! UDP client implementation
pub mod actions;

pub use actions::UdpClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::udp::actions::{UDP_CLIENT_CONNECTED_EVENT, UDP_CLIENT_DATAGRAM_RECEIVED_EVENT};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// Per-client data for LLM handling.
///
/// There is no Idle/Processing/Accumulating state machine here any more, and the one that was
/// here did active harm rather than nothing.
///
/// The receive loop awaits its LLM call inline, so `recv_from` is not called again until the
/// call returns — `Processing` was therefore unreachable from the loop itself, and datagrams
/// arriving during a call wait in the socket buffer. `Accumulating` *was* reachable, via
/// `wait_for_more`, and it was **terminal**: nothing ever moved the state back, so every
/// subsequent datagram took the `Accumulating` branch, was pushed onto `queued_datagrams`, and
/// was never looked at again. One `wait_for_more` made the client permanently deaf while it went
/// on reporting Connected, and the queue grew for as long as datagrams kept arriving.
///
/// `wait_for_more` now means what the TCP server's action of the same name says: answer with
/// nothing, and the next datagram is a fresh event.
struct ClientData {
    memory: String,
    default_target: SocketAddr,
}

/// UDP client that sends/receives datagrams
pub struct UdpClient;

impl UdpClient {
    /// Bind a UDP socket and integrate with LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Parse remote address for default target
        let default_target: SocketAddr = remote_addr
            .parse()
            .context(format!("Failed to parse remote address: {}", remote_addr))?;

        // Bind to local address (0.0.0.0:0 for any available port)
        let socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind UDP socket")?;

        let local_addr = socket.local_addr()?;

        info!(
            "UDP client {} bound to {} (default target: {})",
            client_id, local_addr, default_target
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] UDP client {} ready", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            memory: String::new(),
            default_target,
        }));

        let socket_arc = Arc::new(socket);

        // Command channel for injected actions (the dashboard's [ send ]).
        //
        // Registered BEFORE the connected-event LLM call below: a dashboard-created client
        // defaults to a `*` -> manual routing rule, so that call can park for minutes waiting
        // for a human, and [ send ] must work for the whole park.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        // The receive loop's body awaits an LLM call inline, so a `select!` arm there would
        // stall every injected command for the length of that call. The socket is an
        // `Arc<UdpSocket>` and `send_to` needs only `&self`, so commands get their own task
        // and share the socket directly - no write-half mutex required.
        let cmd_state = app_state.clone();
        let cmd_status_tx = status_tx.clone();
        let cmd_socket = socket_arc.clone();
        let cmd_data = client_data.clone();
        let cmd_task = tokio::spawn(async move {
            Self::command_loop(
                command_rx,
                cmd_socket,
                cmd_data,
                client_id,
                cmd_state,
                cmd_status_tx,
            )
            .await;
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Call LLM with connected event
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn({
            let socket_arc = socket_arc.clone();
            let client_data = client_data.clone();
            let llm_client = llm_client.clone();
            let app_state = app_state.clone();
            let status_tx = status_tx.clone();

            async move {
                // Get instruction for this client
                if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
                    let protocol = Arc::new(UdpClientProtocol::new());
                    let connected_event = Event::new(
                        &UDP_CLIENT_CONNECTED_EVENT,
                        serde_json::json!({
                            "remote_addr": default_target.to_string(),
                            "local_addr": local_addr.to_string(),
                        }),
                    );

                    // Call LLM with connected event
                    match call_llm_for_client(
                        &llm_client,
                        &app_state,
                        client_id.to_string(),
                        &instruction,
                        "",
                        Some(&connected_event),
                        protocol.as_ref(),
                        &status_tx,
                    )
                    .await
                    {
                        Ok(llm_result) => {
                            if let Err(e) = Self::handle_llm_result(
                                llm_result,
                                &socket_arc,
                                &client_data,
                                client_id,
                                &app_state,
                                &status_tx,
                            )
                            .await
                            {
                                error!(
                                    "Error handling LLM result for UDP client {}: {}",
                                    client_id, e
                                );
                            }
                        }
                        Err(e) => {
                            error!(
                                "Failed to call LLM for UDP client {} connected event: {}",
                                client_id, e
                            );
                        }
                    }
                }
            }
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        // Spawn receive loop
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn({
            let socket_arc = socket_arc.clone();
            let client_data = client_data.clone();

            async move {
                let mut buffer = vec![0u8; 65536]; // Max UDP datagram size

                loop {
                    match socket_arc.recv_from(&mut buffer).await {
                        Ok((n, source_addr)) => {
                            let data = buffer[..n].to_vec();
                            trace!(
                                "UDP client {} received {} bytes from {}",
                                client_id,
                                n,
                                source_addr
                            );

                            // Handle the datagram with the LLM. Sequential by construction: the
                            // next `recv_from` does not run until this returns, and datagrams
                            // arriving meanwhile wait in the socket buffer.
                            match Self::process_datagram(
                                data,
                                source_addr,
                                client_id,
                                &llm_client,
                                &app_state,
                                &status_tx,
                                &socket_arc,
                                &client_data,
                            )
                            .await
                            {
                                // `close_socket` used to be honoured only by the injected-command
                                // path: here `handle_llm_result` marked the client Disconnected
                                // and returned Ok, and this loop carried straight on receiving
                                // and calling the model on a client the dashboard showed as
                                // closed.
                                Ok(true) => {
                                    info!(
                                        "UDP client {} closing: decision=model_close_socket",
                                        client_id
                                    );
                                    break;
                                }
                                Ok(false) => {}
                                Err(e) => error!(
                                    "Error processing UDP datagram for client {}: {}",
                                    client_id, e
                                ),
                            }
                        }
                        Err(e) => {
                            error!("UDP client {} receive error: {}", client_id, e);
                            app_state
                                .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                                .await;
                            let _ = status_tx
                                .send(format!("[CLIENT] UDP client {} error: {}", client_id, e));
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            break;
                        }
                    }
                }
                // The socket is dead: drop the command handle so the dashboard stops
                // offering [ send ] into it (a late send then fails fast rather than
                // queueing into a loop that will never run again).
                app_state.remove_client_handle(client_id).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
            }
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(local_addr)
    }

    /// Process a received datagram with the LLM.
    ///
    /// Returns `true` when an action asked to close the socket, so the caller can stop.
    async fn process_datagram(
        data: Vec<u8>,
        source_addr: SocketAddr,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        socket: &Arc<UdpSocket>,
        client_data: &Arc<Mutex<ClientData>>,
    ) -> Result<bool> {
        let data_hex = hex::encode(&data);
        let data_len = data.len();

        // Get instruction for this client
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let protocol = Arc::new(UdpClientProtocol::new());
            let event = Event::new(
                &UDP_CLIENT_DATAGRAM_RECEIVED_EVENT,
                serde_json::json!({
                    "data_hex": data_hex,
                    "data_length": data_len,
                    "source_addr": source_addr.to_string(),
                }),
            );

            // Get current memory
            let memory = {
                let client_data_lock = client_data.lock().await;
                client_data_lock.memory.clone()
            };

            // Call LLM
            let llm_result = call_llm_for_client(
                llm_client,
                app_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                protocol.as_ref(),
                status_tx,
            )
            .await?;

            // Handle LLM result
            return Self::handle_llm_result(
                llm_result,
                socket,
                client_data,
                client_id,
                app_state,
                status_tx,
            )
            .await;
        }

        Ok(false)
    }

    /// Handle LLM result and execute actions.
    ///
    /// Returns `true` when an action asked to close the socket.
    async fn handle_llm_result(
        llm_result: ClientLlmResult,
        socket: &Arc<UdpSocket>,
        client_data: &Arc<Mutex<ClientData>>,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<bool> {
        // Update memory if needed
        if let Some(new_memory) = llm_result.memory_updates {
            let mut client_data_lock = client_data.lock().await;
            client_data_lock.memory = new_memory;
        }

        // Execute actions.
        //
        // Both statements used to end in `?`, so one unusable action discarded every action
        // after it — a model answering `[send_udp_datagram, typo, send_udp_datagram]` sent one
        // datagram and lost the third. Each is reported and the loop continues.
        let protocol = Arc::new(UdpClientProtocol::new());
        for action in llm_result.actions {
            let action_result = match protocol.as_ref().execute_action(action) {
                Ok(result) => result,
                Err(e) => {
                    error!(
                        "UDP client {} rejected one of its actions: {}",
                        client_id, e
                    );
                    continue;
                }
            };

            match Self::apply_action_result(action_result, socket, client_data, client_id).await {
                Ok(Applied::Disconnect) => {
                    app_state
                        .update_client_status(client_id, ClientStatus::Disconnected)
                        .await;
                    let _ = status_tx.send(format!("[CLIENT] UDP client {} closed", client_id));
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                    return Ok(true);
                }
                Ok(_) => {}
                Err(e) => error!("UDP client {} could not apply an action: {}", client_id, e),
            }
        }

        Ok(false)
    }

    /// Put one executed action on the wire.
    ///
    /// Shared by the connected-event path, the receive loop and injected commands, so the
    /// datagram encoding and the target-address resolution exist exactly once and an
    /// injected action behaves identically to an LLM-produced one.
    async fn apply_action_result(
        action_result: ClientActionResult,
        socket: &Arc<UdpSocket>,
        client_data: &Arc<Mutex<ClientData>>,
        client_id: ClientId,
    ) -> Result<Applied> {
        match action_result {
            ClientActionResult::SendData(_) => {
                // Not used for UDP (we use Custom with target_addr)
                warn!("SendData action not supported for UDP client, use send_udp_datagram");
                Ok(Applied::Executed(
                    "send_data is not a UDP client verb; use send_udp_datagram".to_string(),
                ))
            }
            ClientActionResult::Custom { name, data } => {
                if name == "send_udp_datagram" {
                    let data_bytes = data["data"]
                        .as_array()
                        .context("Missing 'data' array in send_udp_datagram")?
                        .iter()
                        .map(|v| v.as_u64().unwrap_or(0) as u8)
                        .collect::<Vec<u8>>();

                    let target_addr = if let Some(target) = data["target_addr"].as_str() {
                        target
                            .parse::<SocketAddr>()
                            .context(format!("Invalid target address: {}", target))?
                    } else {
                        // Use default target or last source
                        let client_data_lock = client_data.lock().await;
                        client_data_lock.default_target
                    };

                    // Send datagram. `send_to` reports what actually left the socket, so
                    // the byte count handed back to the caller is the real one.
                    let sent = socket.send_to(&data_bytes, target_addr).await?;
                    trace!(
                        "UDP client {} sent {} bytes to {}",
                        client_id,
                        sent,
                        target_addr
                    );
                    Ok(Applied::Sent(sent))
                } else if name == "change_target" {
                    let new_target_str = data["new_target"]
                        .as_str()
                        .context("Missing 'new_target' in change_target action")?;

                    let new_target = new_target_str
                        .parse::<SocketAddr>()
                        .context(format!("Invalid target address: {}", new_target_str))?;

                    let mut client_data_lock = client_data.lock().await;
                    client_data_lock.default_target = new_target;
                    info!(
                        "UDP client {} changed default target to {}",
                        client_id, new_target
                    );
                    Ok(Applied::Executed(format!(
                        "default target changed to {new_target}; nothing written to the wire"
                    )))
                } else {
                    Ok(Applied::Executed(format!(
                        "custom result '{name}' is not a UDP client verb"
                    )))
                }
            }
            ClientActionResult::Disconnect => {
                info!("UDP client {} closing socket", client_id);
                Ok(Applied::Disconnect)
            }
            ClientActionResult::WaitForMore => {
                // Answer with nothing and keep listening. This used to move the client into a
                // terminal `Accumulating` state that nothing ever left, so one `wait_for_more`
                // made it permanently deaf: every later datagram was pushed onto a queue nobody
                // read, while the client still reported Connected.
                trace!("UDP client {} answered with nothing", client_id);
                Ok(Applied::Executed(
                    "wait_for_more: nothing written to the wire; the next datagram is a fresh \
                     event"
                        .to_string(),
                ))
            }
            ClientActionResult::NoAction => Ok(Applied::Executed(
                "no_action: nothing written to the wire".to_string(),
            )),
            ClientActionResult::Multiple(_) => {
                warn!("Multiple actions not yet supported in UDP client");
                Ok(Applied::Executed(
                    "multiple actions are not yet supported by the UDP client".to_string(),
                ))
            }
        }
    }

    /// Drain injected commands until the channel closes (the client was removed) or an
    /// injected `close_socket` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot serve this client:
    /// its verbs yield `ClientActionResult::Custom` and the destination is a datagram address,
    /// not a stream write half. The action still goes through [`Self::apply_action_result`] -
    /// the same function the LLM path uses - and the outcome is logged and replied exactly the
    /// way the generic arm does it.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        socket: Arc<UdpSocket>,
        client_data: Arc<Mutex<ClientData>>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;

        let protocol = UdpClientProtocol::new();

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => {
                    match Self::apply_action_result(result, &socket, &client_data, client_id).await
                    {
                        // A UDP datagram either left the socket or it did not; `send_to`
                        // returns the count, so this is never an estimate.
                        Ok(Applied::Sent(bytes_sent)) => Ok(ClientSendOutcome::Sent { bytes_sent }),
                        Ok(Applied::Executed(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                        Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                        Err(e) => Err(e),
                    }
                }
            };

            let outcome_json = match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            };
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![outcome_json],
                )
                .await;

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            if let Err(e) = &outcome {
                error!("UDP client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                let _ = status_tx.send(format!(
                    "[CLIENT] UDP client {} closed (injected action)",
                    client_id
                ));
                break;
            }
        }

        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }
}

/// What [`UdpClient::apply_action_result`] did with one action.
enum Applied {
    /// Bytes that actually left the socket in a datagram.
    Sent(usize),
    /// The action ran but wrote nothing; the string says why.
    Executed(String),
    /// The socket should be considered closed.
    Disconnect,
}
