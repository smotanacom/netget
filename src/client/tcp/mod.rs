//! TCP client implementation
pub mod actions;

pub use actions::TcpClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::tcp::actions::{TCP_CLIENT_CONNECTED_EVENT, TCP_CLIENT_DATA_RECEIVED_EVENT};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::patterns;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// Per-client data for LLM handling.
///
/// There is no Idle/Processing/Accumulating state machine here, and there was never a working
/// one. The server's state machine (`src/server/tcp/mod.rs`) exists because its reader task
/// hands each payload to a *separate* task, so a second read really can arrive mid-call. This
/// client reads and calls the model on the same task: `read_half.read()` is not called again
/// until the LLM round-trip returns, so `Processing` was unreachable, `Accumulating` was
/// unreachable, and the `queued_data` those two branches filled was cleared without ever being
/// looked at. Copying the server's shape here produced dead code that read as backpressure.
///
/// Data arriving during a call waits in the kernel's socket buffer, which is the same discipline
/// `reverse_shell` documents.
struct ClientData {
    memory: String,
}

/// TCP client that connects to a remote TCP server
pub struct TcpClient;

impl TcpClient {
    /// Connect to a TCP server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Resolve and connect
        let stream = TcpStream::connect(&remote_addr)
            .await
            .context(format!("Failed to connect to {}", remote_addr))?;

        let local_addr = stream.local_addr()?;
        let remote_sock_addr = stream.peer_addr()?;

