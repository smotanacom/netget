//! POP3 server implementation
pub mod actions;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

#[cfg(feature = "pop3")]
use crate::console_debug;
#[cfg(feature = "pop3")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "pop3")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "pop3")]
use crate::llm::ActionResult;
#[cfg(feature = "pop3")]
use crate::protocol::Event;
#[cfg(feature = "pop3")]
use crate::server::Pop3Protocol;
#[cfg(feature = "pop3")]
use crate::state::app_state::AppState;
#[cfg(feature = "pop3")]
use actions::POP3_COMMAND_EVENT;
#[cfg(feature = "pop3")]
use tokio_rustls::TlsAcceptor;

/// POP3 server that forwards mail retrieval to LLM
pub struct Pop3Server;

#[cfg(feature = "pop3")]
impl Pop3Server {
    /// Spawn POP3 server with integrated LLM actions
    ///
    /// If tls_config is Some, the server will use implicit TLS (POP3S)
    /// If tls_config is None, the server will use plain text (POP3)
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        if tls_config.is_some() {
            info!(
                "POP3S server (TLS, action-based) listening on {}",
                local_addr
            );
        } else {
            info!(
                "POP3 server (plain, action-based) listening on {}",
                local_addr
            );
        }

        let protocol = Arc::new(Pop3Protocol::new());
        let tls_acceptor = tls_config.map(TlsAcceptor::from);

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id = crate::server::connection::ConnectionId::new(
                            app_state.get_next_unified_id().await,
                        );
                        // Captured before a TLS handshake could consume the TcpStream.
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        console_debug!(
                            status_tx,
                            "POP3 connection {} from {}",
                            connection_id,
                            remote_addr
                        );

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let tls_acceptor_clone = tls_acceptor.clone();

                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Optionally perform TLS handshake
                                if let Some(ref acceptor) = tls_acceptor_clone {
                                    match acceptor.accept(stream).await {
                                        Ok(tls_stream) => {
                                            debug!(
                                                "TLS handshake completed for connection {}",
                                                connection_id
                                            );
                                            let _ = status_clone.send(format!(
                                                "[DEBUG] TLS handshake completed for connection {}",
                                                connection_id
                                            ));
                                            if let Err(e) = Pop3Session::handle_session(
                                                tls_stream,
                                                connection_id,
                                                remote_addr,
                                                local_addr_conn,
                                                server_id,
                                                llm_clone,
                                                state_clone,
                                                status_clone,
                                                protocol_clone,
                                            )
                                            .await
                                            {
                                                error!(
                                                    "POP3S session error for connection {}: {}",
                                                    connection_id, e
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "TLS handshake failed for connection {}: {}",
                                                connection_id, e
                                            );
                                        }
                                    }
                                } else {
                                    // Plain text POP3
                                    if let Err(e) = Pop3Session::handle_session(
                                        stream,
                                        connection_id,
                                        remote_addr,
                                        local_addr_conn,
                                        server_id,
                                        llm_clone,
                                        state_clone,
                                        status_clone,
                                        protocol_clone,
                                    )
                                    .await
                                    {
                                        error!(
                                            "POP3 session error for connection {}: {}",
                                            connection_id, e
                                        );
                                    }
                                }
                            })
                            .await;
                    }
                    Err(e) => {
                        // Do not continue: an accept() error here is persistent (the listener
                        // is gone, or the process is out of descriptors), and looping on it
                        // spins a core at 100% while the server still reports Running.
                        error!("Failed to accept POP3 connection, stopping listener: {}", e);
                        let _ = status_tx.send(format!(
                            "[ERROR] POP3 listener stopped, failed to accept connection: {}",
                            e
                        ));
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

#[cfg(feature = "pop3")]
struct Pop3Session;

#[cfg(feature = "pop3")]
impl Pop3Session {
    /// Drive one POP3 session to completion.
    ///
    /// Generic over the transport so the plain TCP and POP3S (implicit TLS) paths share a
    /// single implementation - they were previously duplicated verbatim, which is how the
    /// two drifted.
    #[allow(clippy::too_many_arguments)]
    async fn handle_session<S>(
        stream: S,
        connection_id: crate::server::connection::ConnectionId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<Pop3Protocol>,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};

        let (read_half, write_half) = tokio::io::split(stream);
        let reader = tokio::io::BufReader::new(read_half);
        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

        // Track the connection so the dashboard lists it with live counters.
        let now = crate::utils::clock::Instant::now();
        app_state
            .add_connection_to_server(
                server_id,
                ConnectionState {
                    id: connection_id,
                    remote_addr,
                    local_addr,
                    bytes_sent: 0,
                    bytes_received: 0,
                    packets_sent: 0,
                    packets_received: 0,
                    last_activity: now,
                    status: ConnectionStatus::Active,
                    status_changed_at: now,
                    protocol_info: ProtocolConnectionInfo::empty(),
                },
            )
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Peer messaging: the dashboard's "message this peer" / "disconnect this peer" inject
        // actions into THIS connection through the same executor the LLM path uses. Registered
        // before the greeting, because a manual `*` rule can park that greeting for minutes and
        // the operator must be able to reach the connection while it waits.
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

        let result = Self::run_session(
            reader,
            &write_half,
            connection_id,
            server_id,
            &llm_client,
            &app_state,
            &status_tx,
            &protocol,
        )
        .await;

        // Every exit path - EOF, read error, close_connection, refused greeting - lands here.
        app_state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        app_state
            .close_connection_on_server(server_id, connection_id)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_session<R, W>(
        mut reader: tokio::io::BufReader<R>,
        write_half: &Arc<tokio::sync::Mutex<W>>,
        connection_id: crate::server::connection::ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<Pop3Protocol>,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        // Send initial greeting
        let greeting_event = Event::new(
            &POP3_COMMAND_EVENT,
            serde_json::json!({
                "command": "CONNECTION_ESTABLISHED",
                "connection_id": connection_id.to_string(),
            }),
        );

        match Self::process_command(
            &greeting_event,
            llm_client,
            app_state,
            status_tx,
            protocol,
            server_id,
            connection_id,
            write_half,
        )
        .await
        {
            Ok(SessionControl::Close) => return Ok(()),
            Ok(SessionControl::Continue) => {}
            Err(e) => {
                // A POP3 client waits for a banner before saying anything, so dropping the
                // socket here left it blocked until its own timeout. RFC 1939 allows the
                // greeting to be `-ERR`, and RFC 2449 gives the reason a machine-readable code.
                let token = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                    "fail_closed_llm_overloaded"
                } else {
                    "fail_closed_llm_error"
                };
                error!(
                    "POP3 greeting on connection {} decision={}: {}",
                    connection_id, token, e
                );
                let _ = status_tx.send(format!(
                    "[ERROR] POP3 greeting on connection {} decision={}",
                    connection_id, token
                ));
                let reply = pop3_failure_reply(&e);
                let _ = status_tx.send(format!(
                    "[ERROR] POP3 connection {} refused: {}",
                    connection_id,
                    reply.trim_end()
                ));
                let mut writer = write_half.lock().await;
                let _ = writer.write_all(reply.as_bytes()).await;
                let _ = writer.flush().await;
                drop(writer);
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        None,
                        Some(reply.len() as u64),
                        None,
                        Some(1),
                    )
                    .await;
                return Ok(());
            }
        }

        // Main command loop
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    debug!("POP3 connection {} closed by client", connection_id);
                    break;
                }
                Ok(n) => {
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
                    let command = line.trim().to_string();
                    if command.is_empty() {
                        continue;
                    }
                    // The verb alone, for log lines. Kept separately so nothing downstream
                    // has to re-derive it from a `command` the event may have consumed.
                    let verb = command
                        .split_whitespace()
                        .next()
                        .unwrap_or("?")
                        .to_uppercase();

                    console_debug!(
                        status_tx,
                        "POP3 connection {} received: {}",
                        connection_id,
                        command
                    );

                    let event = Event::new(
                        &POP3_COMMAND_EVENT,
                        serde_json::json!({
                            "command": command,
                            "connection_id": connection_id.to_string(),
                        }),
                    );

                    match Self::process_command(
                        &event,
                        llm_client,
                        app_state,
                        status_tx,
                        protocol,
                        server_id,
                        connection_id,
                        write_half,
                    )
                    .await
                    {
                        Ok(SessionControl::Continue) => {}
                        Ok(SessionControl::Close) => {
                            debug!("POP3 connection {} closed by server", connection_id);
                            break;
                        }
                        Err(e) => {
                            // Answer before hanging up. Silence here is indistinguishable from
                            // a hung server, and for USER/PASS it is worse than that: the
                            // client cannot tell a refused login from a lost connection.
                            // `-ERR` is a refusal on every command POP3 has, so this fails
                            // closed by construction - there is no `+OK` on this path.
                            // `-ERR`, always. A backend outage and a model that denied the
                            // login are indistinguishable on this wire, so the token is
                            // what keeps them apart in the log.
                            let token = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                                "fail_closed_llm_overloaded"
                            } else {
                                "fail_closed_llm_error"
                            };
                            error!(
                                "POP3 {} on connection {} decision={}: {}",
                                verb, connection_id, token, e
                            );
                            let _ = status_tx.send(format!(
                                "[ERROR] POP3 {} on connection {} decision={}",
                                verb, connection_id, token
                            ));
                            let reply = pop3_failure_reply(&e);
                            let _ = status_tx.send(format!(
                                "[ERROR] POP3 connection {} replying: {}",
                                connection_id,
                                reply.trim_end()
                            ));
                            let mut writer = write_half.lock().await;
                            let _ = writer.write_all(reply.as_bytes()).await;
                            let _ = writer.flush().await;
                            drop(writer);
                            app_state
                                .update_connection_stats(
                                    server_id,
                                    connection_id,
                                    None,
                                    Some(reply.len() as u64),
                                    None,
                                    Some(1),
                                )
                                .await;
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("POP3 connection {} read error: {}", connection_id, e);
                    break;
                }
            }
        }

        Ok(())
    }

    async fn process_command<W>(
        event: &Event,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<Pop3Protocol>,
        server_id: crate::state::ServerId,
        connection_id: crate::server::connection::ConnectionId,
        write_half: &Arc<tokio::sync::Mutex<W>>,
    ) -> Result<SessionControl>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        // The command this answer belongs to, for the decision line. Without it a
        // `decision=` tag has no subject, and "which command was refused" is the only
        // question worth asking of a POP3 log.
        let command = event
            .data
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .split_whitespace()
            .next()
            .unwrap_or("?")
            .to_uppercase();

        // Call LLM for action.
        //
        // The `?` hands an LLM failure back to `run_session`, which answers `-ERR` and
        // logs `decision=fail_closed_llm_*`. **Nothing on that path can produce `+OK`**:
        // the only `+OK` in this protocol comes from an action the model named, and
        // `pop3_failure_reply` is `-ERR` in every branch. That is what stops a backend
        // outage from becoming a granted mailbox — the OAuth2 failure mode.
        let llm_result = call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            event,
            protocol.as_ref(),
        )
        .await?;
        let failures = llm_result.failures.len();

        // Execute actions. `close_connection` must still flush everything queued before it -
        // the QUIT reply is normally `send_pop3_ok` followed by `close_connection` in the same
        // batch - so record the intent and act on it once the batch is drained.
        let mut control = SessionControl::Continue;
        // What the model actually put on the wire. POP3 says yes and no with the same
        // action mechanism, so the leading token of the first reply is the only honest
        // way to tell an approval from a refusal — and they must never be conflated.
        let mut first_reply: Option<String> = None;
        let mut asked_to_close = false;

        for action in llm_result.protocol_results {
            match action {
                ActionResult::Output(data) => {
                    if first_reply.is_none() {
                        first_reply =
                            Some(String::from_utf8_lossy(&data[..data.len().min(8)]).to_string());
                    }
                    let mut writer = write_half.lock().await;
                    writer.write_all(&data).await?;
                    writer.flush().await?;
                    drop(writer);
                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            None,
                            Some(data.len() as u64),
                            None,
                            Some(1),
                        )
                        .await;

                    console_debug!(
                        status_tx,
                        "POP3 connection {} sent {} bytes",
                        connection_id,
                        data.len()
                    );
                }
                ActionResult::CloseConnection => {
                    control = SessionControl::Close;
                    asked_to_close = true;
                }
                ActionResult::WaitForMore => {
                    // Do nothing, wait for next command
                }
                _ => {
                    // Not an action that produces POP3 output (memory updates, logging, ...)
                }
            }
        }

        // One decision line per command, so `grep decision=` separates a granted request
        // from a refused one from a request nobody answered.
        let decision = match first_reply.as_deref() {
            Some(reply) if reply.starts_with("-ERR") => "model_reject",
            Some(_) => "model_answer",
            // A `close_connection` with no reply is the model hanging up rather than
            // answering — a deliberate refusal, not a failure.
            None if asked_to_close => "model_reject",
            None if failures == 0 => "model_silent",
            None => "fail_closed_bad_action",
        };
        let summary = format!(
            "POP3 {} on connection {} decision={} ({} failed action(s))",
            command, connection_id, decision, failures
        );
        match decision {
            // Nothing was written and nothing asked to close: the client is still waiting
            // for a reply it will never get. No `+OK` was produced, so no mailbox was
            // opened — but the peer is owed an answer it did not receive.
            "model_silent" => {
                tracing::warn!("{}", summary);
                let _ = status_tx.send(format!("[WARN] {}", summary));
            }
            "fail_closed_bad_action" => {
                error!("{}", summary);
                let _ = status_tx.send(format!("[ERROR] {}", summary));
            }
            _ => {
                info!("{}", summary);
                let _ = status_tx.send(format!("[INFO] {}", summary));
            }
        }

        Ok(control)
    }
}

