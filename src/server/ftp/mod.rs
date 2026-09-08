//! FTP server implementation
//!
//! File Transfer Protocol (RFC 959) server with LLM-controlled responses.
//! Supports basic FTP commands: USER, PASS, SYST, PWD, CWD, LIST, RETR, STOR, QUIT, etc.

pub mod actions;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
#[cfg(feature = "ftp")]
use tokio::sync::Mutex;

#[cfg(feature = "ftp")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "ftp")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "ftp")]
use crate::llm::ActionResult;
#[cfg(feature = "ftp")]
use crate::logging::emit::Log;
#[cfg(feature = "ftp")]
use crate::protocol::Event;
#[cfg(feature = "ftp")]
use crate::server::ftp::actions::FtpProtocol;
#[cfg(feature = "ftp")]
use crate::state::app_state::AppState;
#[cfg(feature = "ftp")]
use actions::FTP_COMMAND_EVENT;

/// FTP server that provides LLM-controlled file transfer operations
pub struct FtpServer;

#[cfg(feature = "ftp")]
impl FtpServer {
    /// Spawn FTP server with integrated LLM actions
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

        Log::new(Some(&status_tx)).info(format!("FTP server listening on {}", local_addr));

        let protocol = Arc::new(FtpProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id = crate::server::connection::ConnectionId::new(
                            app_state.get_next_unified_id().await,
                        );
                        Log::new(Some(&status_tx)).info(format!(
                            "FTP connection {} from {}",
                            connection_id, remote_addr
                        ));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);

