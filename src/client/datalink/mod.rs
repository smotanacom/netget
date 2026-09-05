//! DataLink client implementation for raw Ethernet frame injection
pub mod actions;

pub use actions::DataLinkClientProtocol;

use anyhow::{Context, Result};
use pcap::{Capture, Device};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace};

use crate::client::datalink::actions::{
    DATALINK_CLIENT_CONNECTED_EVENT, DATALINK_CLIENT_FRAME_CAPTURED_EVENT,
    DATALINK_CLIENT_FRAME_INJECTED_EVENT,
};
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::{Event, StartupParams};
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// Connection state for LLM processing
#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Idle,
    Processing,
    Accumulating,
}

/// Per-client data for LLM handling
struct ClientData {
    state: ConnectionState,
    queued_frames: Vec<Vec<u8>>,
    memory: String,
}

/// Channel for sending frame injection commands to the pcap thread.
///
/// libpcap is a blocking API and its handle lives on the blocking task, so nothing else can
/// call `sendpacket`. `ack` is how an injected command learns whether the frame really went
/// out: the pcap loop reports the result back, which is what lets the command loop answer
/// `Sent { bytes_sent }` truthfully instead of guessing.
struct InjectionCommand {
    frame: Vec<u8>,
    ack: Option<tokio::sync::oneshot::Sender<std::result::Result<usize, String>>>,
}

/// How long an injected command waits for the pcap loop's acknowledgement.
const INJECTION_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// DataLink client that injects raw Ethernet frames
pub struct DataLinkClient;

