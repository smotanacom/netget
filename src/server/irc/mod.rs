//! IRC server implementation
pub mod actions;
pub(crate) mod wire;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::server::irc::wire::{read_irc_line, IrcLine, MAX_IRC_READ_LINE};
use crate::server::IrcProtocol;
use crate::state::app_state::AppState;
use actions::IRC_MESSAGE_RECEIVED_EVENT;

/// IRC server that forwards messages to LLM
pub struct IrcServer;

impl IrcServer {
    /// Spawn IRC server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("IRC server (action-based) listening on {}", local_addr);
        let _ = status_tx.send(format!("IRC server listening on {}", local_addr));

        let protocol = Arc::new(IrcProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let (read_half, write_half) = tokio::io::split(stream);
                            let write_half_arc = Arc::new(tokio::sync::Mutex::new(write_half));

                            // Add connection to ServerInstance
                            use crate::state::server::{
                                ConnectionState as ServerConnectionState, ConnectionStatus,
                                ProtocolConnectionInfo,
                            };
                            let now = std::time::Instant::now();
                            let conn_state = ServerConnectionState {
                                id: connection_id,
                                remote_addr,
                                local_addr: local_addr_conn,
                                bytes_sent: 0,
                                bytes_received: 0,
                                packets_sent: 0,
                                packets_received: 0,
                                last_activity: now,
                                status: ConnectionStatus::Active,
                                status_changed_at: now,
                                protocol_info: ProtocolConnectionInfo::empty(),
                            };
                            state_clone
                                .add_connection_to_server(server_id, conn_state)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());

                            // Peer messaging: the dashboard's "message this peer" /
                            // "disconnect this peer" inject actions into THIS connection
                            // through the same executor the LLM path uses. Registered
                            // before the first read, so a manual `*` rule parking the
                            // registration exchange still leaves the operator able to
                            // reach the connection.
                            let peer_rx = crate::server::peer_support::register_peer_channel(
                                &state_clone,
                                server_id,
                                connection_id.as_u32(),
                            )
                            .await;
                            crate::server::peer_support::spawn_peer_command_task(
                                peer_rx,
                                protocol_clone.clone(),
                                state_clone.clone(),
                                server_id,
                                connection_id.as_u32(),
                                write_half_arc.clone(),
                                status_clone.clone(),
                            );

                            let mut reader = BufReader::new(read_half);

