//! SVN (Subversion) server implementation
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use actions::{SVN_COMMAND_EVENT, SVN_GREETING_EVENT};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// Largest command line this server will buffer, in bytes.
///
/// `read_line` grows its `String` until it sees a newline, so an unbounded read lets one
/// peer that never sends `\n` grow the process without limit — no authentication, no
/// negotiation, just an open socket. A real ra_svn command tuple is a few hundred bytes;
/// 64 KiB is far above anything the subset implemented here can produce and far below a
/// memory problem.
const MAX_COMMAND_BYTES: u64 = 64 * 1024;

pub struct SvnServer;

impl SvnServer {
    /// Spawn SVN server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("SVN server listening on {}", local_addr));

        let protocol = Arc::new(actions::SvnProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, peer_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr: peer_addr,
                            local_addr,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: crate::state::server::ProtocolConnectionInfo::new(
                                serde_json::json!({
                                    "protocol": "svn",
                                    "authenticated": false,
                                    "repository_url": null,
                                    "commands_processed": 0
                                }),
                            ),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        Log::new(Some(&status_tx))
                            .info(format!("SVN client connected from {}", peer_addr));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let connection_id_clone = connection_id;

                        tokio::spawn(async move {
                            handle_svn_connection(
                                socket,
                                peer_addr,
                                llm_clone,
                                state_clone,
                                status_clone,
                                server_id,
                                protocol_clone,
                                connection_id_clone,
                            )
                            .await
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("SVN accept error: {}", e));
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
}

async fn handle_svn_connection(
    socket: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::SvnProtocol>,
    connection_id: ConnectionId,
) {
    // Split into an owned read half and a shared write half. The write half is an
    // Arc<Mutex<..>> so the reader below and the dashboard's peer-command task both
    // write through the same guarded sink (CLAUDE.md "Connection I/O").
    let (reader, write_half) = tokio::io::split(socket);
    let write_half = Arc::new(Mutex::new(write_half));
    let mut buf_reader = BufReader::new(reader);
    let log = Log::new(Some(&status_tx));

    // Peer messaging: the dashboard can inject an action (send_svn_success,
    // close_connection, ...) into THIS connection through the same executor the
    // model's actions use. The task ends when the handle is dropped by one of the
    // close paths below (each calls remove_peer_handle) or by server teardown.
    let peer_rx = crate::server::peer_support::register_peer_channel(
        &app_state,
        server_id,
        connection_id.as_u32(),
    )
    .await;
    crate::server::peer_support::spawn_peer_command_task(
        peer_rx,
        protocol.clone(),
        app_state.clone(),
        server_id,
        connection_id.as_u32(),
        write_half.clone(),
        status_tx.clone(),
    );

    // Send greeting event to LLM
    let greeting_event = Event::new(
        &SVN_GREETING_EVENT,
        serde_json::json!({ "client_ip": peer_addr.ip().to_string() }),
    );

    log.debug(format!("SVN sending greeting to {}", peer_addr));

    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &greeting_event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(execution_result) => {
            // Display messages from LLM
            for message in &execution_result.messages {
                log.info(message);
            }

            // Three outcomes have to stay apart in the log: the handler answered on the
            // wire, it asked to hang up, or it answered nothing at all. Only the third
            // shares a shape with an LLM error, and the `decision=` tags below keep even
            // those two greppable apart.
            let mut wrote_greeting = false;
            let mut close_requested = false;

            // Send greeting responses
            for protocol_result in execution_result.protocol_results {
                match protocol_result {
                    crate::llm::actions::protocol_trait::ActionResult::Output(output_data) => {
                        wrote_greeting = true;
                        {
                            let mut writer = write_half.lock().await;
                            if let Err(e) = writer.write_all(&output_data).await {
                                log.error(format!("SVN write error: {}", e));
                                drop(writer);
                                app_state
                                    .remove_peer_handle(server_id, connection_id.as_u32())
                                    .await;
                                return;
                            }
                            let _ = writer.flush().await;
                        }

                        // Update connection stats
                        app_state
                            .update_connection_stats(
                                server_id,
                                connection_id,
                                None,
                                Some(output_data.len() as u64),
                                None,
                                Some(1),
                            )
                            .await;

                        // Full payload FileOnly: the send_svn_* action template already
                        // reports the send to the TUI.
                        log.trace(format!(
                            "SVN sent greeting: {}",
                            String::from_utf8_lossy(&output_data)
                        ));
                    }
                    crate::llm::actions::protocol_trait::ActionResult::CloseConnection => {
                        close_requested = true;
                    }
                    _ => {}
                }
            }

            if close_requested {
                log.info(format!(
                    "SVN greeting from {} decision=model_close",
                    peer_addr
                ));
                close_svn_connection(&app_state, server_id, connection_id, &status_tx).await;
                return;
            }

            if !wrote_greeting {
                // The handler deliberately said nothing (a static handler with no actions,
                // or a human answering "with nothing" at the dashboard before injecting
                // bytes through `[ message this peer ]`). That is a real answer, so the
                // connection stays open — but it is logged distinctly from the error path
                // below so an operator can tell the two apart.
                log.warn(format!(
                    "SVN greeting from {} decision=no_action (nothing written; peer is \
                     waiting for a greeting)",
                    peer_addr
                ));
            }
        }
        Err(e) => {
            // The backend failed. The peer gets an svn `failure` tuple carrying only a
            // category — never the error, which names the backend, the model and our own
            // retry machinery. The error itself goes to the log and the status stream.
            let failure = WireFailure::classify(&e);
            log.error(format!(
                "SVN greeting for {} decision=fail_closed_llm_error category={} error: {}",
                peer_addr,
                if failure.is_overloaded() {
                    "overloaded"
                } else {
                    "unavailable"
                },
                e
            ));
            write_svn_failure(
                &write_half,
                &app_state,
                server_id,
                connection_id,
                &log,
                failure,
            )
            .await;
            close_svn_connection(&app_state, server_id, connection_id, &status_tx).await;
            return;
        }
    }

    // Main command loop
    let mut buffer = String::new();
    loop {
        buffer.clear();

        // Bounded: `Take` stops the read at the cap instead of buffering until a newline
        // that a hostile peer need never send. Rebuilt each iteration so the limit is
        // per-line, not per-connection.
        let read = (&mut buf_reader)
            .take(MAX_COMMAND_BYTES)
            .read_line(&mut buffer)
            .await;

        // The cap was reached with no newline in sight: this is not a command any svn
        // client sends, so stop reading rather than keep the peer's allocation alive.
        if matches!(&read, Ok(n) if *n as u64 == MAX_COMMAND_BYTES && !buffer.ends_with('\n')) {
            log.warn(format!(
                "SVN command line from {} exceeded {} bytes with no newline; closing",
                peer_addr, MAX_COMMAND_BYTES
            ));
            break;
        }

        match read {
            Ok(0) => {
                log.info(format!("SVN client {} disconnected", peer_addr));

                // Update connection status
                use crate::state::server::ConnectionStatus;
                app_state
                    .update_connection_status(server_id, connection_id, ConnectionStatus::Closed)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                break;
            }
            Ok(n) => {
                // Update connection stats
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        Some(n as u64),
                        None,
                        Some(1),
                        None,
                    )
                    .await;

                // Parse SVN command
                let command_line = buffer.trim().to_string();

                // Summary + full payload FileOnly: the svn_command event template
                // renders the equivalent line to the TUI.
                log.debug(format!("SVN received {} bytes from {}", n, peer_addr));
                log.trace(format!("SVN command: {}", command_line));

                // Parse SVN protocol command
                let parsed_command = parse_svn_command(&command_line);

                // Create event
                let event = Event::new(
                    &SVN_COMMAND_EVENT,
                    serde_json::json!({
                        "command_line": command_line,
                        "command": parsed_command.command,
                        "args": parsed_command.args,
                        "client_ip": peer_addr.ip().to_string(),
                    }),
                );

                log.debug(format!("SVN calling LLM for command from {}", peer_addr));

                // Call LLM
                match call_llm(
                    &llm_client,
                    &app_state,
                    server_id,
                    Some(connection_id),
                    &event,
                    protocol.as_ref(),
                )
                .await
                {
                    Ok(execution_result) => {
                        // Display messages from LLM
                        for message in &execution_result.messages {
                            log.info(message);
                        }

                        log.debug(format!(
                            "SVN got {} protocol results",
                            execution_result.protocol_results.len()
                        ));

                        // Send all outputs to client and check for close
                        let mut should_close = false;
                        let mut wrote_reply = false;
                        for protocol_result in execution_result.protocol_results {
                            match protocol_result {
                                crate::llm::actions::protocol_trait::ActionResult::Output(output_data) => {
                                    wrote_reply = true;
                                    {
                                        let mut writer = write_half.lock().await;
                                        if let Err(e) = writer.write_all(&output_data).await {
                                            log.error(format!("SVN write error: {}", e));
                                            drop(writer);
                                            app_state
                                                .remove_peer_handle(
                                                    server_id,
                                                    connection_id.as_u32(),
                                                )
                                                .await;
                                            return;
                                        }
                                        let _ = writer.flush().await;
                                    }

                                    // Update connection stats
                                    app_state
                                        .update_connection_stats(
                                            server_id,
                                            connection_id,
                                            None,
                                            Some(output_data.len() as u64),
                                            None,
                                            Some(1),
                                        )
                                        .await;

                                    // Summary + full payload FileOnly: the send_svn_*
                                    // action template already reports the send to the TUI.
                                    log.debug(format!(
                                        "SVN sent {} bytes to {}",
                                        output_data.len(),
                                        peer_addr
                                    ));
                                    log.trace(format!(
                                        "SVN response: {}",
                                        String::from_utf8_lossy(&output_data)
                                    ));
                                }
                                crate::llm::actions::protocol_trait::ActionResult::CloseConnection => {
                                    should_close = true;
                                    log.debug("SVN closing connection per LLM request");
                                }
                                _ => {} // Ignore other action results
                            }
                        }

                        // Break loop if LLM requested connection close
                        if should_close {
                            log.debug(format!(
                                "SVN command '{}' from {} decision=model_close",
                                parsed_command.command, peer_addr
                            ));
                            break;
                        }

                        if !wrote_reply {
                            // Answered with nothing: a real answer (static handler with no
                            // actions, or a human choosing silence), kept distinct in the
                            // log from the backend-failure path below.
                            log.warn(format!(
                                "SVN command '{}' from {} decision=no_action (nothing \
                                 written)",
                                parsed_command.command, peer_addr
                            ));
                        }
                    }
                    Err(e) => {
                        // Same rule as the greeting: category on the wire, error in the
                        // log. Without this the peer sat blocked on a command it had
                        // already sent until its own timeout expired.
                        let failure = WireFailure::classify(&e);
                        log.error(format!(
                            "SVN command '{}' from {} decision=fail_closed_llm_error \
                             category={} error: {}",
                            parsed_command.command,
                            peer_addr,
                            if failure.is_overloaded() {
                                "overloaded"
                            } else {
                                "unavailable"
                            },
                            e
                        ));
                        write_svn_failure(
                            &write_half,
                            &app_state,
                            server_id,
                            connection_id,
                            &log,
                            failure,
                        )
                        .await;
                        break;
                    }
                }
            }
            Err(e) => {
                log.error(format!("SVN read error from {}: {}", peer_addr, e));
                break;
            }
        }
    }