/// The `-ERR` line to write when the LLM backend fails, RFC 2449 extended response code
/// included so the client can tell "come back later" from "this is broken".
///
/// `[SYS/TEMP]` is the retryable one and is reserved for capacity exhaustion - the same split
/// HTTP makes between 503 and 500. Everything else gets `[SYS/PERM]`, which does not invite an
/// immediate retry loop against a backend that is down.
///
/// Every branch is `-ERR`. POP3 has no response that both refuses and looks like success, so
/// there is no way for this path to authenticate anybody or hand out a message.
#[cfg(feature = "pop3")]
fn pop3_failure_reply(err: &anyhow::Error) -> String {
    // The text is a category, never the error itself (`crate::utils::wire_failure`). The reply
    // is one line, so a newline in an error would have forged a second response, and a leading
    // `.` would have terminated a multiline block.
    let failure = crate::utils::WireFailure::classify(err);
    let text = failure.prefixed_text();
    if failure.is_overloaded() {
        format!("-ERR [SYS/TEMP] {text}\r\n")
    } else {
        format!("-ERR [SYS/PERM] {text}\r\n")
    }
}

/// Whether the command loop should keep reading or shut the connection down.
///
/// `process_command` used to signal a close by returning `Ok(())` early, which is
/// indistinguishable from "command handled" - so `close_connection` never actually closed
/// anything and a client that sent QUIT was left holding an open socket.
#[cfg(feature = "pop3")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionControl {
    Continue,
    Close,
}