                            loop {
                                // Bounded: `read_line` grows its buffer until it finds a
                                // newline, so an unauthenticated peer that connects and
                                // streams bytes with no `\n` was a one-connection OOM. IRC
                                // messages are 512 bytes by RFC 1459 and 8704 with IRCv3
                                // tags, so nothing real is refused by the ceiling.
                                let (read, n) =
                                    match read_irc_line(&mut reader, MAX_IRC_READ_LINE).await {
                                        Ok(v) => v,
                                        Err(e) => {
                                            debug!(
                                                "IRC read error on connection {}: {}",
                                                connection_id, e
                                            );
                                            break;
                                        }
                                    };
                                let line = match read {
                                    IrcLine::Line(line) => line,
                                    IrcLine::Eof => break,
                                    IrcLine::TooLong => {
                                        // No resynchronisation point: the peer is mid-line
                                        // and its prefix is gone. ERROR + close is IRC's own
                                        // way of ending a link, and it names the limit so a
                                        // real client's operator can see what happened.
                                        error!(
                                            "IRC connection {} sent {} bytes with no newline \
                                             (limit {}), closing",
                                            connection_id, n, MAX_IRC_READ_LINE
                                        );
                                        let reply = format!(
                                            "ERROR :Closing link: message exceeds {MAX_IRC_READ_LINE} bytes\r\n"
                                        );
                                        let mut write = write_half_arc.lock().await;
                                        let _ = write.write_all(reply.as_bytes()).await;
                                        let _ = write.flush().await;
                                        let _ = write.shutdown().await;
                                        break;
                                    }
                                };
                                state_clone
                                    .update_connection_stats(
                                        server_id,
                                        connection_id,
                                        Some(n as u64),
                                        None,
                                        Some(1),
                                        None,
                                    )
                                    .await;

                                // DEBUG: Log summary with text preview.
                                // `truncate_for_log` cuts on a char boundary. Slicing with
                                // `&line[..100]` panicked here on any IRC line longer than 100
                                // bytes whose 100th byte fell inside a multi-byte character -
                                // i.e. on ordinary non-ASCII chat text - and a panic in this
                                // task is silent while the server still reads as Running.
                                let preview = crate::utils::truncate_for_log(&line, 100);
                                debug!(
                                    "IRC received {} bytes on connection {}: {}",
                                    n,
                                    connection_id,
                                    preview.trim()
                                );
                                let _ = status_clone.send(format!(
                                    "[DEBUG] IRC received {} bytes on connection {}: {}",
                                    n,
                                    connection_id,
                                    preview.trim()
                                ));

                                // TRACE: Log full text payload
                                trace!("IRC data (text): {:?}", line.trim());
                                let _ = status_clone
                                    .send(format!("[TRACE] IRC data (text): {:?}", line.trim()));

                                let event = Event::new(
                                    &IRC_MESSAGE_RECEIVED_EVENT,
                                    serde_json::json!({
                                        "message": line.trim()
                                    }),
                                );

                                debug!("IRC calling LLM for connection {}", connection_id);
                                let _ = status_clone.send(format!(
                                    "[DEBUG] IRC calling LLM for connection {}",
                                    connection_id
                                ));

                                match call_llm(
                                    &llm_clone,
                                    &state_clone,
                                    server_id,
                                    Some(connection_id),
                                    &event,
                                    protocol_clone.as_ref(),
                                )
                                .await
                                {
                                    Ok(execution_result) => {
                                        for message in &execution_result.messages {
                                            info!("{}", message);
                                            let _ =
                                                status_clone.send(format!("[INFO] {}", message));
                                        }

                                        debug!(
                                            "IRC got {} protocol results",
                                            execution_result.protocol_results.len()
                                        );
                                        let _ = status_clone.send(format!(
                                            "[DEBUG] IRC got {} protocol results",
                                            execution_result.protocol_results.len()
                                        ));

                                        for protocol_result in execution_result.protocol_results {
                                            match protocol_result {
                                                ActionResult::Output(data) => {
                                                    let response = String::from_utf8_lossy(&data);
                                                    let formatted = if response.ends_with("\r\n") {
                                                        response.to_string()
                                                    } else if response.ends_with('\n') {
                                                        format!("{response}\r")
                                                    } else {
                                                        format!("{response}\r\n")
                                                    };
                                                    {
                                                        let mut write = write_half_arc.lock().await;
                                                        let _ = write
                                                            .write_all(formatted.as_bytes())
                                                            .await;
                                                        let _ = write.flush().await;
                                                    }
                                                    state_clone
                                                        .update_connection_stats(
                                                            server_id,
                                                            connection_id,
                                                            None,
                                                            Some(formatted.len() as u64),
                                                            None,
                                                            Some(1),
                                                        )
                                                        .await;

                                                    // Same char-boundary hazard as the inbound
                                                    // preview above, but on model output.
                                                    let preview = crate::utils::truncate_for_log(
                                                        &formatted, 100,
                                                    );
                                                    debug!(
                                                        "IRC sent {} bytes on connection {}: {}",
                                                        formatted.len(),
                                                        connection_id,
                                                        preview.trim()
                                                    );
                                                    let _ = status_clone.send(format!("[DEBUG] IRC sent {} bytes on connection {}: {}", formatted.len(), connection_id, preview.trim()));

                                                    // TRACE: Log full text payload
                                                    trace!(
                                                        "IRC sent (text): {:?}",
                                                        formatted.trim()
                                                    );
                                                    let _ = status_clone.send(format!(
                                                        "[TRACE] IRC sent (text): {:?}",
                                                        formatted.trim()
                                                    ));
                                                }
                                                ActionResult::CloseConnection => break,
                                                _ => {}
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // Silence here is worse than it looks: a client that
                                        // has sent NICK/USER blocks until it gives up, because
                                        // registration only completes on numeric 001. 400
                                        // (ERR_UNKNOWNERROR) is the numeric reserved for
                                        // exactly this - a command the server understood and
                                        // could not carry out - and it can never be mistaken
                                        // for a registration, a JOIN or a PRIVMSG.
                                        error!(
                                            "IRC LLM call failed on connection {}: {}",
                                            connection_id, e
                                        );
                                        let command = irc_command_token(&line);
                                        let (reply, close) = irc_failure_reply(&command, &e);
                                        let _ = status_clone.send(format!(
                                            "[ERROR] IRC connection {} replying: {}",
                                            connection_id,
                                            reply.trim_end()
                                        ));
                                        {
                                            let mut write = write_half_arc.lock().await;
                                            let _ = write.write_all(reply.as_bytes()).await;
                                            let _ = write.flush().await;
                                            if close {
                                                let _ = write.shutdown().await;
                                            }
                                        }
                                        state_clone
                                            .update_connection_stats(
                                                server_id,
                                                connection_id,
                                                None,
                                                Some(reply.len() as u64),
                                                None,
                                                Some(1),
                                            )
                                            .await;
                                        if close {
                                            break;
                                        }
                                    }
                                }
                            }

                            // Connection closed - every exit path (EOF, read error,
                            // close_connection, backend-unavailable ERROR) lands here.
                            state_clone
                                .remove_peer_handle(server_id, connection_id.as_u32())
                                .await;
                            state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept IRC connection: {}", e);
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

/// The command token of an IRC message, uppercased, for echoing back in a numeric.
///
/// Handles the optional `:prefix` clients are allowed to send, and falls back to `*` for a
/// line with nothing usable in it - a numeric with a missing parameter is worse than a vague
/// one, because it changes the arity the client parses.
fn irc_command_token(line: &str) -> String {
    let trimmed = line.trim();
    let without_prefix = if let Some(rest) = trimmed.strip_prefix(':') {
        rest.split_once(' ').map(|(_, r)| r).unwrap_or("")
    } else {
        trimmed
    };
    let token: String = without_prefix
        .split_whitespace()
        .next()
        .unwrap_or("*")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if token.is_empty() {
        "*".to_string()
    } else {
        token.to_uppercase()
    }
}

/// What to send an IRC client when the LLM backend fails, and whether to close the link.
///
/// Numeric 400 (`ERR_UNKNOWNERROR`) is the code reserved for "an error the server has no more
/// specific numeric for", carrying the offending command as a parameter. It is not 421
/// (`ERR_UNKNOWNCOMMAND`), which would tell the client the command does not exist and stop it
/// ever retrying, and it is not any of the registration numerics, so a client waiting on 001
/// cannot mistake it for a completed registration.
///
/// A capacity failure is transient, so the link survives and the client may try again. Any
/// other failure means the server cannot answer anything at all, and IRC's honest way to say
/// that is `ERROR` followed by closing the link - which is also what unblocks a client that is
/// mid-registration instead of leaving it on a socket that will never speak again.
fn irc_failure_reply(command: &str, err: &anyhow::Error) -> (String, bool) {
    // The text is a category, never the error itself (`crate::utils::wire_failure`). A numeric
    // is one CRLF-terminated line whose trailing parameter runs to end of line, so an embedded
    // newline in an error would have forged a second message.
    let failure = crate::utils::WireFailure::classify(err);
    let text = failure.prefixed_text();
    if failure.is_overloaded() {
        (format!(":netget 400 * {command} :{text}\r\n"), false)
    } else {
        (
            format!(
                ":netget 400 * {command} :{text}\r\nERROR :Closing link: netget backend unavailable\r\n"
            ),
            true,
        )
    }
}
