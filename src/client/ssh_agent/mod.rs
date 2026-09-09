//! SSH Agent client implementation
//!
//! Platform: Unix/Linux/macOS (uses Unix domain sockets)
#![cfg(unix)]

pub mod actions;

pub use actions::SshAgentClientProtocol;

use anyhow::{Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::ssh_agent::actions::{
    SSH_AGENT_CLIENT_CONNECTED_EVENT, SSH_AGENT_CLIENT_RESPONSE_RECEIVED_EVENT,
};
use crate::llm::actions::client_trait::ClientActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// SSH Agent message types
const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
const SSH_AGENTC_ADD_IDENTITY: u8 = 17;
const SSH_AGENTC_REMOVE_IDENTITY: u8 = 18;
const SSH_AGENTC_REMOVE_ALL_IDENTITIES: u8 = 19;

const SSH_AGENT_FAILURE: u8 = 5;
const SSH_AGENT_SUCCESS: u8 = 6;
/// Where an SSH Agent client connects when no address is given.
///
/// Matches the `socket_path` default of NetGet's own SSH Agent server, so the pair works out
/// of the box against fabricated keys. It is emphatically not `$SSH_AUTH_SOCK` - see
/// `connect_with_llm_actions`.
const DEFAULT_AGENT_SOCKET: &str = "./netget-ssh-agent.sock";

const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
const SSH_AGENT_SIGN_RESPONSE: u8 = 14;

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

/// SSH Agent client that connects to an SSH agent
pub struct SshAgentClient;

impl SshAgentClient {
    /// Connect to an SSH Agent with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Resolve the socket path.
        //
        // This deliberately does NOT fall back to `$SSH_AUTH_SOCK`. It used to, and that was
        // the single most dangerous default in this protocol: with no address given, an
        // LLM-driven client silently attached to the operator's *live* ssh-agent and could
        // then enumerate their real identities and ask the agent to sign arbitrary bytes with
        // their real private keys. Nothing about the request said that was happening, and the
        // model - not the operator - chose what to sign.
        //
        // The default is now NetGet's own agent socket, matching the `socket_path` default in
        // `src/server/ssh_agent/`, so an empty address pairs the client with a NetGet server
        // whose keys are fabricated. Pointing this at a real agent is still possible and is
        // still a legitimate thing to do - it just has to be asked for by name.
        let socket_path = if remote_addr.is_empty() {
            PathBuf::from(DEFAULT_AGENT_SOCKET)
        } else {
            PathBuf::from(&remote_addr)
        };

        info!(
            "SSH Agent client {} connecting to {:?}",
            client_id, socket_path
        );

        // Connect to Unix socket
        let stream = UnixStream::connect(&socket_path).await.context(format!(
            "Failed to connect to SSH Agent at {:?}",
            socket_path
        ))?;

        info!("SSH Agent client {} connected", client_id);

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] SSH Agent client {} connected to {:?}",
            client_id, socket_path
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream
        let (mut read_half, write_half) = tokio::io::split(stream);
        let write_half_arc = Arc::new(Mutex::new(write_half));

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_data: Vec::new(),
            memory: String::new(),
        }));

        // Call LLM with connected event
        let protocol = Arc::new(SshAgentClientProtocol::new());
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &SSH_AGENT_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "socket_path": socket_path.to_string_lossy(),
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

                    // Execute initial actions
                    for action in actions {
                        use crate::llm::actions::client_trait::Client;
                        match protocol.as_ref().execute_action(action) {
                            Ok(ClientActionResult::Custom { name, data }) => {
                                if let Err(e) = Self::handle_custom_action(
                                    &name,
                                    data,
                                    client_id,
                                    &write_half_arc,
                                    &app_state,
                                    &status_tx,
                                )
                                .await
                                {
                                    error!("Failed to execute custom action: {}", e);
                                }
                            }
                            Ok(ClientActionResult::Disconnect) => {
                                info!("SSH Agent client {} disconnecting", client_id);
                                app_state
                                    .update_client_status(client_id, ClientStatus::Disconnected)
                                    .await;
                                return Ok("127.0.0.1:0".parse().unwrap());
                            }
                            Ok(ClientActionResult::WaitForMore) => {}
                            Ok(ClientActionResult::NoAction) => {}
                            Ok(ClientActionResult::SendData(_)) => {
                                error!("SendData not expected in SSH Agent client (protocol should use Custom actions)");
                            }
                            Ok(ClientActionResult::Multiple(_)) => {
                                error!("Multiple not expected in SSH Agent client (protocol should use individual actions)");
                            }
                            Err(e) => {
                                error!("Action execution error: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error on SSH Agent client connect: {}", e);
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
                            &crate::client::ssh_agent::actions::SshAgentClientProtocol,
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
                                "[CLIENT] SSH Agent client {} disconnected (injected action)",
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
                        info!("SSH Agent client {} disconnected", client_id);
                        app_state
                            .update_client_status(client_id, ClientStatus::Disconnected)
                            .await;
                        let _ = status_tx.send(format!(
                            "[CLIENT] SSH Agent client {} disconnected",
                            client_id
                        ));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                    Ok(n) => {
                        let data = buffer[..n].to_vec();
                        trace!("SSH Agent client {} received {} bytes", client_id, n);

                        // Handle data with LLM
                        let mut client_data_lock = client_data.lock().await;

                        match client_data_lock.state {
                            ConnectionState::Idle => {
                                // Process immediately
                                client_data_lock.state = ConnectionState::Processing;
                                let current_memory = client_data_lock.memory.clone();
                                drop(client_data_lock);

                                // Parse response and call LLM
                                if let Some(instruction) =
                                    app_state.get_instruction_for_client(client_id).await
                                {
                                    match Self::parse_response(&data) {
                                        Ok(event_data) => {
                                            let event = Event::new(
                                                &SSH_AGENT_CLIENT_RESPONSE_RECEIVED_EVENT,
                                                event_data,
                                            );

                                            match call_llm_for_client(
                                                &llm_client,
                                                &app_state,
                                                client_id.to_string(),
                                                &instruction,
                                                &current_memory,
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
                                                        match protocol
                                                            .as_ref()
                                                            .execute_action(action)
                                                        {
                                                            Ok(ClientActionResult::Custom {
                                                                name,
                                                                data,
                                                            }) => {
                                                                if let Err(e) =
                                                                    Self::handle_custom_action(
                                                                        &name,
                                                                        data,
                                                                        client_id,
                                                                        &write_half_arc,
                                                                        &app_state,
                                                                        &status_tx,
                                                                    )
                                                                    .await
                                                                {
                                                                    error!("Failed to execute custom action: {}", e);
                                                                }
                                                            }
                                                            Ok(ClientActionResult::Disconnect) => {
                                                                info!("SSH Agent client {} disconnecting", client_id);
                                                                app_state
                                                                    .update_client_status(
                                                                        client_id,
                                                                        ClientStatus::Disconnected,
                                                                    )
                                                                    .await;
                                                                // This early return leaves the
                                                                // read loop, so the after-loop
                                                                // cleanup below never runs on this
                                                                // path — drop the command handle
                                                                // here too, or a dashboard [ send ]
                                                                // keeps being offered on a dead
                                                                // client (idempotent with the
                                                                // after-loop removal).
                                                                app_state
                                                                    .remove_client_handle(client_id)
                                                                    .await;
                                                                return;
                                                            }
                                                            Ok(ClientActionResult::WaitForMore) => {
                                                            }
                                                            Ok(ClientActionResult::NoAction) => {}
                                                            Ok(ClientActionResult::SendData(_)) => {
                                                                error!("SendData not expected in SSH Agent client (protocol should use Custom actions)");
                                                            }
                                                            Ok(ClientActionResult::Multiple(_)) => {
                                                                error!("Multiple not expected in SSH Agent client (protocol should use individual actions)");
                                                            }
                                                            Err(e) => {
                                                                error!(
                                                                    "Action execution error: {}",
                                                                    e
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    error!(
                                                        "LLM error for SSH Agent client {}: {}",
                                                        client_id, e
                                                    );
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            error!("Failed to parse SSH Agent response: {}", e);
                                        }
                                    }
                                }

                                // Check for queued data
                                let mut client_data_lock = client_data.lock().await;
                                if client_data_lock.state == ConnectionState::Accumulating {
                                    let _queued = std::mem::take(&mut client_data_lock.queued_data);
                                    client_data_lock.state = ConnectionState::Idle;
                                    drop(client_data_lock);
                                    // Process queued data (recursive)
                                    // For simplicity, we'll just discard queued data for now
                                } else {
                                    client_data_lock.state = ConnectionState::Idle;
                                }
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
                        error!("SSH Agent client {} read error: {}", client_id, e);
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                            .await;
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

        // Return dummy socket address (Unix sockets don't have IP addresses)
        Ok("127.0.0.1:0".parse().unwrap())
    }

    /// Parse SSH Agent response message
    fn parse_response(data: &[u8]) -> Result<serde_json::Value> {
        if data.is_empty() {
            anyhow::bail!("Empty response");
        }

        // SSH Agent wire format: [uint32: length][byte: type][data...]
        // Assume length prefix already consumed by reader
        let msg_type = data[0];
        let mut cursor = &data[1..];

        match msg_type {
            SSH_AGENT_SUCCESS => Ok(serde_json::json!({
                "response_type": "success",
                "response_data": {}
            })),
            SSH_AGENT_FAILURE => Ok(serde_json::json!({
                "response_type": "failure",
                "response_data": {}
            })),
            SSH_AGENT_IDENTITIES_ANSWER => {
                let num_keys = Self::read_uint32(&mut cursor)?;
                let mut identities = Vec::new();

                for _ in 0..num_keys {
                    let key_blob = Self::read_string(&mut cursor)?;
                    let comment = Self::read_string(&mut cursor)?;

                    identities.push(serde_json::json!({
                        "public_key_blob_hex": hex::encode(key_blob),
                        "comment": String::from_utf8_lossy(comment),
                    }));
                }

                Ok(serde_json::json!({
                    "response_type": "identities",
                    "response_data": {
                        "count": num_keys,
                        "identities": identities,
                    }
                }))
            }
            SSH_AGENT_SIGN_RESPONSE => {
                let signature_blob = Self::read_string(&mut cursor)?;

                Ok(serde_json::json!({
                    "response_type": "signature",
                    "response_data": {
                        "signature_hex": hex::encode(signature_blob),
                    }
                }))
            }
            _ => {
                anyhow::bail!("Unknown SSH Agent response type: {}", msg_type);
            }
        }
    }

    /// Handle custom action by name
    async fn handle_custom_action(
        action_name: &str,
        data: serde_json::Value,
        client_id: ClientId,
        write_half: &Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
        _app_state: &Arc<AppState>,
        _status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        match action_name {
            "request_identities" => {
                let mut message = BytesMut::new();
                message.put_u8(SSH_AGENTC_REQUEST_IDENTITIES);
                Self::send_message(message.freeze(), write_half).await?;
                trace!("SSH Agent client {} sent REQUEST_IDENTITIES", client_id);
            }
            "sign_request" => {
                let public_key_blob_hex = data["public_key_blob_hex"]
                    .as_str()
                    .context("Missing public_key_blob_hex")?;
                let data_hex = data["data_hex"].as_str().context("Missing data_hex")?;
                let flags = data["flags"].as_u64().unwrap_or(0) as u32;

                let public_key_blob = hex::decode(public_key_blob_hex)?;
                let data_to_sign = hex::decode(data_hex)?;

                let mut message = BytesMut::new();
                message.put_u8(SSH_AGENTC_SIGN_REQUEST);
                Self::write_string(&mut message, &public_key_blob);
                Self::write_string(&mut message, &data_to_sign);
                message.put_u32(flags);

                Self::send_message(message.freeze(), write_half).await?;
                trace!("SSH Agent client {} sent SIGN_REQUEST", client_id);
            }
            "add_identity" => {
                let key_type = data["key_type"].as_str().context("Missing key_type")?;
                let public_key_blob_hex = data["public_key_blob_hex"]
                    .as_str()
                    .context("Missing public_key_blob_hex")?;
                let private_key_blob_hex = data["private_key_blob_hex"]
                    .as_str()
                    .context("Missing private_key_blob_hex")?;
                let comment = data["comment"].as_str().unwrap_or("");

                let public_key_blob = hex::decode(public_key_blob_hex)?;
                let private_key_blob = hex::decode(private_key_blob_hex)?;

                let mut message = BytesMut::new();
                message.put_u8(SSH_AGENTC_ADD_IDENTITY);
                Self::write_string(&mut message, key_type.as_bytes());
                Self::write_string(&mut message, &public_key_blob);
                Self::write_string(&mut message, &private_key_blob);
                Self::write_string(&mut message, comment.as_bytes());

                Self::send_message(message.freeze(), write_half).await?;
                trace!("SSH Agent client {} sent ADD_IDENTITY", client_id);
            }
            "remove_identity" => {
                let public_key_blob_hex = data["public_key_blob_hex"]
                    .as_str()
                    .context("Missing public_key_blob_hex")?;
                let public_key_blob = hex::decode(public_key_blob_hex)?;

                let mut message = BytesMut::new();
                message.put_u8(SSH_AGENTC_REMOVE_IDENTITY);
                Self::write_string(&mut message, &public_key_blob);

                Self::send_message(message.freeze(), write_half).await?;
                trace!("SSH Agent client {} sent REMOVE_IDENTITY", client_id);
            }
            "remove_all_identities" => {
                let mut message = BytesMut::new();
                message.put_u8(SSH_AGENTC_REMOVE_ALL_IDENTITIES);
                Self::send_message(message.freeze(), write_half).await?;
                trace!("SSH Agent client {} sent REMOVE_ALL_IDENTITIES", client_id);
            }
            _ => {
                anyhow::bail!("Unknown custom action: {}", action_name);
            }
        }
        Ok(())
    }

    /// Send message with SSH wire format (length prefix + data)
    async fn send_message(
        data: Bytes,
        write_half: &Arc<Mutex<tokio::io::WriteHalf<UnixStream>>>,
    ) -> Result<()> {
        let mut message = BytesMut::new();
        message.put_u32(data.len() as u32);
        message.put_slice(&data);

        write_half.lock().await.write_all(&message).await?;
        Ok(())
    }

    /// Read SSH wire format string (uint32 length + bytes)
    fn read_string<'a>(cursor: &mut &'a [u8]) -> Result<&'a [u8]> {
        if cursor.len() < 4 {
            anyhow::bail!("Not enough data to read string length");
        }
        let len = u32::from_be_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]) as usize;
        *cursor = &cursor[4..];

        if cursor.len() < len {
            anyhow::bail!("Not enough data to read string of length {}", len);
        }
        let result = &cursor[..len];
        *cursor = &cursor[len..];
        Ok(result)
    }

    /// Read SSH wire format uint32
    fn read_uint32(cursor: &mut &[u8]) -> Result<u32> {
        if cursor.len() < 4 {
            anyhow::bail!("Not enough data to read uint32");
        }
        let result = u32::from_be_bytes([cursor[0], cursor[1], cursor[2], cursor[3]]);
        *cursor = &cursor[4..];
        Ok(result)
    }

    /// Write SSH wire format string (uint32 length + bytes)
    fn write_string(buffer: &mut BytesMut, data: &[u8]) {
        buffer.put_u32(data.len() as u32);
        buffer.put_slice(data);
    }
}
