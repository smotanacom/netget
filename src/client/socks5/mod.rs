//! SOCKS5 client implementation
pub mod actions;

pub use actions::Socks5ClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use tokio_socks::tcp::Socks5Stream;
use tracing::{error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::socks5::actions::{
    SOCKS5_CLIENT_CONNECTED_EVENT, SOCKS5_CLIENT_DATA_RECEIVED_EVENT,
};
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

/// SOCKS5 client that connects to a target server through a SOCKS5 proxy
pub struct Socks5Client;

impl Socks5Client {
    /// Connect to a target server through a SOCKS5 proxy with integrated LLM actions
    pub async fn connect_with_llm_actions(
        proxy_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract target address from startup params
        let target_addr = startup_params
            .as_ref()
            .map(|p| p.get_string("target_addr"))
            .transpose()?
            .context("Missing required startup parameter 'target_addr'")?;

        // Extract optional authentication
        let auth_username = startup_params
            .as_ref()
            .map(|p| p.get_optional_string("auth_username"))
            .transpose()?
            .flatten();

        let auth_password = startup_params
            .as_ref()
            .map(|p| p.get_optional_string("auth_password"))
            .transpose()?
            .flatten();

        info!(
            "SOCKS5 client {} connecting to {} through proxy {}",
            client_id, target_addr, proxy_addr
        );

        // Connect through SOCKS5 proxy
        let stream = if let (Some(username), Some(password)) = (auth_username, auth_password) {
            // Connect with authentication
            Socks5Stream::connect_with_password(
                proxy_addr.as_str(),
                target_addr.as_str(),
                username.as_str(),
                password.as_str(),
            )
            .await
            .context(format!(
                "Failed to connect to {} through SOCKS5 proxy {} with auth",
                target_addr, proxy_addr
            ))?
        } else {
            // Connect without authentication
            Socks5Stream::connect(proxy_addr.as_str(), target_addr.as_str())
                .await
                .context(format!(
                    "Failed to connect to {} through SOCKS5 proxy {}",
                    target_addr, proxy_addr
                ))?
        };

        // Get addresses before consuming the stream
        let tcp_stream = stream.into_inner();
        let local_addr = tcp_stream.local_addr()?;
        let proxy_sock_addr = tcp_stream.peer_addr()?;

        info!(
            "SOCKS5 client {} connected through proxy {} to target {} (local: {})",
            client_id, proxy_sock_addr, target_addr, local_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] SOCKS5 client {} connected to {} through proxy",
            client_id, target_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream (tcp_stream was already extracted above)
        let (mut read_half, write_half) = tokio::io::split(tcp_stream);
        let write_half_arc = Arc::new(Mutex::new(write_half));

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_data: Vec::new(),
            memory: String::new(),
        }));

        // Call LLM with connected event
        let protocol = Arc::new(crate::client::socks5::actions::Socks5ClientProtocol::new());
        let connected_event = Event::new(
            &SOCKS5_CLIENT_CONNECTED_EVENT,
            serde_json::json!({
                "proxy_addr": proxy_addr,
                "target_addr": target_addr,
            }),
        );

        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            // Copy the memory out under its own guard. Passing
            // `&client_data.lock().await.memory` straight into the call would keep the guard
            // alive for the whole `match` -- temporaries in a match scrutinee live until the
            // match ends -- and the `Ok` arm below locks the same mutex again to store the
            // model's memory update. `tokio::sync::Mutex` is not reentrant, so that is a
            // permanent deadlock, reached only when the model happens to return memory.
            let memory = { client_data.lock().await.memory.clone() };
            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&connected_event),
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

                    // Execute initial actions
                    for action in actions {
                        use crate::llm::actions::client_trait::Client;
                        match protocol.as_ref().execute_action(action) {
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::SendData(
                                    bytes,
                                ),
                            ) => {
                                // The `Err` half used to be discarded outright, so a failed
                                // write to the tunnel produced no log line at any level, no
                                // status message and no state change -- the client reported
                                // success having put nothing on the wire. The protocol's own
                                // CLAUDE.md claimed these were "logged but not fatal"; only
                                // the second half was true.
                                match write_half_arc.lock().await.write_all(&bytes).await {
                                    Ok(()) => trace!(
                                        "SOCKS5 client {} sent {} bytes through tunnel",
                                        client_id,
                                        bytes.len()
                                    ),
                                    Err(e) => error!(
                                        "SOCKS5 client {} failed to write {} bytes to the tunnel: {}",
                                        client_id,
                                        bytes.len(),
                                        e
                                    ),
                                }
                            }
                            Ok(
                                crate::llm::actions::client_trait::ClientActionResult::Disconnect,
                            ) => {
                                warn!("SOCKS5 client {} requested disconnect immediately after connect", client_id);
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "LLM error for SOCKS5 client {} on connect: {}",
                        client_id, e
                    );
                }
            }
        }

        // Spawn read loop
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        // Command channel: lets the dashboard inject actions into this loop
        // via AppState::send_to_client (see client/command_support.rs).
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        let task_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 8192];

            loop {
                let read_result = tokio::select! {
                    read = read_half.read(&mut buffer) => read,
                    Some(cmd) = command_rx.recv() => {
                        let disconnect = crate::client::command_support::handle_stream_client_command(
                            &crate::client::socks5::actions::Socks5ClientProtocol,
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
                                "[CLIENT] SOCKS5 client {} disconnected (injected action)",
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
                        info!("SOCKS5 client {} disconnected", client_id);
                        app_state
                            .update_client_status(client_id, ClientStatus::Disconnected)
                            .await;
                        let _ = status_tx
                            .send(format!("[CLIENT] SOCKS5 client {} disconnected", client_id));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                    Ok(n) => {
                        let data = buffer[..n].to_vec();
                        trace!(
                            "SOCKS5 client {} received {} bytes from target",
                            client_id,
                            n
                        );

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
                                    let protocol = Arc::new(
                                        crate::client::socks5::actions::Socks5ClientProtocol::new(),
                                    );
                                    let event = Event::new(
                                        &SOCKS5_CLIENT_DATA_RECEIVED_EVENT,
                                        serde_json::json!({
                                            "data_hex": hex::encode(&data),
                                            "data_length": data.len(),
                                        }),
                                    );

                                    // Same reasoning as the connected-event call above: the
                                    // guard must not survive into the match arms.
                                    let memory = { client_data.lock().await.memory.clone() };
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
                                                        match write_half_arc.lock().await.write_all(&bytes).await {
                                                            Ok(()) => trace!("SOCKS5 client {} sent {} bytes", client_id, bytes.len()),
                                                            Err(e) => error!("SOCKS5 client {} failed to write {} bytes to the tunnel: {}", client_id, bytes.len(), e),
                                                        }
                                                    }
                                                    Ok(crate::llm::actions::client_trait::ClientActionResult::Disconnect) => {
                                                        info!("SOCKS5 client {} disconnecting", client_id);
                                                        break;
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "LLM error for SOCKS5 client {}: {}",
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
                        error!("SOCKS5 client {} read error: {}", client_id, e);
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

        Ok(local_addr)
    }
}
