//! SMTP server implementation
pub mod actions;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::console_debug;
#[cfg(feature = "smtp")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "smtp")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "smtp")]
use crate::llm::ActionResult;
#[cfg(feature = "smtp")]
use crate::logging::emit::Log;
#[cfg(feature = "smtp")]
use crate::protocol::Event;
#[cfg(feature = "smtp")]
use crate::server::SmtpProtocol;
#[cfg(feature = "smtp")]
use crate::state::app_state::AppState;
#[cfg(feature = "smtp")]
use actions::SMTP_COMMAND_EVENT;
#[cfg(feature = "smtp")]
use tokio_rustls::TlsAcceptor;

/// The longest single line this server will buffer, in bytes.
///
/// RFC 5321 §4.5.3.1 sets the minimum a server must accept at 512 octets for a command and
/// 1000 for a `DATA` text line, and invites larger. 64 KiB is far above anything legitimate
/// and far below what it costs us to refuse.
///
/// The cap is the point. `BufReader::read_line` grows its `String` until it finds a `\n`, so
/// a peer that connects and streams bytes without ever sending one allocates until the
/// process dies — one unauthenticated socket, no credentials, no protocol state.
#[cfg(feature = "smtp")]
const MAX_LINE_BYTES: usize = 64 * 1024;

/// How long a session waits for the next line before it gives up on the peer.
///
/// RFC 5321 §4.5.3.2 sets the server-side "command" timeout at 5 minutes. Without one, a peer
/// that connects and then says nothing holds a connection task, a socket and its slot in the
/// connection map for as long as the process runs.
#[cfg(feature = "smtp")]
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The outcome of one bounded read.
#[cfg(feature = "smtp")]
enum LineRead {
    Line(String),
    /// The peer closed, or sent a partial line and then closed.
    Eof,
    /// `MAX_LINE_BYTES` went by without a `\n`.
    TooLong,
    /// `READ_TIMEOUT` went by without a byte.
    Timeout,
}

/// Read one `\n`-terminated line, bounded in both length and time.
///
/// Two deliberate differences from `BufReader::read_line`:
///
/// * It stops at `MAX_LINE_BYTES` instead of growing without bound.
/// * It decodes lossily instead of failing on invalid UTF-8. `read_line` returns
///   `ErrorKind::InvalidData` for any non-UTF-8 byte and the session died on it — while
///   `execute_send_smtp_ehlo` advertises `8BITMIME` by default, so the server was promising a
///   capability its own read path could not survive. One Latin-1 byte in a message body was
///   enough. These bytes only ever become the model's event payload and a log line; nothing
///   here is re-emitted on the wire, so a lossy decode loses nothing a peer can observe.
///
/// A partial line at EOF is reported as `Eof`, not as a line: half a command is not a command.
#[cfg(feature = "smtp")]
async fn read_line_bounded<R>(reader: &mut tokio::io::BufReader<R>) -> Result<LineRead>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = match tokio::time::timeout(READ_TIMEOUT, reader.fill_buf()).await {
            Err(_elapsed) => return Ok(LineRead::Timeout),
            Ok(result) => result?,
        };
        if chunk.is_empty() {
            return Ok(LineRead::Eof);
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                if buf.len() + idx + 1 > MAX_LINE_BYTES {
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(&chunk[..=idx]);
                reader.consume(idx + 1);
                return Ok(LineRead::Line(String::from_utf8_lossy(&buf).into_owned()));
            }
            None => {
                let taken = chunk.len();
                if buf.len() + taken > MAX_LINE_BYTES {
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(chunk);
                reader.consume(taken);
            }
        }
    }
}

/// SMTP server that forwards mail to LLM
pub struct SmtpServer;

