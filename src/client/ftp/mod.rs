//! FTP client implementation
//!
//! LLM-controlled FTP client that connects to remote FTP servers
//! and allows the LLM to send commands and interpret responses.

pub mod actions;

pub use actions::FtpClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace};

use crate::client::ftp::actions::{FTP_CLIENT_CONNECTED_EVENT, FTP_CLIENT_RESPONSE_EVENT};
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

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
    queued_responses: Vec<String>,
    memory: String,
}

/// FTP client that connects to remote FTP servers
pub struct FtpClient;

impl FtpClient {
    /// Connect to an FTP server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Resolve and connect
        let stream = TcpStream::connect(&remote_addr).await.context(format!(
            "Failed to connect to FTP server at {}",
            remote_addr
        ))?;

        let local_addr = stream.local_addr()?;
        let remote_sock_addr = stream.peer_addr()?;

        info!(
            "FTP client {} connected to {} (local: {})",
            client_id, remote_sock_addr, local_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] FTP client {} connected", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream
        let (read_half, write_half) = tokio::io::split(stream);
        let write_half_arc = Arc::new(Mutex::new(write_half));
        let write_half_for_connected = write_half_arc.clone();

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_responses: Vec::new(),
            memory: String::new(),
        }));

        // Command channel for the dashboard's [ send_ftp_command ] / [ disconnect ] rows.
        // Registered BEFORE the ftp_connected call: a manual handler can park that call for
        // minutes, and an injected send must work for the whole park. The read loop below
        // uses `read_line`, which is not cancellation-safe, so the channel is drained by its
        // own task sharing the write half rather than by a `select!` arm.
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_state = app_state.clone();
        let cmd_tx = status_tx.clone();
        let cmd_write = write_half_arc.clone();
        let cmd_task = tokio::spawn(async move {
            while let Some(cmd) = command_rx.recv().await {
                let disconnect = crate::client::command_support::handle_stream_client_command(
                    &crate::client::ftp::actions::FtpClientProtocol,
                    &cmd_write,
                    cmd,
                    client_id,
                    &cmd_state,
                    &cmd_tx,
                )
                .await;
                if disconnect {
                    // Half-close: the server reads EOF and closes; the read loop then sees
                    // 0 and runs its normal disconnect path.
                    let _ = cmd_write.lock().await.shutdown().await;
                    break;
                }
            }
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Call LLM with ftp_connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &FTP_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "remote_addr": remote_sock_addr.to_string(),
                }),
            );

            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &client_data.lock().await.memory,
                Some(&event),
                &crate::client::ftp::actions::FtpClientProtocol,
                &status_tx,
            )
            .await
            {
                Ok(result) => {
                    // Update memory if provided
                    if let Some(new_memory) = result.memory_updates {
                        client_data.lock().await.memory = new_memory;
                    }

                    // Execute actions from LLM response
                    use crate::llm::actions::client_trait::Client;
                    let protocol = crate::client::ftp::actions::FtpClientProtocol::new();
                    for action in result.actions {
                        match protocol.execute_action(action) {
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::SendData(
                                    bytes,
                                ),
                            ) => {
                                let mut write_guard = write_half_for_connected.lock().await;
                                if let Err(e) = write_guard.write_all(&bytes).await {
                                    error!("Failed to send FTP command after connect: {}", e);
                                } else if let Err(e) = write_guard.flush().await {
                                    error!("Failed to flush after connect: {}", e);
                                } else {
                                    let cmd = String::from_utf8_lossy(&bytes);
                                    info!("FTP client sent: {}", cmd.trim());
                                }
                            }
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::Disconnect,
                            ) => {
                                info!("LLM requested disconnect after connect");
                                app_state.remove_client_handle(client_id).await;
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
                                error!("Failed to execute FTP action after connect: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error on ftp_connected event: {}", e);
                }
            }
        }

        // Spawn read loop
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn(async move {
            info!("FTP client {} read loop started", client_id);
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();

            loop {
                line.clear();
                match crate::client::response_reader::read_response_line(&mut reader, &mut line)
                    .await
                {
                    Ok(0) => {
                        info!("FTP client {} disconnected", client_id);
                        app_state
                            .update_client_status(client_id, ClientStatus::Disconnected)
                            .await;
                        let _ = status_tx
                            .send(format!("[CLIENT] FTP client {} disconnected", client_id));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                    Ok(_) => {
                        let response = line.trim().to_string();
                        debug!("FTP client {} received: {}", client_id, response);
                        trace!("FTP client {} received response: {}", client_id, response);

                        // Handle response with LLM
                        let mut client_data_lock = client_data.lock().await;

                        match client_data_lock.state {
                            ConnectionState::Idle => {
                                // Process immediately
                                client_data_lock.state = ConnectionState::Processing;
                                drop(client_data_lock);

                                // Call LLM
                                if let Some(instruction) =
                                    app_state.get_instruction_for_client(client_id).await
                                {
                                    let protocol = Arc::new(
                                        crate::client::ftp::actions::FtpClientProtocol::new(),
                                    );

                                    // Parse response code if present
                                    let response_code = response
                                        .split_whitespace()
                                        .next()
                                        .and_then(|s| s.trim_end_matches('-').parse::<u16>().ok());

                                    let event = Event::new(
                                        &FTP_CLIENT_RESPONSE_EVENT,
                                        serde_json::json!({
                                            "response": response,
                                            "response_code": response_code,
                                        }),
                                    );

                                    match call_llm_for_client(
                                        &llm_client,
                                        &app_state,
                                        client_id.to_string(),
                                        &instruction,
                                        &client_data.lock().await.memory,
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
                                                        if write_guard.write_all(&bytes).await.is_ok() {
                                                            if write_guard.flush().await.is_ok() {
                                                                let cmd = String::from_utf8_lossy(&bytes);
                                                                trace!("FTP client {} sent: {}", client_id, cmd.trim());
                                                            }
                                                        }
                                                    }
                                                    Ok(crate::llm::actions::client_trait::ClientActionResult::Disconnect) => {
                                                        info!("FTP client {} disconnecting", client_id);
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!("LLM error for FTP client {}: {}", client_id, e);
                                        }
                                    }
                                }

                                // Process queued responses if any
                                let mut client_data_lock = client_data.lock().await;
                                if !client_data_lock.queued_responses.is_empty() {
                                    client_data_lock.queued_responses.clear();
                                }
                                client_data_lock.state = ConnectionState::Idle;
                            }
                            ConnectionState::Processing => {
                                // Queue response
                                client_data_lock.queued_responses.push(response);
                                client_data_lock.state = ConnectionState::Accumulating;
                            }
                            ConnectionState::Accumulating => {
                                // Continue queuing
                                client_data_lock.queued_responses.push(response);
                            }
                        }
                    }
                    Err(e) => {
                        error!("FTP client {} read error: {}", client_id, e);
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                }
            }
            // Every exit path lands here: drop the command handle so the dashboard stops
            // offering [ send ] on a dead connection (a late send then fails fast).
            app_state.remove_client_handle(client_id).await;
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(local_addr)
    }
}