    // Update connection status to closed. Reached by every loop `break` (EOF,
    // read error, close_connection, LLM failure), so this is the one place the
    // peer handle must be dropped — otherwise the rail keeps offering
    // "message this peer" on a dead connection.
    use crate::state::server::ConnectionStatus;
    app_state
        .remove_peer_handle(server_id, connection_id.as_u32())
        .await;
    app_state
        .update_connection_status(server_id, connection_id, ConnectionStatus::Closed)
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());
}

/// Encode an svn `failure` tuple that carries a failure **category** and nothing else.
///
/// ra_svn lets the server answer any command — and the greeting itself — with
/// `( failure ( ( apr-err:number message:string file:string line:number ) ) )`, so this is
/// the protocol's own error shape rather than something invented here.
///
/// The two [`WireFailure`] categories map onto different apr error numbers so a client can
/// tell "come back later" from "this request is not going to work":
///
/// - `Overloaded` -> 210003 `SVN_ERR_RA_SVN_IO_ERROR`, the transient transport-side failure
/// - `Unavailable` -> 210000 `SVN_ERR_RA_SVN_CMD_ERR`, the generic command failure
///
/// The message is [`WireFailure::text`], a `&'static str`: no part of the underlying error
/// — backend URL, model name, file path, anyhow chain — can reach the wire through here.
fn svn_failure_tuple(failure: WireFailure) -> Vec<u8> {
    let error_code = if failure.is_overloaded() {
        210003
    } else {
        210000
    };
    let message = failure.text();
    // Counted strings (`<len>:<bytes>`); the file field is the empty string `0:`.
    format!(
        "( failure ( ( {} {}:{} 0: 0 ) ) )\n",
        error_code,
        message.len(),
        message
    )
    .into_bytes()
}