#[cfg(feature = "smtp")]
impl SmtpServer {
    /// Spawn SMTP server with integrated LLM actions
    ///
    /// If tls_config is Some, the server will use implicit TLS (SMTPS)
    /// If tls_config is None, the server will use plain text (SMTP)
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
            Log::new(Some(&status_tx)).info(format!(
                "SMTPS server (TLS, action-based) listening on {}",
                local_addr
            ));
        } else {
            Log::new(Some(&status_tx)).info(format!(
                "SMTP server (plain, action-based) listening on {}",
                local_addr
            ));
        }

        let protocol = Arc::new(SmtpProtocol::new());
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
                            "SMTP connection {} from {}",
                            connection_id,
                            remote_addr
                        );

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let tls_acceptor_clone = tls_acceptor.clone();

                        tokio::spawn(async move {
                            // Optionally perform TLS handshake
                            if let Some(ref acceptor) = tls_acceptor_clone {
                                match acceptor.accept(stream).await {
                                    Ok(tls_stream) => {
                                        Log::new(Some(&status_clone)).debug(format!(
                                            "TLS handshake completed for connection {}",
                                            connection_id
                                        ));
                                        if let Err(e) = SmtpSession::handle_session(
                                            tls_stream,
                                            connection_id,
                                            remote_addr,
                                            local_addr_conn,
                                            server_id,
                                            llm_clone,
                                            state_clone,
                                            status_clone.clone(),
                                            protocol_clone,
                                        )
                                        .await
                                        {
                                            Log::new(Some(&status_clone))
                                                .error(format!("SMTP session error: {}", e));
                                        }
                                    }
                                    Err(e) => {
                                        Log::new(Some(&status_clone)).error(format!(
                                            "TLS handshake failed for connection {}: {}",
                                            connection_id, e
                                        ));
                                    }
                                }
                            } else {
                                if let Err(e) = SmtpSession::handle_session(
                                    stream,
                                    connection_id,
                                    remote_addr,
                                    local_addr_conn,
                                    server_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone.clone(),
                                    protocol_clone,
                                )
                                .await
                                {
                                    Log::new(Some(&status_clone))
                                        .error(format!("SMTP session error: {}", e));
                                }
                            };
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept SMTP connection: {}", e);
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

#[cfg(feature = "smtp")]
struct SmtpSession;

#[cfg(feature = "smtp")]
impl SmtpSession {
    /// Handle one SMTP session, plain or SMTPS.
    ///
    /// Generic over the transport: the plain and TLS paths were previously two verbatim
    /// copies of the same greeting-then-command-loop code.
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
        protocol: Arc<SmtpProtocol>,
    ) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};

        let (read_half, write_half) = tokio::io::split(stream);
        let reader = tokio::io::BufReader::new(read_half);
        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

        // Track the connection so the dashboard lists it with live counters.
        let now = std::time::Instant::now();
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
        protocol: &Arc<SmtpProtocol>,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        // Send initial greeting. `false` means the model refused the connection; close
        // without entering the command loop.
        if !Self::send_greeting(
            write_half,
            connection_id,
            server_id,
            llm_client,
            app_state,
            status_tx,
            protocol,
        )
        .await?
        {
            return Ok(());
        }

        loop {
            let line = match read_line_bounded(&mut reader).await? {
                LineRead::Line(line) => line,
                LineRead::Eof => break,
                LineRead::TooLong => {
                    // RFC 5321 §4.5.3.1 sets the line limits; 500 5.5.2 is the reply for
                    // breaking them. Close afterwards: we stopped reading mid-line, so the
                    // rest of that line would be parsed as fresh commands.
                    warn!(
                        "SMTP connection {} sent a line over {} bytes with no newline; closing",
                        connection_id, MAX_LINE_BYTES
                    );
                    Self::write_counted(
                        write_half,
                        b"500 5.5.2 Line too long\r\n",
                        connection_id,
                        server_id,
                        app_state,
                        status_tx,
                    )
                    .await?;
                    break;
                }
                LineRead::Timeout => {
                    // RFC 5321 §4.5.3.2: the server-side command timeout. 421 is the code for
                    // "service closing transmission channel", which is exactly what happens.
                    warn!(
                        "SMTP connection {} idle for {}s; closing",
                        connection_id,
                        READ_TIMEOUT.as_secs()
                    );
                    Self::write_counted(
                        write_half,
                        b"421 4.4.2 Timeout waiting for command\r\n",
                        connection_id,
                        server_id,
                        app_state,
                        status_tx,
                    )
                    .await?;
                    break;
                }
            };
            let n = line.len();
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

            let command = line.trim();
            console_debug!(status_tx, "SMTP received: {}", command);

            let event = Event::new(
                &SMTP_COMMAND_EVENT,
                serde_json::json!({
                    "command": command
                }),
            );

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
                    let mut should_close = false;
                    let mut wrote_reply = false;
                    let mut silence_is_deliberate = false;

                    for protocol_result in execution_result.protocol_results {
                        match protocol_result {
                            ActionResult::Output(data) => {
                                wrote_reply = true;
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

                                let response = String::from_utf8_lossy(&data);
                                console_debug!(status_tx, "SMTP sent: {}", response.trim());
                            }
                            // Do not return here: the QUIT reply is normally a 221 followed by
                            // close_connection in the same batch, and returning early would
                            // drop the 221 when the ordering came back reversed.
                            ActionResult::CloseConnection => should_close = true,
                            // `wait_for_more` is the model saying "send nothing and read the
                            // next line" - correct and required during DATA, where SMTP
                            // expects no per-line reply. It is the one answer that makes
                            // silence right, which is exactly why it has to be told apart
                            // from an empty answer below.
                            ActionResult::WaitForMore => silence_is_deliberate = true,
                            _ => {}
                        }
                    }

                    if should_close {
                        return Ok(());
                    }

                    // A model that answered with nothing at all used to leave the peer
                    // blocked until its own timeout - the same defect the `Err` arm's 451
                    // exists to remove, reached through the success path instead. SMTP is not
                    // one of the deliberately-silent protocols: every command owes a reply,
                    // and `wait_for_more` is already the way to decline one. So an empty
                    // answer is a failure and gets the same 451, and the log says which of
                    // the two it was.
                    if !wrote_reply && !silence_is_deliberate {
                        Log::new(Some(status_tx)).error(format!(
                            "SMTP connection {} decision=model_silent for {:?}: the model \
                             produced no reply; answering 451",
                            connection_id,
                            crate::utils::truncate_for_log(command, 200)
                        ));
                        Self::write_counted(
                            write_half,
                            b"451 4.3.0 Temporary local error, try again later\r\n",
                            connection_id,
                            server_id,
                            app_state,
                            status_tx,
                        )
                        .await?;
                    }
                }
                Err(e) => {
                    // Answer 451 rather than writing nothing. SMTP has a whole 4xx class
                    // meaning "temporary, retry later" (RFC 5321 §4.2.1), which is exactly
                    // what an unavailable backend is, and a client that gets one requeues the
                    // message instead of blocking until its own timeout and then bouncing it.
                    //
                    // It also fails closed: a 4xx is never mistaken for acceptance, so an
                    // outage cannot silently look like a delivered message.
                    Log::new(Some(status_tx)).error(format!(
                        "SMTP connection {} got no response for {:?}: {:#}",
                        connection_id, command, e
                    ));

                    // 4.3.2 is "system not accepting network messages" (RFC 3463), which is
                    // the truthful enhanced code for capacity exhaustion; 4.3.0 covers the
                    // rest.
                    let reply: &[u8] = if crate::llm::is_overload_error(&e) {
                        warn!(
                            "SMTP 451 on connection {}: LLM capacity exhausted",
                            connection_id
                        );
                        b"451 4.3.2 Backend at capacity, try again later\r\n"
                    } else {
                        b"451 4.3.0 Temporary local error, try again later\r\n"
                    };
                    let mut writer = write_half.lock().await;
                    writer.write_all(reply).await?;
                    writer.flush().await?;
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
                    console_debug!(
                        status_tx,
                        "SMTP sent: {}",
                        String::from_utf8_lossy(reply).trim()
                    );
                }
            }
        }

        Ok(())
    }

    /// Write a fixed reply and account for it, dropping the writer guard before returning.
    ///
    /// The byte slice is `&'static [u8]` on purpose: everything this is used for is a protocol
    /// constant, and a `&[u8]` parameter would be one `format!` away from putting an internal
    /// error string on a stranger's terminal — the defect `crate::utils::wire_failure` exists
    /// to prevent.
    async fn write_counted<W>(
        write_half: &Arc<tokio::sync::Mutex<W>>,
        reply: &'static [u8],
        connection_id: crate::server::connection::ConnectionId,
        server_id: crate::state::ServerId,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        {
            let mut writer = write_half.lock().await;
            writer.write_all(reply).await?;
            writer.flush().await?;
        }
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
        console_debug!(
            status_tx,
            "SMTP sent: {}",
            String::from_utf8_lossy(reply).trim()
        );
        Ok(())
    }

    /// Send the greeting.
    ///
    /// Returns `Ok(false)` when the model answered the greeting event with `close_connection`
    /// — a deliberate refusal, which is a different thing from a backend failure and must not
    /// be reported as one. `Err` is reserved for the backend actually failing, and carries the
    /// 421 that has already been written.
    async fn send_greeting<W>(
        write_half: &Arc<tokio::sync::Mutex<W>>,
        connection_id: crate::server::connection::ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<SmtpProtocol>,
    ) -> Result<bool>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;

        let greeting_event = Event::new(
            &SMTP_COMMAND_EVENT,
            serde_json::json!({
                "command": "CONNECTION_ESTABLISHED"
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
                let mut refused = false;
                for protocol_result in execution_result.protocol_results {
                    match protocol_result {
                        ActionResult::Output(data) => {
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
                        }
                        // `close_connection` is advertised on `smtp_command`, and the greeting
                        // *is* an `smtp_command` event, so refusing the connection is an answer
                        // the model is explicitly offered. This arm used to be an
                        // `if let ActionResult::Output(..)`, which dropped the refusal on the
                        // floor and carried on into the command loop on a connection the model
                        // had declined - a denial that did not deny.
                        ActionResult::CloseConnection => refused = true,
                        _ => {}
                    }
                }
                if refused {
                    Log::new(Some(status_tx)).info(format!(
                        "SMTP connection {} decision=model_reject: the model refused the \
                         connection at the greeting",
                        connection_id
                    ));
                    return Ok(false);
                }
            }
            Err(e) => {
                // No banner means no session. RFC 5321 §3.1 gives a server exactly this way
                // to decline one: a 421 greeting, after which the connection closes. Writing
                // nothing (the previous behaviour) left the peer waiting for a 220 until its
                // own timeout, with no way to tell an overloaded server from a black hole.
                Log::new(Some(status_tx)).error(format!(
                    "SMTP greeting for connection {} failed: {:#}",
                    connection_id, e
                ));

                let reply: &[u8] = if crate::llm::is_overload_error(&e) {
                    warn!(
                        "SMTP 421 on connection {}: LLM capacity exhausted",
                        connection_id
                    );
                    b"421 4.3.2 Service not available, backend at capacity\r\n"
                } else {
                    b"421 4.3.0 Service not available, closing transmission channel\r\n"
                };
                let mut writer = write_half.lock().await;
                writer.write_all(reply).await?;
                writer.flush().await?;
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
                let _ = status_tx.send(format!(
                    "→ SMTP {} to connection {}",
                    String::from_utf8_lossy(reply).trim(),
                    connection_id
                ));

                // Propagate so handle_session stops before the command loop: after a 421 the
                // only thing the server may do is close.
                anyhow::bail!("SMTP greeting unavailable: {e:#}");
            }
        }

        Ok(true)
    }
}

#[cfg(not(feature = "smtp"))]
impl SmtpServer {
    pub async fn spawn_with_llm_actions(
        _listen_addr: SocketAddr,
        _llm_client: OllamaClient,
        _app_state: Arc<AppState>,
        _status_tx: mpsc::UnboundedSender<String>,
        _server_id: crate::state::ServerId,
        _tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> Result<SocketAddr> {
        anyhow::bail!("SMTP feature not enabled")
    }
}
