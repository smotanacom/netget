//! NNTP server implementation
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::NntpProtocol;
use crate::state::app_state::AppState;
use actions::NNTP_COMMAND_RECEIVED_EVENT;

/// NNTP server that forwards commands to LLM
pub struct NntpServer;

impl NntpServer {
    /// Spawn NNTP server with integrated LLM actions
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
        Log::new(Some(&status_tx)).info(format!("NNTP server listening on {}", local_addr));

        let protocol = Arc::new(NntpProtocol::new());

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
                            Self::handle_connection(
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
                            .await;
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept NNTP connection: {}", e));
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Track one connection, register its peer handle, run the session, and clean up on
    /// every exit path.
    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        stream: tokio::net::TcpStream,
        connection_id: ConnectionId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<NntpProtocol>,
    ) {
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };

        let (read_half, write_half) = tokio::io::split(stream);
        let write_half_arc = Arc::new(tokio::sync::Mutex::new(write_half));

        // Add connection to ServerInstance
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
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
        };
        app_state
            .add_connection_to_server(server_id, conn_state)
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
            write_half_arc.clone(),
            status_tx.clone(),
        );

        Self::run_session(
            BufReader::new(read_half),
            &write_half_arc,
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
            .remove_connection_from_server(server_id, connection_id)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Write one buffer to the peer and count it.
    async fn write_counted<W>(
        write_half: &Arc<tokio::sync::Mutex<W>>,
        app_state: &AppState,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        data: &[u8],
        shutdown: bool,
    ) where
        W: tokio::io::AsyncWrite + Unpin,
    {
        {
            let mut write = write_half.lock().await;
            let _ = write.write_all(data).await;
            let _ = write.flush().await;
            if shutdown {
                let _ = write.shutdown().await;
            }
        }
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
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_session<R, W>(
        mut reader: BufReader<R>,
        write_half_arc: &Arc<tokio::sync::Mutex<W>>,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<NntpProtocol>,
    ) where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let log = Log::new(Some(status_tx));

        // Send initial greeting
        log.debug(format!(
            "NNTP sending greeting to connection {}",
            connection_id
        ));

        let greeting_event = Event::new(
            &NNTP_COMMAND_RECEIVED_EVENT,
            serde_json::json!({
                "command": "GREETING"
            }),
        );
        match call_llm(
            llm_client,
            app_state,
            server_id,
            Some(connection_id),
            &greeting_event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                for message in &execution_result.messages {
                    log.info(message);
                }

                let plan = SessionWrite::collect(&execution_result.protocol_results);

                for data in &plan.outputs {
                    Self::write_counted(
                        write_half_arc,
                        app_state,
                        server_id,
                        connection_id,
                        data,
                        false,
                    )
                    .await;

                    // Sent summary + payload are FileOnly.
                    let response = String::from_utf8_lossy(data);
                    let preview = crate::utils::truncate_for_log(&response, 100);
                    log.debug(format!(
                        "NNTP sent {} bytes on connection {} (decision={}): {}",
                        data.len(),
                        connection_id,
                        nntp_model_decision(data),
                        preview.trim()
                    ));
                    log.trace(format!("NNTP sent (text): {:?}", response.trim()));
                }

                // A greeting is mandatory: RFC 3977 5.1 has the server speak
                // first, and a client reads that line before it sends
                // anything. So an answer with no greeting in it is not
                // "say nothing", it is a session that can never start - the
                // client blocks forever on a line that is not coming. The
                // Err branch below already answers 400; this covers the
                // model (or a static/manual handler) returning zero actions,
                // or actions that all failed to execute, which reaches here
                // as Ok with nothing to write. An explicit wait_for_more or
                // close_connection is a deliberate choice and is left alone.
                if plan.wrote_nothing() && !plan.silence_is_deliberate {
                    let decision = if execution_result.failures.is_empty() {
                        "fail_closed_no_actions"
                    } else {
                        "fail_closed_action_error"
                    };
                    let line = nntp_greeting_failure_line_static();
                    log.warn(format!(
                        "NNTP greeting produced no response on connection {} \
                         (decision={}, refused: {}): {}",
                        connection_id,
                        decision,
                        line.trim_end(),
                        nntp_failure_detail(&execution_result)
                    ));
                    Self::write_counted(
                        write_half_arc,
                        app_state,
                        server_id,
                        connection_id,
                        line.as_bytes(),
                        true,
                    )
                    .await;
                    return;
                }

                if plan.close {
                    return;
                }
            }
            Err(e) => {
                // RFC 3977 5.1: the initial greeting may be `400 service
                // temporarily unavailable`, after which the server closes.
                // Writing nothing left the client blocked reading a
                // greeting line that was never coming.
                let line = nntp_greeting_failure_line(&e);
                log.warn(format!(
                    "NNTP greeting LLM error on connection {} \
                     (decision=fail_closed_llm_error, refused: {}): {}",
                    connection_id,
                    line.trim_end(),
                    e
                ));
                Self::write_counted(
                    write_half_arc,
                    app_state,
                    server_id,
                    connection_id,
                    line.as_bytes(),
                    true,
                )
                .await;
                return;
            }
        }

        // Read commands from client
        let mut line = String::new();

        while let Ok(n) = reader.read_line(&mut line).await {
            if n == 0 {
                break;
            }
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

            // Summary + payload FileOnly: the nntp_command_received
            // event template surfaces the command to the TUI.
            let preview = crate::utils::truncate_for_log(&line, 100);
            log.debug(format!(
                "NNTP received {} bytes on connection {}: {}",
                n,
                connection_id,
                preview.trim()
            ));
            log.trace(format!("NNTP data (text): {:?}", line.trim()));

            let event = Event::new(
                &NNTP_COMMAND_RECEIVED_EVENT,
                serde_json::json!({
                    "command": line.trim()
                }),
            );

            log.debug(format!("NNTP calling LLM for connection {}", connection_id));

            match call_llm(
                llm_client,
                app_state,
                server_id,
                Some(connection_id),
                &event,
                protocol.as_ref(),
            )
            .await
            {
                Ok(execution_result) => {
                    for message in &execution_result.messages {
                        log.info(message);
                    }

                    log.debug(format!(
                        "NNTP got {} protocol results",
                        execution_result.protocol_results.len()
                    ));

                    let plan = SessionWrite::collect(&execution_result.protocol_results);

                    // NNTP is one response line per command, so writing
                    // nothing desynchronises the client for the rest of the
                    // session - it waits forever for a line that will never
                    // come, and if it ever gives up and sends another
                    // command it reads the next reply as the answer to this
                    // one. Two Ok cases land here with nothing to write and
                    // both get 403: actions that failed to execute (a
                    // missing required field, say) and an answer with no
                    // actions in it at all - an empty model reply, a manual
                    // "answer with nothing", a zero-action static handler.
                    // The empty case is the worse of the two, not the
                    // exempt one. A deliberate wait_for_more or
                    // close_connection is left alone.
                    if plan.wrote_nothing() && !plan.silence_is_deliberate {
                        let decision = if execution_result.failures.is_empty() {
                            "fail_closed_no_actions"
                        } else {
                            "fail_closed_action_error"
                        };
                        // The peer gets a category; the detail goes to the
                        // log. Action names and executor error strings are
                        // netget's internals and are not the peer's to read.
                        let reply = nntp_command_failure_line_static();
                        log.warn(format!(
                            "NNTP produced no response on connection {} \
                             (decision={}, replying: {}): {}",
                            connection_id,
                            decision,
                            reply.trim_end(),
                            nntp_failure_detail(&execution_result)
                        ));
                        Self::write_counted(
                            write_half_arc,
                            app_state,
                            server_id,
                            connection_id,
                            reply.as_bytes(),
                            false,
                        )
                        .await;
                    }

                    for data in &plan.outputs {
                        Self::write_counted(
                            write_half_arc,
                            app_state,
                            server_id,
                            connection_id,
                            data,
                            false,
                        )
                        .await;

                        // Sent summary + payload are FileOnly.
                        let response = String::from_utf8_lossy(data);
                        let preview = crate::utils::truncate_for_log(&response, 100);
                        log.debug(format!(
                            "NNTP sent {} bytes on connection {} (decision={}): {}",
                            data.len(),
                            connection_id,
                            nntp_model_decision(data),
                            preview.trim()
                        ));
                        log.trace(format!("NNTP sent (text): {:?}", response.trim()));
                    }

                    if plan.close {
                        break;
                    }
                }
                Err(e) => {
                    // NNTP is strictly one response line per command, so
                    // silence desynchronises the client for the rest of
                    // the session. 403 is RFC 3977's "internal fault or
                    // problem preventing action being taken" and is a
                    // refusal on every command NNTP has, AUTHINFO
                    // included - there is no way for this path to
                    // authenticate anyone or hand back an article.
                    let (reply, close) = nntp_command_failure_line(&e);
                    log.warn(format!(
                        "NNTP LLM error on connection {} \
                         (decision=fail_closed_llm_error, replying: {}): {}",
                        connection_id,
                        reply.trim_end(),
                        e
                    ));
                    Self::write_counted(
                        write_half_arc,
                        app_state,
                        server_id,
                        connection_id,
                        reply.as_bytes(),
                        close,
                    )
                    .await;
                    if close {
                        break;
                    }
                }
            }

            line.clear();
        }

        log.info(format!("NNTP connection {} closed", connection_id));
    }
}

