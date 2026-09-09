//! Redis client implementation
pub mod actions;

pub use actions::RedisClientProtocol;

use crate::llm::actions::client_trait::{Client, ClientActionResult};
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::redis::actions::{
    REDIS_CLIENT_CONNECTED_EVENT, REDIS_CLIENT_RESPONSE_RECEIVED_EVENT,
};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::patterns;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// Redis client that connects to a Redis server
pub struct RedisClient;

impl RedisClient {
    /// Connect to a Redis server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // Connect to Redis server
        let stream = TcpStream::connect(&remote_addr)
            .await
            .context(format!("Failed to connect to Redis at {}", remote_addr))?;

        let local_addr = stream.local_addr()?;
        let remote_sock_addr = stream.peer_addr()?;

        info!(
            "Redis client {} {} {} (local: {})",
            client_id,
            patterns::REDIS_CLIENT_CONNECTED,
            remote_sock_addr,
            local_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] Redis client {} connected", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream
        let (read_half, write_half) = tokio::io::split(stream);
        let write_half_arc = Arc::new(Mutex::new(write_half));
        let mut reader = BufReader::new(read_half);
        let protocol = Arc::new(RedisClientProtocol::new());

        // Command channel for injected actions (the dashboard's [ execute_redis_command ]).
        // Registered BEFORE the connected-event LLM call, which a manual `*` rule can park
        // for minutes - the operator must be able to reach the client while it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        // `read_line` is not cancellation-safe, so the commands are drained by their own task
        // rather than a `select!` arm in the read loop. Both tasks share the write half.
        let cmd_state = app_state.clone();
        let cmd_tx = status_tx.clone();
        let cmd_write = write_half_arc.clone();
        let cmd_protocol = protocol.clone();
        let cmd_task = tokio::spawn(async move {
            Self::command_loop(
                command_rx,
                cmd_protocol,
                cmd_write,
                client_id,
                cmd_state,
                cmd_tx,
            )
            .await;
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Call LLM with redis_connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &REDIS_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "remote_addr": remote_sock_addr.to_string(),
                }),
            );

            // `initial_memory` is written before `connect()` runs
            // (`src/cli/client_startup.rs`), so it is available here; passing an empty string
            // discarded whatever the operator or the model had set up.
            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

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
                Ok(result) => {
                    // A `set_memory` on the connect turn used to be dropped on the floor -
                    // `result.memory_updates` was never read - so the model could not carry
                    // anything from its first turn into the next one. mysql and postgresql
                    // both do this correctly.
                    if let Some(mem) = result.memory_updates {
                        app_state.set_memory_for_client(client_id, mem).await;
                    }
                    for action in result.actions {
                        match protocol.execute_action(action) {
                            Ok(action_result) => {
                                match Self::apply_action(action_result, &write_half_arc, client_id)
                                    .await
                                {
                                    Ok(Applied::Disconnect) => {
                                        info!("LLM requested disconnect after connect");
                                        let _ = write_half_arc.lock().await.shutdown().await;
                                        app_state.remove_client_handle(client_id).await;
                                        app_state
                                            .update_client_status(
                                                client_id,
                                                ClientStatus::Disconnected,
                                            )
                                            .await;
                                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                                        return Ok(local_addr);
                                    }
                                    Ok(Applied::Sent(_)) => {}
                                    Err(e) => {
                                        error!("Failed to send Redis command after connect: {}", e)
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Redis client {} could not execute action after connect: {}",
                                    client_id, e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error on redis_connected event: {}", e);
                }
            }
        }

        // Spawn read loop for Redis responses. Registered with AppState so
        // stop_client can abort it and release the socket.
        let task_registrar = app_state.clone();
        let handle = tokio::spawn(async move {
            loop {
                // Read Redis RESP response
                // Simplified: just read line-by-line
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        info!(
                            "Redis client {} {}",
                            client_id,
                            patterns::REDIS_CLIENT_DISCONNECTED
                        );
                        app_state
                            .update_client_status(client_id, ClientStatus::Disconnected)
                            .await;
                        let _ = status_tx
                            .send(format!("[CLIENT] Redis client {} disconnected", client_id));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                    Ok(_) => {
                        trace!("Redis client {} received: {}", client_id, line.trim());

                        // Call LLM with response
                        if let Some(instruction) =
                            app_state.get_instruction_for_client(client_id).await
                        {
                            let event = Event::new(
                                &REDIS_CLIENT_RESPONSE_RECEIVED_EVENT,
                                serde_json::json!({
                                    "response": line.trim(),
                                }),
                            );

                            let memory = app_state
                                .get_memory_for_client(client_id)
                                .await
                                .unwrap_or_default();

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
                                        app_state.set_memory_for_client(client_id, mem).await;
                                    }

                                    // Execute actions
                                    //
                                    // `disconnect_requested` rather than a bare `break`: the
                                    // break here left only the `for action in actions` loop,
                                    // so the read loop carried straight on and the socket
                                    // stayed open. Nor did it shut the write half, drop the
                                    // command handle or set `Disconnected` - all of which the
                                    // connect-time path above does. The model could ask this
                                    // client to disconnect and nothing at all happened.
                                    let mut disconnect_requested = false;
                                    for action in actions {
                                        match protocol.execute_action(action) {
                                            Ok(action_result) => {
                                                match Self::apply_action(
                                                    action_result,
                                                    &write_half_arc,
                                                    client_id,
                                                )
                                                .await
                                                {
                                                    Ok(Applied::Disconnect) => {
                                                        info!(
                                                            "Redis client {} disconnecting",
                                                            client_id
                                                        );
                                                        disconnect_requested = true;
                                                        break;
                                                    }
                                                    Ok(Applied::Sent(_)) => {}
                                                    Err(e) => {
                                                        error!(
                                                            "Redis client {} write failed: {}",
                                                            client_id, e
                                                        );
                                                    }
                                                }
                                            }
                                            // Do not swallow. `_ => {}` here meant an action
                                            // the client cannot execute — including one it
                                            // never advertised — was indistinguishable from
                                            // success: nothing went on the wire and nothing
                                            // said so. Two tests passed at HEAD while sending
                                            // `wait_for_more`, which this client does not
                                            // implement.
                                            Err(e) => {
                                                error!(
                                                    "Redis client {} could not execute action: {}",
                                                    client_id, e
                                                );
                                                let _ = status_tx.send(format!(
                                                    "[ERROR] Redis client {} action failed: {}",
                                                    client_id, e
                                                ));
                                            }
                                        }
                                    }

                                    if disconnect_requested {
                                        // The same teardown the connect-time path runs:
                                        // half-close so the server sees EOF, drop the command
                                        // handle so the dashboard stops offering [ send ] on
                                        // a client whose loop is gone, and record the status.
                                        let _ = write_half_arc.lock().await.shutdown().await;
                                        app_state.remove_client_handle(client_id).await;
                                        app_state
                                            .update_client_status(
                                                client_id,
                                                ClientStatus::Disconnected,
                                            )
                                            .await;
                                        let _ = status_tx.send(format!(
                                            "[CLIENT] Redis client {} disconnected",
                                            client_id
                                        ));
                                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!("LLM error for Redis client {}: {}", client_id, e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Redis client {} read error: {}", client_id, e);
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        break;
                    }
                }
            }
            // Every exit path lands here: drop the command handle so the dashboard stops
            // offering [ send ] on a dead connection (a late send then fails fast). This
            // also closes the command channel, which ends `command_loop`.
            app_state.remove_client_handle(client_id).await;
            let _ = status_tx.send("__UPDATE_UI__".to_string());
        });
        task_registrar.register_client_task(client_id, handle).await;

        Ok(local_addr)
    }

    /// Drain injected commands until the channel closes (client removed) or an injected
    /// `disconnect` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot run this client's
    /// vocabulary because `execute_redis_command` yields `ClientActionResult::Custom`, so the
    /// action goes through [`Self::apply_action`] - the same function the LLM path uses -
    /// and the outcome is recorded and replied exactly the way the generic arm does it.
    async fn command_loop<W>(
        mut command_rx: tokio::sync::mpsc::Receiver<crate::state::client_handles::ClientCommand>,
        protocol: Arc<RedisClientProtocol>,
        write_half: Arc<Mutex<W>>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) where
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::client_handles::ClientSendOutcome;
        use crate::state::AccessLogOwner;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => Self::apply_action(result, &write_half, client_id)
                    .await
                    .map(|applied| match applied {
                        Applied::Disconnect => ClientSendOutcome::Disconnected,
                        Applied::Sent(0) => ClientSendOutcome::Executed {
                            detail: "executed (nothing to write)".to_string(),
                        },
                        Applied::Sent(bytes_sent) => ClientSendOutcome::Sent { bytes_sent },
                    }),
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
                error!("Redis client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                // Half-close so the server reads EOF and the read loop runs its normal
                // disconnect path.
                let _ = write_half.lock().await.shutdown().await;
                break;
            }
        }
    }

    /// Put one executed action on the wire. Shared by the connected-event path, the read
    /// loop and injected commands so the RESP encoding of `execute_redis_command` exists
    /// exactly once.
    async fn apply_action<W>(
        action_result: ClientActionResult,
        write_half: &Arc<Mutex<W>>,
        client_id: ClientId,
    ) -> Result<Applied>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        match action_result {
            ClientActionResult::Custom { name, data } => {
                if name != "redis_command" {
                    debug!(
                        "Redis client {} ignoring unhandled custom result: {}",
                        client_id, name
                    );
                    return Ok(Applied::Sent(0));
                }
                let command = data
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing command in action data"))?;
                let cmd_bytes = encode_redis_command(command);

                let mut writer = write_half.lock().await;
                writer.write_all(&cmd_bytes).await?;
                writer.flush().await?;
                info!("{} {}", patterns::REDIS_CLIENT_SENT_COMMAND, command);
                Ok(Applied::Sent(cmd_bytes.len()))
            }
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            other => {
                debug!(
                    "Redis client {} ignoring unhandled action result: {:?}",
                    client_id, other
                );
                Ok(Applied::Sent(0))
            }
        }
    }
}

/// What [`RedisClient::apply_action`] did with one action.
enum Applied {
    /// Bytes written (0 when the action produced no wire output).
    Sent(usize),
    /// The session should end.
    Disconnect,
}

/// Encode a Redis command as a RESP array
///
/// Example: "PING" -> "*1\r\n$4\r\nPING\r\n"
/// Example: "SET key value" -> "*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n"
fn encode_redis_command(command: &str) -> Vec<u8> {
    // Split command into parts
    let parts: Vec<&str> = command.split_whitespace().collect();

    // Start with array length
    let mut result = format!("*{}\r\n", parts.len()).into_bytes();

    // Encode each part as a bulk string
    for part in parts {
        result.extend_from_slice(&format!("${}\r\n", part.len()).into_bytes());
        result.extend_from_slice(part.as_bytes());
        result.extend_from_slice(b"\r\n");
    }

    result
}
