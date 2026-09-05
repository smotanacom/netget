//! Socket File client implementation
pub mod actions;

pub use actions::SocketFileClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::socket_file::actions::SOCKET_FILE_CLIENT_DATA_RECEIVED_EVENT;
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
    queued_data: Vec<u8>,
    memory: String,
}

/// Socket File client that connects to a Unix domain socket
pub struct SocketFileClient;

impl SocketFileClient {
    /// Connect to a Unix domain socket with integrated LLM actions
    pub async fn connect_with_llm_actions(
        socket_path: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Connect to Unix domain socket
        let stream = UnixStream::connect(&socket_path)
            .await
            .context(format!("Failed to connect to Unix socket {}", socket_path))?;

        // Unix sockets don't have traditional socket addresses, so we create a dummy one
        // The socket path is stored in the client's remote_addr field in app_state
        let dummy_addr = SocketAddr::from(([127, 0, 0, 1], 0));

        info!(
            "Socket File client {} connected to {}",
            client_id, socket_path
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] Socket File client {} connected to {}",
            client_id, socket_path
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream
        let (mut read_half, write_half) = stream.into_split();
        let write_half_arc = Arc::new(Mutex::new(write_half));

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_data: Vec::new(),
            memory: String::new(),
        }));

        // Spawn read loop
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        // Command channel: lets the dashboard inject actions into this loop
        // via AppState::send_to_client (see client/command_support.rs).
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        // Raise the connected event.
        //
        // SOCKET_FILE_CLIENT_CONNECTED_EVENT was declared and nothing raised it, so the
        // model was only ever consulted once the peer had already said something. A client
        // meant to speak first -- which this protocol's own example action
        // (`send_socket_file_data`) implies -- could never do so.
        //
        // From a registered task, not inline: a dashboard-created client defaults to a
        // `*` -> manual rule and awaiting a parked answer would block creation.
        let conn_state = app_state.clone();
        let conn_llm = llm_client.clone();
        let conn_status = status_tx.clone();
        let conn_write = write_half_arc.clone();
        let conn_path = socket_path.clone();
        let conn_task = tokio::spawn(async move {
            let Some(instruction) = conn_state.get_instruction_for_client(client_id).await else {
                return;
            };
            let protocol = crate::client::socket_file::actions::SocketFileClientProtocol::new();
            let event = Event::new(
                &crate::client::socket_file::actions::SOCKET_FILE_CLIENT_CONNECTED_EVENT,
                serde_json::json!({ "socket_path": conn_path }),
            );
            match call_llm_for_client(
                &conn_llm,
                &conn_state,
                client_id.to_string(),
                &instruction,
                "",
                Some(&event),
                &protocol,
                &conn_status,
            )
            .await
            {
                Ok(result) => {
                    if let Some(mem) = result.memory_updates {
                        conn_state.set_memory_for_client(client_id, mem).await;
                    }
                    use crate::llm::actions::client_trait::{Client, ClientActionResult};
                    for action in result.actions {
                        if let Ok(ClientActionResult::SendData(bytes)) =
                            protocol.execute_action(action)
                        {
                            let mut guard = conn_write.lock().await;
                            if let Err(e) = guard.write_all(&bytes).await {
                                error!(
                                    "Socket File client {} connect-time write failed: {}",
                                    client_id, e
                                );
                            }
                        }
                    }
                }
                Err(e) => error!(
                    "Socket File client {} LLM error on connected event: {}",
                    client_id, e
                ),
            }
        });
        app_state.register_client_task(client_id, conn_task).await;

        let task_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 8192];

            loop {
                let read_result = tokio::select! {
                    read = read_half.read(&mut buffer) => read,
                    Some(cmd) = command_rx.recv() => {
                        let disconnect = crate::client::command_support::handle_stream_client_command(
                            &crate::client::socket_file::actions::SocketFileClientProtocol,
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
                                "[CLIENT] SocketFile client {} disconnected (injected action)",
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
                        info!("Socket File client {} disconnected", client_id);
                        app_state
                            .update_client_status(client_id, ClientStatus::Disconnected)
                            .await;
                        let _ = status_tx.send(format!(
                            "[CLIENT] Socket File client {} disconnected",
                            client_id
                        ));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                    Ok(n) => {
                        let data = buffer[..n].to_vec();
                        trace!("Socket File client {} received {} bytes", client_id, n);

                        // Handle data with LLM
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
                                    let protocol = Arc::new(crate::client::socket_file::actions::SocketFileClientProtocol::new());
                                    let event = Event::new(
                                        &SOCKET_FILE_CLIENT_DATA_RECEIVED_EVENT,
                                        serde_json::json!({
                                            "data_hex": hex::encode(&data),
                                            "data_length": data.len(),
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
                                                        if let Ok(_) = write_half_arc.lock().await.write_all(&bytes).await {
                                                            trace!("Socket File client {} sent {} bytes", client_id, bytes.len());
                                                        }
                                                    }
                                                    Ok(crate::llm::actions::client_trait::ClientActionResult::Disconnect) => {
                                                        info!("Socket File client {} disconnecting", client_id);
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "LLM error for Socket File client {}: {}",
                                                client_id, e
                                            );
                                        }
                                    }
                                }

                                // Process queued data if any
                                let mut client_data_lock = client_data.lock().await;
                                if !client_data_lock.queued_data.is_empty() {
                                    client_data_lock.queued_data.clear();
                                }
                                client_data_lock.state = ConnectionState::Idle;
                            }
                            ConnectionState::Processing => {
                                // Queue data
                                client_data_lock.queued_data.extend_from_slice(&data);
                                client_data_lock.state = ConnectionState::Accumulating;
                            }
                            ConnectionState::Accumulating => {
                                // Continue queuing
                                client_data_lock.queued_data.extend_from_slice(&data);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Socket File client {} read error: {}", client_id, e);
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
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(dummy_addr)
    }
}
