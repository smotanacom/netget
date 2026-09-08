//! NNTP (Network News Transfer Protocol) client implementation
pub mod actions;

pub use actions::NntpClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::nntp::actions::{
    NNTP_CLIENT_CONNECTED_EVENT, NNTP_CLIENT_RESPONSE_RECEIVED_EVENT,
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
    queued_lines: Vec<String>,
    memory: String,
    last_command: Option<String>,
    pending_post_article: Option<String>,
}

/// NNTP client that connects to a remote NNTP server
pub struct NntpClient;

impl NntpClient {
    /// Connect to an NNTP server with integrated LLM actions
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
            "NNTP client {} connected to {} (local: {})",
            client_id, remote_sock_addr, local_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] NNTP client {} connected", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream for reading and writing
        let (read_half, write_half) = tokio::io::split(stream);
        let write_half_arc = Arc::new(Mutex::new(write_half));

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_lines: Vec::new(),
            memory: String::new(),
            last_command: None,
            pending_post_article: None,
        }));

        // Command channel for injected actions (the dashboard's [ nntp_* ] rows).
        // Registered BEFORE the connected-event LLM call, which a manual `*` rule can park
        // for minutes - the operator must be able to reach the client while it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        // `read_line` is not cancellation-safe, so the commands are drained by their own task
        // rather than a `select!` arm in the read loop. Both tasks share the write half.
        let cmd_state = app_state.clone();
        let cmd_tx = status_tx.clone();
        let cmd_write = write_half_arc.clone();
        let cmd_data = client_data.clone();
        let cmd_task = tokio::spawn(async move {
            Self::command_loop(
                command_rx, cmd_write, cmd_data, client_id, cmd_state, cmd_tx,
            )
            .await;
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Spawn read loop
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn(async move {
            let cleanup_state = app_state.clone();
            // The body is an inner block so every exit - early `return` on a missing welcome
            // or read error, `break` on EOF - falls through to the handle removal below.
            async {
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();

                // Read welcome message
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        error!("NNTP server closed connection before sending welcome");
                        app_state
                            .update_client_status(
                                client_id,
                                ClientStatus::Error("No welcome message".to_string()),
                            )
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        return;
                    }
                    Ok(_) => {
                        let welcome = line.trim();
                        info!("NNTP client {} received welcome: {}", client_id, welcome);

                        // Parse status code (for future use)
                        let _status_code = welcome
                            .split_whitespace()
                            .next()
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(0);

                        // Call LLM with connected event
                        if let Some(instruction) =
                            app_state.get_instruction_for_client(client_id).await
                        {
                            let protocol =
                                Arc::new(crate::client::nntp::actions::NntpClientProtocol::new());
                            let event = Event::new(
                                &NNTP_CLIENT_CONNECTED_EVENT,
                                serde_json::json!({
                                    "remote_addr": remote_sock_addr.to_string(),
                                    "welcome_message": welcome,
                                }),
                            );

                            // Snapshot the memory before the `match`, do not borrow it out of a
                            // guard in the scrutinee. A temporary created in a `match`
                            // scrutinee lives until the end of the whole `match`, so
                            // `&client_data.lock().await.memory` leaves the guard held across
                            // every arm — and the arms re-lock the same non-reentrant
                            // `tokio::sync::Mutex` (the `memory_updates` write below, and every
                            // `apply_action` that records `last_command`). That deadlocked the
                            // read loop against itself permanently: the command reached the
                            // wire, then the task never read another byte and the dashboard's
                            // inject path blocked behind it too.
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

                                    // Execute actions from LLM
                                    Self::execute_actions(
                                        actions,
                                        &protocol,
                                        &write_half_arc,
                                        &client_data,
                                        client_id,
                                        &status_tx,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    error!("LLM error for NNTP client {}: {}", client_id, e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("NNTP client {} read error: {}", client_id, e);
                        app_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        return;
                    }
                }

                // Main read loop
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            info!("NNTP client {} disconnected", client_id);
                            app_state
                                .update_client_status(client_id, ClientStatus::Disconnected)
                                .await;
                            let _ = status_tx
                                .send(format!("[CLIENT] NNTP client {} disconnected", client_id));
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            break;
                        }
                        Ok(_) => {
                            let response = line.trim().to_string();
                            if response.is_empty() {
                                continue;
                            }

                            trace!("NNTP client {} received: {}", client_id, response);

                            // Parse status code
                            let status_code = response
                                .split_whitespace()
                                .next()
                                .and_then(|s| s.parse::<u32>().ok())
                                .unwrap_or(0);

                            // Check if this is a multi-line response
                            let is_multiline = matches!(
                                status_code,
                                100 | // HELP text
                            215 | // LIST response
                            220 | // ARTICLE follows
                            221 | // HEAD follows
                            222 | // BODY follows
                            224 | // XOVER follows
                            230 | // NEWNEWS follows
                            231 // NEWGROUPS follows
                            );

                            // Collect multi-line responses
                            let full_response = if is_multiline {
                                let mut lines = vec![response.clone()];
                                loop {
                                    line.clear();
                                    match reader.read_line(&mut line).await {
                                        Ok(0) => break,
                                        Ok(_) => {
                                            let data_line = line.trim();
                                            if data_line == "." {
                                                // End of multi-line response
                                                break;
                                            }
                                            lines.push(data_line.to_string());
                                        }
                                        Err(e) => {
                                            error!("Error reading multi-line response: {}", e);
                                            break;
                                        }
                                    }
                                }
                                lines.join("\n")
                            } else {
                                response.clone()
                            };

                            debug!("NNTP client {} full response: {}", client_id, full_response);

                            // Handle 340 POST response - send pending article immediately
                            if status_code == 340 {
                                let mut client_data_lock = client_data.lock().await;
                                if let Some(article_data) =
                                    client_data_lock.pending_post_article.take()
                                {
                                    drop(client_data_lock);

                                    trace!("NNTP client {} sending article for POST", client_id);
                                    if let Err(e) = write_half_arc
                                        .lock()
                                        .await
                                        .write_all(article_data.as_bytes())
                                        .await
                                    {
                                        error!(
                                            "NNTP client {} failed to send article: {}",
                                            client_id, e
                                        );
                                    } else {
                                        let _ = status_tx.send(format!(
                                            "[CLIENT] NNTP {} > [article sent]",
                                            client_id
                                        ));
                                    }
                                    continue; // Skip LLM processing for 340
                                }
                            }

                            // Handle response with LLM
                            let mut client_data_lock = client_data.lock().await;

                            match client_data_lock.state {
                                ConnectionState::Idle => {
                                    // Process immediately
                                    client_data_lock.state = ConnectionState::Processing;
                                    let last_command = client_data_lock.last_command.clone();
                                    drop(client_data_lock);

                                    // Call LLM
                                    if let Some(instruction) =
                                        app_state.get_instruction_for_client(client_id).await
                                    {
                                        let protocol = Arc::new(
                                            crate::client::nntp::actions::NntpClientProtocol::new(),
                                        );
                                        let event = Event::new(
                                            &NNTP_CLIENT_RESPONSE_RECEIVED_EVENT,
                                            serde_json::json!({
                                                "status_code": status_code,
                                                "response": full_response,
                                                "command": last_command,
                                            }),
                                        );

                                        // Snapshot before the `match` - see the note on the
                                        // connected-event call above. Borrowing out of a guard
                                        // in the scrutinee holds it across every arm, and the
                                        // arms re-lock the same non-reentrant mutex.
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
                                                Self::execute_actions(
                                                    actions,
                                                    &protocol,
                                                    &write_half_arc,
                                                    &client_data,
                                                    client_id,
                                                    &status_tx,
                                                )
                                                .await;
                                            }
                                            Err(e) => {
                                                error!(
                                                    "LLM error for NNTP client {}: {}",
                                                    client_id, e
                                                );
                                            }
                                        }
                                    }

                                    // Process queued lines if any
                                    let mut client_data_lock = client_data.lock().await;
                                    if !client_data_lock.queued_lines.is_empty() {
                                        client_data_lock.queued_lines.clear();
                                    }
                                    client_data_lock.state = ConnectionState::Idle;
                                }
                                ConnectionState::Processing => {
                                    // Queue data
                                    client_data_lock.queued_lines.push(full_response);
                                    client_data_lock.state = ConnectionState::Accumulating;
                                }
                                ConnectionState::Accumulating => {
                                    // Continue queuing
                                    client_data_lock.queued_lines.push(full_response);
                                }
                            }
                        }
                        Err(e) => {
                            error!("NNTP client {} read error: {}", client_id, e);
                            app_state
                                .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                                .await;
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            break;
                        }
                    }
                }
            }
            .await;
            // Every exit path lands here: drop the command handle so the dashboard stops
            // offering [ send ] on a dead connection (a late send then fails fast).
            cleanup_state.remove_client_handle(client_id).await;
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(local_addr)
    }

    /// Drain injected commands until the channel closes (client removed) or an injected
    /// `nntp_quit` ends the session.
    ///
    /// The generic `command_support::handle_stream_client_command` cannot run this client's
    /// vocabulary because every wire verb yields `ClientActionResult::Custom`, so the action
    /// goes through [`Self::apply_action`] - the same function the LLM path uses - and the
    /// outcome is recorded and replied exactly the way the generic arm does it.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<crate::state::client_handles::ClientCommand>,
        write_half_arc: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        client_data: Arc<Mutex<ClientData>>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::client_trait::Client;
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::client_handles::ClientSendOutcome;
        use crate::state::AccessLogOwner;

        let protocol = Arc::new(NntpClientProtocol::new());

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.as_ref().execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => {
                    Self::apply_action(result, &write_half_arc, &client_data, client_id, &status_tx)
                        .await
                        .map(|applied| match applied {
                            Applied::Disconnect => ClientSendOutcome::Disconnected,
                            Applied::Sent(0) => ClientSendOutcome::Executed {
                                detail: "executed (nothing to write)".to_string(),
                            },
                            Applied::Sent(bytes_sent) => ClientSendOutcome::Sent { bytes_sent },
                        })
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
                error!("NNTP client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                // QUIT is already on the wire; half-close so the server reads EOF and the
                // read loop runs its normal disconnect path.
                let _ = write_half_arc.lock().await.shutdown().await;
                app_state.remove_client_handle(client_id).await;
                break;
            }
        }
    }

    /// Execute actions from LLM
    async fn execute_actions(
        actions: Vec<serde_json::Value>,
        protocol: &Arc<NntpClientProtocol>,
        write_half_arc: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        client_data: &Arc<Mutex<ClientData>>,
        client_id: ClientId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::client_trait::Client;

        for action in actions {
            match protocol.as_ref().execute_action(action) {
                Ok(result) => {
                    match Self::apply_action(
                        result,
                        write_half_arc,
                        client_data,
                        client_id,
                        status_tx,
                    )
                    .await
                    {
                        Ok(Applied::Disconnect) => break,
                        Ok(Applied::Sent(_)) => {}
                        Err(e) => {
                            error!("NNTP client {} failed to send: {}", client_id, e);
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "Error executing action for NNTP client {}: {}",
                        client_id, e
                    );
                }
            }
        }
    }

    /// Put one executed action on the wire. Shared by the LLM path and injected commands so
    /// the encoding of every NNTP verb exists exactly once.
    async fn apply_action(
        result: crate::llm::actions::client_trait::ClientActionResult,
        write_half_arc: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        client_data: &Arc<Mutex<ClientData>>,
        client_id: ClientId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        use crate::llm::actions::client_trait::ClientActionResult;

        match result {
            ClientActionResult::Custom { name, data } => {
                if name == "nntp_command" {
                    // Send NNTP command
                    let command = data["command"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("Missing command in action data"))?;
                    let command_line = format!("{}\r\n", command);
                    write_half_arc
                        .lock()
                        .await
                        .write_all(command_line.as_bytes())
                        .await?;
                    trace!("NNTP client {} sent: {}", client_id, command);
                    let _ = status_tx.send(format!("[CLIENT] NNTP {} > {}", client_id, command));
                    client_data.lock().await.last_command = Some(command.to_string());
                    Ok(Applied::Sent(command_line.len()))
                } else if name == "nntp_post" {
                    // Handle POST command - send POST and wait for 340 response
                    let (headers, body) = match (data["headers"].as_object(), data["body"].as_str())
                    {
                        (Some(h), Some(b)) => (h, b),
                        _ => anyhow::bail!("Missing headers or body in nntp_post action data"),
                    };

                    // Build article data to send after receiving 340
                    let mut article = String::new();
                    for (key, value) in headers {
                        if let Some(val_str) = value.as_str() {
                            article.push_str(&format!("{}: {}\r\n", key, val_str));
                        }
                    }
                    // Blank line between headers and body, then terminate with CRLF.CRLF
                    article.push_str("\r\n");
                    article.push_str(body);
                    article.push_str("\r\n.\r\n");

                    // Store article for sending after 340 response
                    client_data.lock().await.pending_post_article = Some(article);

                    let post_command = "POST\r\n";
                    if let Err(e) = write_half_arc
                        .lock()
                        .await
                        .write_all(post_command.as_bytes())
                        .await
                    {
                        // Failed to send POST, clear pending article
                        client_data.lock().await.pending_post_article = None;
                        return Err(e.into());
                    }
                    trace!("NNTP client {} sent: POST (waiting for 340)", client_id);
                    let _ = status_tx.send(format!("[CLIENT] NNTP {} > POST", client_id));
                    client_data.lock().await.last_command = Some("POST".to_string());
                    Ok(Applied::Sent(post_command.len()))
                } else {
                    Ok(Applied::Sent(0))
                }
            }
            ClientActionResult::Disconnect => {
                // Send QUIT command
                let quit_command = "QUIT\r\n";
                write_half_arc
                    .lock()
                    .await
                    .write_all(quit_command.as_bytes())
                    .await?;
                info!("NNTP client {} disconnecting", client_id);
                let _ = status_tx.send(format!("[CLIENT] NNTP {} > QUIT", client_id));
                Ok(Applied::Disconnect)
            }
            ClientActionResult::WaitForMore => {
                // Do nothing, just wait
                trace!("NNTP client {} waiting for more data", client_id);
                Ok(Applied::Sent(0))
            }
            // Other action results not applicable to NNTP
            _ => Ok(Applied::Sent(0)),
        }
    }
}

/// What [`NntpClient::apply_action`] did with one action.
enum Applied {
    /// Bytes written (0 when the action produced no wire output).
    Sent(usize),
    /// QUIT was written and the session should end.
    Disconnect,
}