/// Write the failure tuple to the peer, best effort, and count the bytes.
async fn write_svn_failure(
    write_half: &Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
    app_state: &Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    log: &Log<'_>,
    failure: WireFailure,
) {
    let payload = svn_failure_tuple(failure);
    let written = {
        let mut writer = write_half.lock().await;
        match writer.write_all(&payload).await {
            Ok(()) => {
                let _ = writer.flush().await;
                true
            }
            Err(e) => {
                log.debug(format!("SVN could not write failure tuple: {}", e));
                false
            }
        }
    };
    if written {
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(payload.len() as u64),
                None,
                Some(1),
            )
            .await;
    }
}

/// Drop the peer handle and mark the connection closed (the greeting paths return before
/// reaching the loop's shared teardown).
async fn close_svn_connection(
    app_state: &Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    use crate::state::server::ConnectionStatus;
    app_state
        .remove_peer_handle(server_id, connection_id.as_u32())
        .await;
    app_state
        .update_connection_status(server_id, connection_id, ConnectionStatus::Closed)
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());
}

#[derive(Debug, Clone)]
struct ParsedSvnCommand {
    command: String,
    args: Vec<String>,
}

/// Parse SVN protocol command from line
/// SVN protocol uses S-expression-like format: ( command args... )
fn parse_svn_command(line: &str) -> ParsedSvnCommand {
    let line = line.trim();

    // Simple parser for SVN protocol format
    if line.starts_with('(') && line.ends_with(')') {
        let inner = &line[1..line.len() - 1];
        let parts: Vec<String> = inner.split_whitespace().map(String::from).collect();

        if parts.is_empty() {
            ParsedSvnCommand {
                command: String::new(),
                args: Vec::new(),
            }
        } else {
            ParsedSvnCommand {
                command: parts[0].clone(),
                args: parts[1..].to_vec(),
            }
        }
    } else {
        // Not a valid SVN command format, return as-is
        ParsedSvnCommand {
            command: line.to_string(),
            args: Vec::new(),
        }
    }
}