        info!(
            "TCP client {} connected to {} (local: {})",
            client_id, remote_sock_addr, local_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] TCP client {} connected", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream
        let (mut read_half, write_half) = tokio::io::split(stream);
        let write_half_arc = Arc::new(Mutex::new(write_half));
        let write_half_for_connected = write_half_arc.clone();

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            memory: String::new(),
        }));

        // Command channel: lets the dashboard (and any programmatic caller)
        // inject actions into this loop via AppState::send_to_client.
        //
        // Registered BEFORE the connected event is handled: a `manual` routing
        // rule can park that event at the dashboard for minutes, and until
        // registration the UI reports "no command channel" — reading as a
        // protocol limitation when it is only a queue. Registered here, a send
        // during the park waits in the channel instead of being refused.
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        // Call LLM with tcp_connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &TCP_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "remote_addr": remote_sock_addr.to_string(),
                }),
            );

            // The memory is copied out and the guard dropped *before* the call. Passing
            // `&client_data.lock().await.memory` straight into the call holds the guard across
            // the whole LLM round-trip — the rule the top-level CLAUDE.md states outright — and
            // worse, a temporary in a `match` scrutinee lives until the end of the whole
            // `match`, so it was still held inside the arms. The Ok arm re-locks the same
            // non-reentrant tokio Mutex to store `memory_updates`.
            //
            // That is latent rather than live today only because `call_llm_for_client` hardcodes
            // `memory_updates = None` (`src/llm/action_helper.rs`), so the re-lock is never
            // reached. The day anyone extracts memory from a client response, this task hangs
            // forever holding the socket while still reporting Connected. The DC client has the
            // same shape and *is* live, because its Ok arm re-locks on every action.
            let memory = client_data.lock().await.memory.clone();

            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &crate::client::tcp::actions::TcpClientProtocol,
                &status_tx,
            )
            .await
            {
                Ok(result) => {
                    // Update memory if provided
                    if let Some(new_memory) = result.memory_updates {
                        client_data.lock().await.memory = new_memory;
                    }

                    // Execute actions from LLM response using the proper execute_action method
                    use crate::llm::actions::client_trait::Client;
                    let protocol = crate::client::tcp::actions::TcpClientProtocol::new();
                    for action in result.actions {
                        match protocol.execute_action(action) {
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::SendData(
                                    bytes,
                                ),
                            ) => {
                                let mut write_guard = write_half_for_connected.lock().await;
                                if let Err(e) = write_guard.write_all(&bytes).await {
                                    error!("Failed to send data after connect: {}", e);
                                } else if let Err(e) = write_guard.flush().await {
                                    error!("Failed to flush after connect: {}", e);
                                } else {
                                    info!("Sent {} {}", bytes.len(), patterns::TCP_CLIENT_SENT);
                                }
                            }
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::Disconnect,
                            ) => {
                                info!("LLM requested disconnect after connect");
                                return Ok(local_addr);
                            }
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::WaitForMore,
                            ) => {
                                // Just wait for data
                            }
                            Ok(_) => {
                                // Other action results
                            }
                            Err(e) => {
                                error!("Failed to execute action after connect: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error on tcp_connected event: {}", e);
                }
            }
        }

        // Spawn read loop. The handle is registered with AppState so that
        // stop_client / remove_client can abort it — dropping a JoinHandle only
        // detaches the task, leaving the socket open and the LLM being called.
        let task_registrar = app_state.clone();
        let handle = tokio::spawn(async move {
            info!("TCP client {} read loop started", client_id);
            let mut buffer = vec![0u8; 8192];

            'read_loop: loop {
                let read_result = tokio::select! {
                    read = read_half.read(&mut buffer) => read,
                    Some(cmd) = command_rx.recv() => {
                        let disconnect = crate::client::command_support::handle_stream_client_command(
                            &crate::client::tcp::actions::TcpClientProtocol,
                            &write_half_arc,
                            cmd,
                            client_id,
                            &app_state,
                            &status_tx,
                        )
                        .await;
                        if disconnect {
                            app_state
                                .update_client_status(client_id, ClientStatus::Disconnected)
                                .await;
                            let _ = status_tx.send(format!(
                                "[CLIENT] TCP client {} disconnected (injected action)",
                                client_id
                            ));
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            break;
                        }
                        continue;
                    }
                };
                match read_result {
                    Ok(0) => {
                        info!(
                            "TCP client {} {}",
                            client_id,
                            patterns::TCP_CLIENT_DISCONNECTED
                        );
                        app_state
                            .update_client_status(client_id, ClientStatus::Disconnected)
                            .await;
                        let _ = status_tx
                            .send(format!("[CLIENT] TCP client {} disconnected", client_id));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                    Ok(n) => {
                        let data = buffer[..n].to_vec();
                        info!(
                            "TCP client {} received {} {}",
                            client_id,
                            n,
                            patterns::TCP_CLIENT_RECEIVED
                        );
                        trace!("TCP client {} received {} bytes", client_id, n);

                        // Handle data with the LLM. This is sequential by construction: the next
                        // `read_half.read()` does not happen until this returns, and anything
                        // the server sends meanwhile waits in the socket buffer.
                        if let Some(instruction) =
                            app_state.get_instruction_for_client(client_id).await
                        {
                            let protocol =
                                Arc::new(crate::client::tcp::actions::TcpClientProtocol::new());
                            let event = Event::new(
                                &TCP_CLIENT_DATA_RECEIVED_EVENT,
                                serde_json::json!({
                                    "data_hex": hex::encode(&data),
                                    "data_length": data.len(),
                                }),
                            );

                            // Copied out, guard dropped: see the note at the connected-event
                            // call above. Holding it across the call deadlocked on the memory
                            // write in the Ok arm.
                            let memory = client_data.lock().await.memory.clone();

                            match call_llm_for_client(
                                &llm_client,
                                &app_state,
                                client_id.to_string(),
                                &instruction,
                                &memory,
                                Some(&event),
                                protocol.as_ref(),
                                &status_tx,
                            )
                            .await
                            {
                                Ok(ClientLlmResult {
                                    actions,
                                    memory_updates,
                                }) => {
                                    // Update memory
                                    if let Some(mem) = memory_updates {
                                        client_data.lock().await.memory = mem;
                                    }

                                    // Execute actions
                                    for action in actions {
                                        use crate::llm::actions::client_trait::Client;
                                        match protocol.as_ref().execute_action(action) {
                                            Ok(crate::llm::actions::client_trait::ClientActionResult::SendData(bytes)) => {
                                                let mut write_guard = write_half_arc.lock().await;
                                                // A failed write used to be discarded by
                                                // `.is_ok()`, so a send that never reached the
                                                // server looked identical to one that did.
                                                if let Err(e) = write_guard.write_all(&bytes).await {
                                                    error!("TCP client {} write failed: {}", client_id, e);
                                                } else if let Err(e) = write_guard.flush().await {
                                                    error!("TCP client {} flush failed: {}", client_id, e);
                                                } else {
                                                    trace!("TCP client {} sent {} bytes", client_id, bytes.len());
                                                }
                                            }
                                            Ok(crate::llm::actions::client_trait::ClientActionResult::Disconnect) => {
                                                info!("TCP client {} disconnecting", client_id);
                                                app_state
                                                    .update_client_status(client_id, ClientStatus::Disconnected)
                                                    .await;
                                                let _ = status_tx.send(format!(
                                                    "[CLIENT] TCP client {} disconnected",
                                                    client_id
                                                ));
                                                let _ = status_tx.send("__UPDATE_UI__".to_string());
                                                break 'read_loop;
                                            }
                                            Ok(_) => {}
                                            Err(e) => {
                                                // Keep going: one unusable action must not
                                                // discard the rest of the model's answer.
                                                error!(
                                                    "TCP client {} could not execute an action: {}",
                                                    client_id, e
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("LLM error for TCP client {}: {}", client_id, e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("TCP client {} read error: {}", client_id, e);
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                }
            }
            // The loop owns the only receiver; dropping the registered handle
            // makes later send_to_client calls fail fast instead of timing out.
            app_state.remove_client_handle(client_id).await;
        });
        task_registrar.register_client_task(client_id, handle).await;

        Ok(local_addr)
    }
}