impl DataLinkClient {
    /// Connect to a network interface for frame injection with integrated LLM actions
    pub async fn connect_with_llm_actions(
        _remote_addr: String, // Not used for DataLink (interface instead)
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract interface from startup_params
        let params = startup_params.as_ref().ok_or_else(|| {
            anyhow::anyhow!("DataLink client requires startup parameters (interface)")
        })?;

        let interface = params.get_string("interface")?;
        let promiscuous = params.get_optional_bool("promiscuous")?.unwrap_or(false);

        info!(
            "DataLink client {} opening interface: {} (promiscuous: {})",
            client_id, interface, promiscuous
        );

        // Create channel for frame injection commands
        let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<InjectionCommand>();
        let inject_tx_cmd = inject_tx.clone();
        let inject_tx_arc = Arc::new(Mutex::new(inject_tx));

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] DataLink client {} connected to interface {}",
            client_id, interface
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Initialize client data for capture handling
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_frames: Vec::new(),
            memory: String::new(),
        }));

        // Command channel for injected actions (the dashboard's [ send ]).
        // This client makes no connected-event LLM call, so there is nothing to race here -
        // but it is still registered before the pcap task starts, so [ send ] is live from
        // the moment the client exists.
        let protocol = Arc::new(crate::client::datalink::actions::DataLinkClientProtocol::new());
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            protocol,
            inject_tx_cmd,
            client_id,
            app_state.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Clone for the blocking task
        let inject_tx_arc_thread = inject_tx_arc.clone();
        // ...and for the connected-event task below, which is spawned after the blocking
        // closure has taken ownership of `inject_tx_arc`.
        let inject_tx_conn = inject_tx_arc.clone();
        let interface_clone = interface.clone();
        let status_tx_clone = status_tx.clone();
        let app_state_clone = app_state.clone();
        let llm_client_clone = llm_client.clone();
        let client_data_clone = client_data.clone();

        // Spawn blocking task for pcap operations
        tokio::task::spawn_blocking(move || {
            // Find device
            let device = match Self::find_device(&interface_clone) {
                Ok(d) => d,
                Err(e) => {
                    error!("DataLink client {} failed to find device: {}", client_id, e);
                    let _ = status_tx_clone.send(format!(
                        "[ERROR] DataLink client {} failed to find device: {}",
                        client_id, e
                    ));
                    return;
                }
            };

            // Open capture
            let mut cap = match Capture::from_device(device)
                .map(|c| c.promisc(promiscuous).snaplen(65535).timeout(100))
                .and_then(|c| c.open())
            {
                Ok(c) => c,
                Err(e) => {
                    error!(
                        "DataLink client {} failed to open capture: {}",
                        client_id, e
                    );
                    let _ = status_tx_clone.send(format!(
                        "[ERROR] DataLink client {} failed to open capture: {}",
                        client_id, e
                    ));
                    return;
                }
            };

            info!("DataLink client {} capture opened successfully", client_id);
            let _ = status_tx_clone.send(format!(
                "[INFO] DataLink client {} ready for frame injection",
                client_id
            ));

            let runtime = tokio::runtime::Handle::current();

            // Main loop: handle injection commands and optionally capture frames
            loop {
                // Check for injection commands (non-blocking)
                if let Ok(cmd) = inject_rx.try_recv() {
                    let result = match cap.sendpacket(&cmd.frame[..]) {
                        Ok(_) => {
                            trace!(
                                "DataLink client {} injected frame ({} bytes)",
                                client_id,
                                cmd.frame.len()
                            );
                            let _ = status_tx_clone.send(format!(
                                "[TRACE] DataLink client {} injected frame ({} bytes)",
                                client_id,
                                cmd.frame.len()
                            ));

                            // Tell the model the frame went out. `datalink_frame_injected`
                            // was declared in get_event_types() and raised nowhere, so a
                            // model that injected a frame was never told it had worked and
                            // could not follow it with anything. Spawned onto the runtime
                            // rather than awaited: this is the blocking pcap thread, and
                            // the LLM call can park for minutes on a manual routing rule.
                            let ev_state = app_state_clone.clone();
                            let ev_llm = llm_client_clone.clone();
                            let ev_status = status_tx_clone.clone();
                            let ev_inject = inject_tx_arc_thread.clone();
                            let ev_frame = cmd.frame.clone();
                            runtime.spawn(async move {
                                Self::report_injection(
                                    client_id, ev_frame, ev_state, ev_llm, ev_status, ev_inject,
                                )
                                .await;
                            });

                            Ok(cmd.frame.len())
                        }
                        Err(e) => {
                            error!(
                                "DataLink client {} frame injection failed: {}",
                                client_id, e
                            );
                            let _ = status_tx_clone.send(format!(
                                "[ERROR] DataLink client {} frame injection failed: {}",
                                client_id, e
                            ));
                            Err(e.to_string())
                        }
                    };
                    if let Some(ack) = cmd.ack {
                        let _ = ack.send(result);
                    }
                }

                // If promiscuous mode, capture frames
                if promiscuous {
                    match cap.next_packet() {
                        Ok(packet) => {
                            let frame = packet.data.to_vec();
                            trace!(
                                "DataLink client {} captured frame ({} bytes)",
                                client_id,
                                frame.len()
                            );

                            // Handle frame with LLM
                            let state_clone = app_state_clone.clone();
                            let llm_clone = llm_client_clone.clone();
                            let status_clone = status_tx_clone.clone();
                            let client_data_task = client_data_clone.clone();
                            let inject_tx_task = inject_tx_arc.clone();

                            runtime.spawn(async move {
                                let mut client_data_lock = client_data_task.lock().await;

                                match client_data_lock.state {
                                    ConnectionState::Idle => {
                                        // Process immediately
                                        client_data_lock.state = ConnectionState::Processing;
                                        drop(client_data_lock);

                                        // Call LLM
                                        if let Some(instruction) = state_clone.get_instruction_for_client(client_id).await {
                                            let protocol = Arc::new(crate::client::datalink::actions::DataLinkClientProtocol::new());
                                            let event = Event::new(
                                                &DATALINK_CLIENT_FRAME_CAPTURED_EVENT,
                                                serde_json::json!({
                                                    "frame_hex": hex::encode(&frame),
                                                    "frame_length": frame.len(),
                                                }),
                                            );

                                            match call_llm_for_client(
                                                &llm_clone,
                                                &state_clone,
                                                client_id.to_string(),
                                                &instruction,
                                                &client_data_task.lock().await.memory,
                                                Some(&event),
                                                protocol.as_ref(),
                                                &status_clone,
                                            ).await {
                                                Ok(ClientLlmResult { actions, memory_updates }) => {
                                                    // Update memory
                                                    if let Some(mem) = memory_updates {
                                                        client_data_task.lock().await.memory = mem;
                                                    }

                                                    // Execute actions
                                                    for action in actions {
                                                        use crate::llm::actions::client_trait::Client;
                                                        match protocol.as_ref().execute_action(action) {
                                                            Ok(crate::llm::actions::client_trait::ClientActionResult::SendData(frame_bytes)) => {
                                                                // Send frame injection command
                                                                let _ = inject_tx_task.lock().await.send(InjectionCommand { frame: frame_bytes, ack: None });
                                                            }
                                                            Ok(crate::llm::actions::client_trait::ClientActionResult::Disconnect) => {
                                                                info!("DataLink client {} disconnecting", client_id);
                                                                // Exit will be handled by loop break
                                                            }
                                                            _ => {}
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    error!("LLM error for DataLink client {}: {}", client_id, e);
                                                }
                                            }
                                        }

                                        // Process queued frames if any
                                        let mut client_data_lock = client_data_task.lock().await;
                                        if !client_data_lock.queued_frames.is_empty() {
                                            client_data_lock.queued_frames.clear();
                                        }
                                        client_data_lock.state = ConnectionState::Idle;
                                    }
                                    ConnectionState::Processing => {
                                        // Queue frame
                                        client_data_lock.queued_frames.push(frame);
                                        client_data_lock.state = ConnectionState::Accumulating;
                                    }
                                    ConnectionState::Accumulating => {
                                        // Continue queuing
                                        client_data_lock.queued_frames.push(frame);
                                    }
                                }
                            });
                        }
                        Err(pcap::Error::TimeoutExpired) => {
                            // Normal timeout, continue
                        }
                        Err(e) => {
                            error!("DataLink client {} capture error: {}", client_id, e);
                            break;
                        }
                    }
                }

                // Small sleep to avoid busy loop when not capturing
                if !promiscuous {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }

            info!("DataLink client {} disconnected", client_id);
            runtime.block_on(async {
                app_state_clone
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                // The pcap handle is gone: drop the command handle so the dashboard stops
                // offering [ send ] on a client that can no longer inject anything. This
                // also closes the command channel, which ends `command_loop`.
                app_state_clone.remove_client_handle(client_id).await;
                let _ = status_tx_clone.send(format!(
                    "[CLIENT] DataLink client {} disconnected",
                    client_id
                ));
                let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
            });
        });

        // For DataLink, we return a dummy socket address since we're not using TCP/UDP
        // Ask the model what to do now that the interface is open.
        //
        // This client used to make no connected-event LLM call at all, so a client created
        // with "inject an ARP request for 10.0.0.2" opened the capture and then sat there:
        // nothing consulted the model, so nothing was ever injected. Run from a registered
        // task rather than inline, because a dashboard-created client defaults to a
        // `*` -> manual rule and that call can park for minutes -- `connect` must return.
        let conn_state = app_state.clone();
        let conn_llm = llm_client.clone();
        let conn_status = status_tx.clone();
        let conn_inject = inject_tx_conn;
        let conn_interface = interface.clone();
        let conn_task = tokio::spawn(async move {
            let Some(instruction) = conn_state.get_instruction_for_client(client_id).await else {
                return;
            };
            let event = Event::new(
                &DATALINK_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "interface": conn_interface,
                    "promiscuous": promiscuous,
                }),
            );
            Self::run_llm_turn(
                client_id,
                &instruction,
                event,
                &conn_state,
                &conn_llm,
                &conn_status,
                &conn_inject,
            )
            .await;
        });
        app_state.register_client_task(client_id, conn_task).await;

        // The interface name is stored in the client metadata
        Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
    }

    /// Report a frame that really went out, and queue whatever the model answers with.
    async fn report_injection(
        client_id: ClientId,
        frame: Vec<u8>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
        inject_tx: Arc<Mutex<mpsc::UnboundedSender<InjectionCommand>>>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let event = Event::new(
            &DATALINK_CLIENT_FRAME_INJECTED_EVENT,
            serde_json::json!({
                "frame_length": frame.len(),
                "frame_hex": hex::encode(&frame),
            }),
        );
        Self::run_llm_turn(
            client_id,
            &instruction,
            event,
            &app_state,
            &llm_client,
            &status_tx,
            &inject_tx,
        )
        .await;
    }

    /// One LLM turn: raise `event`, then queue any frames the model asks to inject.
    ///
    /// Deliberately does NOT recurse. Injecting a frame raises `datalink_frame_injected`
    /// from the pcap loop, which comes back here, so the chain continues by itself through
    /// the queue rather than through the stack -- and a model that answers every injection
    /// with another injection is bounded by nothing else, which is why `run_llm_turn`
    /// executes exactly one round and returns.
    async fn run_llm_turn(
        client_id: ClientId,
        instruction: &str,
        event: Event,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
        inject_tx: &Arc<Mutex<mpsc::UnboundedSender<InjectionCommand>>>,
    ) {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};

        let protocol = crate::client::datalink::actions::DataLinkClientProtocol::new();
        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        match call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            instruction,
            &memory,
            Some(&event),
            &protocol,
            status_tx,
        )
        .await
        {
            Ok(ClientLlmResult {
                actions,
                memory_updates,
            }) => {
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }
                for action in actions {
                    match protocol.execute_action(action) {
                        Ok(ClientActionResult::SendData(frame)) => {
                            let _ = inject_tx
                                .lock()
                                .await
                                .send(InjectionCommand { frame, ack: None });
                        }
                        Ok(ClientActionResult::Disconnect) => {
                            info!("DataLink client {} disconnecting", client_id);
                            app_state.remove_client_handle(client_id).await;
                        }
                        Ok(_) => {}
                        Err(e) => error!("DataLink client {} rejected action: {}", client_id, e),
                    }
                }
            }
            Err(e) => {
                // Nothing is written to the interface on failure. DataLink is one of the
                // deliberately silent protocols: a fabricated frame on a real network is
                // worse than no frame at all.
                error!(
                    "DataLink client {} LLM error on {}: {}",
                    client_id, event.event_type.id, e
                );
            }
        }
    }

    /// Drain injected commands until the channel closes (client removed, or the pcap loop
    /// exited and dropped the handle) or an injected `disconnect` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot serve this client:
    /// there is no `AsyncWrite` half at all. `inject_frame` yields
    /// `ClientActionResult::SendData`, which is handed to the same pcap injection queue the
    /// LLM path uses; the pcap loop acknowledges the `sendpacket` call so the reply can be
    /// truthful.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        protocol: Arc<crate::client::datalink::actions::DataLinkClientProtocol>,
        inject_tx: mpsc::UnboundedSender<InjectionCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = Self::execute_injected_action(&protocol, &inject_tx, &action).await;

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
                error!(
                    "DataLink client {} injected action failed: {}",
                    client_id, e
                );
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                app_state.remove_client_handle(client_id).await;
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                break;
            }
        }
    }

    /// Execute one injected action and report exactly what happened to it.
    ///
    /// - `Sent { bytes_sent }` only after the pcap loop has acknowledged a successful
    ///   `sendpacket` for that many bytes.
    /// - `Executed { detail }` when the frame could not be handed over at all - which is
    ///   what an unprivileged run looks like, because the capture never opened.
    /// - `Rejected { error }` for an action the protocol refuses (unknown type, bad hex).
    /// - `Err` when pcap accepted the frame and failed to transmit it.
    async fn execute_injected_action(
        protocol: &Arc<crate::client::datalink::actions::DataLinkClientProtocol>,
        inject_tx: &mpsc::UnboundedSender<InjectionCommand>,
        action: &serde_json::Value,
    ) -> Result<ClientSendOutcome> {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};

        let result = match protocol.as_ref().execute_action(action.clone()) {
            Ok(result) => result,
            Err(e) => {
                return Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                })
            }
        };

        let frame = match result {
            ClientActionResult::SendData(frame) => frame,
            ClientActionResult::Disconnect => return Ok(ClientSendOutcome::Disconnected),
            ClientActionResult::WaitForMore => {
                return Ok(ClientSendOutcome::Executed {
                    detail: "wait_for_more".to_string(),
                })
            }
            other => {
                return Ok(ClientSendOutcome::Executed {
                    detail: format!("{other:?} injects no frame"),
                })
            }
        };

        let frame_len = frame.len();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        if inject_tx
            .send(InjectionCommand {
                frame,
                ack: Some(ack_tx),
            })
            .is_err()
        {
            // The blocking task returned before opening a capture (no such device, or -
            // the usual case off-root - libpcap could not open the interface), so the
            // injection queue's receiver is gone.
            return Ok(ClientSendOutcome::Executed {
                detail: format!(
                    "{frame_len}-byte frame built but not injected: the pcap capture is not \
                     open (raw frame injection needs root / BPF access)"
                ),
            });
        }

        match tokio::time::timeout(INJECTION_ACK_TIMEOUT, ack_rx).await {
            Ok(Ok(Ok(bytes_sent))) => Ok(ClientSendOutcome::Sent { bytes_sent }),
            Ok(Ok(Err(e))) => Err(anyhow::anyhow!("pcap sendpacket failed: {e}")),
            Ok(Err(_)) => Ok(ClientSendOutcome::Executed {
                detail: format!(
                    "{frame_len}-byte frame not injected: it was handed to the pcap loop, \
                     which exited without reporting the result"
                ),
            }),
            Err(_) => Ok(ClientSendOutcome::Executed {
                detail: format!(
                    "{frame_len}-byte frame not confirmed injected: the pcap loop did not \
                     acknowledge it within {}s",
                    INJECTION_ACK_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    /// Find a network device by name
    fn find_device(name: &str) -> Result<Device> {
        let devices = Device::list().context("Failed to list network devices")?;
        devices
            .into_iter()
            .find(|d| d.name == name)
            .ok_or_else(|| anyhow::anyhow!("Network device '{}' not found", name))
    }
}