/// What a batch of action results means for the wire.
///
/// Collected up front so the "nothing to say" case can be recognised *before* anything is
/// written, and so a nested `Multiple` still counts as a response — matching on the top-level
/// variants alone would read `Multiple(vec![Output(..)])` as silence and answer 403 over a
/// perfectly good reply.
#[derive(Default)]
struct SessionWrite {
    /// Bytes to put on the wire, in order.
    outputs: Vec<Vec<u8>>,
    /// The session should end after those bytes.
    close: bool,
    /// The answer contained an explicit `wait_for_more` or `close_connection`, so writing
    /// nothing is a choice rather than a hole.
    silence_is_deliberate: bool,
}

impl SessionWrite {
    fn collect(results: &[ActionResult]) -> Self {
        let mut plan = Self::default();
        plan.absorb(results);
        plan
    }

    fn absorb(&mut self, results: &[ActionResult]) {
        for result in results {
            match result {
                ActionResult::Output(data) => self.outputs.push(data.clone()),
                ActionResult::CloseConnection => {
                    self.close = true;
                    self.silence_is_deliberate = true;
                }
                ActionResult::WaitForMore => self.silence_is_deliberate = true,
                ActionResult::Multiple(inner) => self.absorb(inner),
                _ => {}
            }
        }
    }