                        tokio::spawn(async move {
                            // Register the connection so it shows up in the TUI and in
                            // list_connections, and so stop_server accounts for it.
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

                            if let Err(e) = FtpSession::handle_session(
                                stream,
                                connection_id,
                                server_id,
                                llm_clone,
                                state_clone.clone(),
                                status_clone.clone(),
                                protocol_clone,
                            )
                            .await
                            {
                                Log::new(Some(&status_clone))
                                    .error(format!("FTP session error: {}", e));
                            }

                            state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept FTP connection: {}", e));
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

/// Largest control line the server will accumulate before giving up on the peer.
///
/// RFC 959 commands are short — the longest standard one is a `STOR` with a path — and
/// 8 KiB is far above anything a real client sends. The cap exists because
/// [`tokio::io::AsyncBufReadExt::read_line`] grows its buffer until it finds a newline: a
/// peer that opens a connection and streams bytes with no `\n` makes the server allocate
/// without bound, which is a one-connection out-of-memory.
#[cfg(feature = "ftp")]
const MAX_COMMAND_LINE: usize = 8192;

/// The result of trying to read one control line.
#[cfg(feature = "ftp")]
enum CommandLine {
    /// A complete line. The trailing CRLF is still attached; the caller trims it.
    Line(String),
    /// The peer closed the control connection.
    Eof,
    /// The peer sent `MAX_COMMAND_LINE` bytes with no newline in them.
    TooLong,
}

/// Read a single CRLF-terminated FTP command, refusing to buffer more than `max_len` bytes.
///
/// Returns the line and the number of bytes consumed from the socket, so the caller can
/// account for them in the connection stats exactly as `read_line` allowed.
#[cfg(feature = "ftp")]
async fn read_command_line<R>(
    reader: &mut tokio::io::BufReader<R>,
    max_len: usize,
) -> std::io::Result<(CommandLine, usize)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    let mut buf: Vec<u8> = Vec::new();
    loop {
        // `fill_buf` borrows the reader, so decide what to take and drop the borrow before
        // calling `consume`.
        let (newline_at, available_len) = {
            let available = reader.fill_buf().await?;
            (
                available.iter().position(|&b| b == b'\n'),
                available.len(),
            )
        };

        if available_len == 0 {
            // EOF. A trailing fragment with no newline is not a command; the peer hung up
            // mid-line, which is the same as hanging up.
            return Ok((CommandLine::Eof, buf.len()));
        }

        let take = match newline_at {
            Some(idx) => idx + 1,
            None => available_len,
        };

        if buf.len() + take > max_len {
            // Consume what we looked at so the reader is not left mid-buffer, then give up:
            // the caller closes the connection, so there is nothing to resynchronise with.
            reader.consume(take);
            return Ok((CommandLine::TooLong, buf.len() + take));
        }

        {
            let available = reader.fill_buf().await?;
            buf.extend_from_slice(&available[..take]);
        }
        reader.consume(take);

        if newline_at.is_some() {
            let consumed = buf.len();
            // `from_utf8_lossy` rather than a hard error: a stray non-UTF-8 byte in a
            // command is the peer's problem to be answered with 500, not a reason to drop
            // the connection without a reply.
            return Ok((
                CommandLine::Line(String::from_utf8_lossy(&buf).into_owned()),
                consumed,
            ));
        }
    }
}

#[cfg(feature = "ftp")]
struct FtpSession;

#[cfg(feature = "ftp")]
impl FtpSession {
    /// Handle an FTP session
    ///
    /// Owns the split socket. The write half lives in an `Arc<Mutex<_>>` shared with the
    /// peer command task, so the dashboard's "message this peer" / "disconnect this peer"
    /// write through the same half the session does. The peer handle is registered here
    /// (the accept loop has already added the connection) and removed on every exit —
    /// EOF, 421, `close_connection` and errors all funnel through the single return below.
    async fn handle_session(
        stream: tokio::net::TcpStream,
        connection_id: crate::server::connection::ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<FtpProtocol>,
    ) -> Result<()> {
        let (read_half, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(Mutex::new(write_half));

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

        let result = async {
            Self::send_greeting(
                &write_half,
                connection_id,
                server_id,
                &llm_client,
                &app_state,
                &status_tx,
                &protocol,
            )
            .await?;

            Self::handle_session_commands(
                read_half,
                &write_half,
                connection_id,
                server_id,
                llm_client,
                &app_state,
                &status_tx,
                protocol,
            )
            .await
        }
        .await;

        app_state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        result
    }

    /// Lock the shared write half, write + flush, and account for the bytes. The guard is
    /// dropped before returning so no `.await` on the LLM ever holds it.
    async fn write_out<W>(
        write_half: &Arc<Mutex<W>>,
        data: &[u8],
        app_state: &AppState,
        server_id: crate::state::ServerId,
        connection_id: crate::server::connection::ConnectionId,
    ) -> std::io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::AsyncWriteExt;
        {
            let mut write = write_half.lock().await;
            write.write_all(data).await?;
            write.flush().await?;
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
        Ok(())
    }

    /// Send FTP greeting (220 response)
    async fn send_greeting<W>(
        write_half: &Arc<Mutex<W>>,
        connection_id: crate::server::connection::ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<FtpProtocol>,
    ) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        // The client will not speak until it has seen a 2xx greeting, so this event is the
        // handler's only chance to produce one. `CONNECTION_ESTABLISHED` is a sentinel, not a
        // command the client sent - see FTP_COMMAND_EVENT.
        let greeting_event = Event::new(
            &FTP_COMMAND_EVENT,
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
                for protocol_result in execution_result.protocol_results {
                    if let ActionResult::Output(data) = protocol_result {
                        Self::write_out(write_half, &data, app_state, server_id, connection_id)
                            .await?;
                    }
                }
            }
            Err(e) => {
                // Logging it was an improvement on silence for *us*; the client still sat
                // waiting for a greeting that never came, because an FTP client may not send
                // a command until it has read one. RFC 959 defines 421 as the greeting a
                // server sends when it is declining the session, and it closes afterwards -
                // which is the same shape SMTP uses, for the same reason.
                let notice = crate::utils::WireFailure::classify(&e).prefixed_text();
                Log::new(Some(status_tx)).warn(format!(
                    "FTP greeting handler failed on connection {connection_id}, refused with 421 ({notice}): {e}"
                ));
                let reply =
                    format!("421 Service not available, closing control connection ({notice})\r\n");
                let _ = Self::write_out(
                    write_half,
                    reply.as_bytes(),
                    app_state,
                    server_id,
                    connection_id,
                )
                .await;
                // 421 means the control connection is closing, so the session must not
                // continue into the command loop. The caller propagates this with `?`, which
                // ends the connection task and drops the socket.
                return Err(anyhow::anyhow!("FTP greeting refused with 421: {e}"));
            }
        }

        Ok(())
    }

    /// Handle FTP session commands
    #[allow(clippy::too_many_arguments)]
    async fn handle_session_commands<R, W>(
        read_half: R,
        write_half: &Arc<Mutex<W>>,
        connection_id: crate::server::connection::ConnectionId,
        server_id: crate::state::ServerId,
        llm_client: OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: Arc<FtpProtocol>,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::BufReader;

        let mut reader = BufReader::new(read_half);
        let log = Log::new(Some(status_tx));

        loop {
            let (outcome, n) = read_command_line(&mut reader, MAX_COMMAND_LINE).await?;
            let line = match outcome {
                CommandLine::Eof => break,
                CommandLine::TooLong => {
                    // A control line this long is not an FTP command. RFC 959 has no
                    // "line too long" code, but 500 is the syntax-error reply and the
                    // connection is closed so the peer cannot keep feeding the buffer.
                    log.warn(format!(
                        "FTP command line from connection {connection_id} exceeded \
                         {MAX_COMMAND_LINE} bytes without a newline; closing"
                    ));
                    let _ = Self::write_out(
                        write_half,
                        b"500 Command line too long\r\n",
                        app_state,
                        server_id,
                        connection_id,
                    )
                    .await;
                    return Ok(());
                }
                CommandLine::Line(line) => line,
            };
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
            // FileOnly: the ftp_command event template surfaces the command on the TUI.
            log.debug(format!("FTP received: {}", command));

            // Create FTP command event
            let event = Event::new(
                &FTP_COMMAND_EVENT,
                serde_json::json!({
                    "command": command
                }),
            );

            // Get handler/LLM response
            match call_llm(
                &llm_client,
                app_state,
                server_id,
                Some(connection_id),
                &event,
                protocol.as_ref(),
            )
            .await
            {
                Ok(execution_result) => {
                    for protocol_result in execution_result.protocol_results {
                        match protocol_result {
                            ActionResult::Output(data) => {
                                Self::write_out(
                                    write_half,
                                    &data,
                                    app_state,
                                    server_id,
                                    connection_id,
                                )
                                .await?;

                                let response = String::from_utf8_lossy(&data);
                                log.debug(format!("FTP sent: {}", response.trim()));
                            }
                            ActionResult::CloseConnection => {
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    // Do not leave the client hanging with no diagnostic: RFC 959 421 tells it
                    // the service is unavailable and the control connection is closing.
                    //
                    // The peer gets the `WireFailure` *category* — the same one the greeting
                    // path uses — never the error text. `prefixed_text()` returns
                    // `&'static str`, so nothing derived from `e` can reach the wire; `e`
                    // itself goes to the log, which is where an operator looks.
                    let notice = crate::utils::WireFailure::classify(&e).prefixed_text();
                    log.warn(format!(
                        "FTP handler failed for command {command:?}, refused with 421 \
                         ({notice}): {e}"
                    ));
                    let reply = format!(
                        "421 Service not available, closing control connection ({notice})\r\n"
                    );
                    let _ = Self::write_out(
                        write_half,
                        reply.as_bytes(),
                        app_state,
                        server_id,
                        connection_id,
                    )
                    .await;
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

#[cfg(not(feature = "ftp"))]
impl FtpServer {
    pub async fn spawn_with_llm_actions(
        _listen_addr: SocketAddr,
        _llm_client: OllamaClient,
        _app_state: Arc<AppState>,
        _status_tx: mpsc::UnboundedSender<String>,
        _server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        anyhow::bail!("FTP feature not enabled")
    }
}

// Stub types needed for non-feature compilation
#[cfg(not(feature = "ftp"))]
use crate::llm::ollama_client::OllamaClient;
#[cfg(not(feature = "ftp"))]
use crate::state::app_state::AppState;