    fn wrote_nothing(&self) -> bool {
        self.outputs.is_empty()
    }
}

/// Log tag for a response the model itself produced: a 4xx/5xx status line is the model
/// refusing, anything else is the model answering. Keeping the two apart in the log is what
/// makes a real denial distinguishable from netget's own fail-closed replies, which are
/// tagged `decision=fail_closed_*`.
fn nntp_model_decision(data: &[u8]) -> &'static str {
    match data.first() {
        Some(b'4') | Some(b'5') => "model_reject",
        _ => "model_answer",
    }
}

/// Everything about a failed answer that belongs in the log and nowhere else.
fn nntp_failure_detail(result: &crate::llm::actions::executor::ExecutionResult) -> String {
    if result.failures.is_empty() {
        "the answer contained no actions".to_string()
    } else {
        result
            .failures
            .iter()
            .map(|f| format!("{}: {}", f.action, f.error))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// The greeting to write when the answer contains no greeting at all.
///
/// There is no error to classify here — nothing failed, the answer was simply empty — so this
/// is the flat `Unavailable` category. 400 is the same legal failure greeting the LLM-error
/// path uses, and the server closes after it.
fn nntp_greeting_failure_line_static() -> String {
    format!(
        "400 {}\r\n",
        crate::utils::WireFailure::Unavailable.prefixed_text()
    )
}

/// The command reply to write when the answer contains nothing to send.
///
/// 403 leaves the session usable, which is right for a per-command hole. The free text is a
/// fixed category: action names and executor error strings name netget's internals and never
/// reach the peer.
fn nntp_command_failure_line_static() -> String {
    format!(
        "403 {}\r\n",
        crate::utils::WireFailure::Unavailable.prefixed_text()
    )
}

/// The greeting to write when the LLM backend cannot produce one.
///
/// RFC 3977 5.1 defines exactly two failure greetings: 400 (temporarily unavailable) and 502
/// (permanently unavailable). Neither can be mistaken for the 200/201 that means "ready", so
/// a client can never read this as a usable session.
fn nntp_greeting_failure_line(err: &anyhow::Error) -> String {
    // The free text is a category, never the error itself (`crate::utils::wire_failure`),
    // which also makes it impossible for a newline in an error to forge a second response
    // line and desynchronise the session.
    format!(
        "400 {}\r\n",
        crate::utils::WireFailure::classify(err).prefixed_text()
    )
}

/// The response line for a command the LLM backend failed to answer, and whether to close.
///
/// 403 is RFC 3977's generic "internal fault or problem preventing action being taken" and
/// leaves the session usable, which is what a one-off failure deserves. Capacity exhaustion
/// gets 400 instead - "service discontinued", which per RFC 3977 3.1 the server follows by
/// closing the connection - because telling a client to come back later is more useful than
/// letting it hammer a backend that is already saturated.
fn nntp_command_failure_line(err: &anyhow::Error) -> (String, bool) {
    let failure = crate::utils::WireFailure::classify(err);
    let text = failure.prefixed_text();
    if failure.is_overloaded() {
        (format!("400 {text}\r\n"), true)
    } else {
        (format!("403 {text}\r\n"), false)
    }
}
