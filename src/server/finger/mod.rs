//! Finger (RFC 1288) server.
//!
//! One TCP connection carries exactly one query line and one free-text answer, and then the
//! server closes — which is what RFC 1288 §2.3 specifies and what every real client relies on,
//! because a finger client reads until EOF. This server does **not** keep reading after the
//! answer; see `src/server/finger/CLAUDE.md` for why that differs from the WHOIS
//! implementation next door.
//!
//! Nothing local is ever consulted. There is no `passwd` lookup, no `utmp`, no `.plan` file,
//! and no filesystem access of any kind: a real finger daemon leaks real accounts, and the
//! whole point of this one is that the model invents every user.
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use actions::{FingerQuery, FINGER_QUERY_EVENT, MAX_QUERY_BYTES};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// RFC 1288 §3.2.1's own recommended wording for a refused forwarding query.
const FORWARD_DENIED: &[u8] = b"Finger forwarding service denied.\r\n";

/// The query line exceeded [`MAX_QUERY_BYTES`] with no terminator in sight.
const QUERY_TOO_LONG: &[u8] = b"finger: query too long\r\n";

/// The model (or a handler) answered without producing anything for the wire.
const NO_INFORMATION: &[u8] = b"finger: no information available\r\n";

pub struct FingerServer;

impl FingerServer {
    /// Bind, then serve. Binding is awaited here so a failure is returned rather than lost in
    /// a detached task — `server_startup` turns the `Err` into `ServerStatus::Error`.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        answer_forward_queries: bool,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        let log = Log::new(Some(&status_tx));
        log.info(format!("FINGER server listening on {}", local_addr));
        if answer_forward_queries {
            log.warn(
                "FINGER answer_forward_queries=true: user@host queries will be answered locally \
                 with invented information. Nothing is proxied to the named host.",
            );
        } else {
            log.debug(
                "FINGER forwarding refused (RFC 1288 3.2.1); set answer_forward_queries=true to \
                 answer user@host locally instead",
            );
        }

        let protocol = Arc::new(actions::FingerProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, peer_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
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
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        Log::new(Some(&status_tx))
                            .info(format!("FINGER client connected from {}", peer_addr));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let registrar = app_state.clone();

                        let conn_handle = tokio::spawn(async move {
                            handle_finger_connection(
                                socket,
                                peer_addr,
                                llm_clone,
                                state_clone,
                                status_clone,
                                server_id,
                                protocol_clone,
                                connection_id,
                                answer_forward_queries,
                            )
                            .await
                        });

                        // Register every task we spawn, not just the accept loop: aborting a
                        // task does not abort tasks it spawned, so an unregistered connection
                        // task keeps its socket alive after stop_server.
                        registrar.register_server_task(server_id, conn_handle).await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("FINGER accept error: {}", e));
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

/// Write one reply and count it. The guard is dropped before the stats update so nothing
/// awaits an `AppState` lock while holding the write half.
async fn write_counted<W>(
    write_half: &Arc<Mutex<W>>,
    data: &[u8],
    app_state: &AppState,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    {
        let mut writer = write_half.lock().await;
        writer.write_all(data).await?;
        writer.flush().await?;
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

#[allow(clippy::too_many_arguments)]
async fn handle_finger_connection(
    socket: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::FingerProtocol>,
    connection_id: ConnectionId,
    answer_forward_queries: bool,
) {
    let (reader, write_half) = tokio::io::split(socket);
    let write_half = Arc::new(Mutex::new(write_half));

    // Registered before the first read: a finger server says nothing until the client speaks,
    // and a manual `*` rule parks the query for as long as the operator takes to answer it.
    // The dashboard must be able to reach the connection while it waits.
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

    run_finger_session(
        reader,
        &write_half,
        peer_addr,
        &llm_client,
        &app_state,
        &status_tx,
        server_id,
        &protocol,
        connection_id,
        answer_forward_queries,
    )
    .await;

    // Every exit path lands here. Dropping the handle ends the peer command task, which
    // releases its clone of the write half; the explicit shutdown makes the FIN immediate,
    // and a finger client is blocked on exactly that EOF.
    app_state
        .remove_peer_handle(server_id, connection_id.as_u32())
        .await;
    let _ = write_half.lock().await.shutdown().await;

    use crate::state::server::ConnectionStatus;
    app_state
        .update_connection_status(server_id, connection_id, ConnectionStatus::Closed)
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());
}

/// What reading the client's one query line produced.
enum QueryRead {
    /// A complete line (terminator stripped) and the number of bytes consumed.
    Line(String, usize),
    /// The peer closed without sending anything.
    Eof,
    /// More than [`MAX_QUERY_BYTES`] arrived with no terminator.
    TooLong,
    /// The socket errored.
    Failed(std::io::Error),
}

/// Read one CRLF-terminated query.
///
/// Lenient about the terminator: a bare LF is accepted, and so is EOF after a partial line —
/// RFC 1288 requires CRLF and every real client sends it, but a hand-typed `nc` session or a
/// script using `printf` without `\r` is not worth refusing.
async fn read_query_line<R>(reader: &mut R) -> QueryRead
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut accumulated: Vec<u8> = Vec::with_capacity(64);
    let mut chunk = [0u8; 256];

    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => {
                return if accumulated.is_empty() {
                    QueryRead::Eof
                } else {
                    let n = accumulated.len();
                    QueryRead::Line(
                        String::from_utf8_lossy(&accumulated).trim_end().to_string(),
                        n,
                    )
                };
            }
            Ok(n) => {
                accumulated.extend_from_slice(&chunk[..n]);
                if let Some(idx) = accumulated.iter().position(|b| *b == b'\n') {
                    let consumed = accumulated.len();
                    let line = String::from_utf8_lossy(&accumulated[..idx])
                        .trim_end_matches('\r')
                        .to_string();
                    return QueryRead::Line(line, consumed);
                }
                if accumulated.len() > MAX_QUERY_BYTES {
                    return QueryRead::TooLong;
                }
            }
            Err(e) => return QueryRead::Failed(e),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_finger_session<R, W>(
    mut reader: R,
    write_half: &Arc<Mutex<W>>,
    peer_addr: SocketAddr,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: &Arc<actions::FingerProtocol>,
    connection_id: ConnectionId,
    answer_forward_queries: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let log = Log::new(Some(status_tx));

    let (line, consumed) = match read_query_line(&mut reader).await {
        QueryRead::Line(line, consumed) => (line, consumed),
        QueryRead::Eof => {
            log.info(format!(
                "FINGER client {} disconnected without sending a query",
                peer_addr
            ));
            return;
        }
        QueryRead::TooLong => {
            log.warn(format!(
                "FINGER query from {} exceeded {} bytes with no terminator; refusing",
                peer_addr, MAX_QUERY_BYTES
            ));
            let _ = write_counted(
                write_half,
                QUERY_TOO_LONG,
                app_state,
                server_id,
                connection_id,
            )
            .await;
            return;
        }
        QueryRead::Failed(e) => {
            log.error(format!("FINGER read error from {}: {}", peer_addr, e));
            return;
        }
    };

    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(consumed as u64),
            None,
            Some(1),
            None,
        )
        .await;

    log.debug(format!(
        "FINGER received {} bytes from {}",
        consumed, peer_addr
    ));
    log.trace(format!("FINGER query line: {}", line));

    let query = FingerQuery::parse(&line);

    // Forwarding is refused *before* the model is consulted, and the refusal is the same
    // every time. RFC 1288 3.2.1 calls forwarding a security risk; NetGet has no outbound
    // code path at all, so "refuse" here means "say so and hang up", never "proxy quietly".
    if let Some(host) = query.forward_host.as_deref() {
        if !answer_forward_queries {
            log.warn(format!(
                "FINGER decision=forward_refused peer={} user={:?} forward_host={}",
                peer_addr, query.username, host
            ));
            let _ = write_counted(
                write_half,
                FORWARD_DENIED,
                app_state,
                server_id,
                connection_id,
            )
            .await;
            return;
        }
        log.warn(format!(
            "FINGER decision=forward_answered_locally peer={} user={:?} forward_host={} \
             (nothing is contacted; the model invents the answer)",
            peer_addr, query.username, host
        ));
    }

    let event = Event::new(&FINGER_QUERY_EVENT, query.to_event_data());

    log.debug(format!("FINGER calling LLM for query from {}", peer_addr));

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

            let mut wrote_output = false;
            for protocol_result in execution_result.protocol_results {
                match protocol_result {
                    crate::llm::actions::protocol_trait::ActionResult::Output(output_data) => {
                        if let Err(e) = write_counted(
                            write_half,
                            &output_data,
                            app_state,
                            server_id,
                            connection_id,
                        )
                        .await
                        {
                            log.error(format!("FINGER write error: {}", e));
                            return;
                        }
                        wrote_output = true;
                        log.debug(format!(
                            "FINGER sent {} bytes to {}",
                            output_data.len(),
                            peer_addr
                        ));
                        log.trace(format!(
                            "FINGER response: {}",
                            String::from_utf8_lossy(&output_data)
                        ));
                        log.info(format!(
                            "FINGER response to {} ({} bytes)",
                            peer_addr,
                            output_data.len()
                        ));
                    }
                    crate::llm::actions::protocol_trait::ActionResult::CloseConnection => {
                        log.debug("FINGER closing connection per LLM request");
                        // The session ends after this response regardless; an explicit close
                        // just means "and write nothing more".
                        break;
                    }
                    _ => {}
                }
            }

            // Nothing reached the wire: the model answered with only a close, or every action
            // failed. Closing in silence is indistinguishable from a broken server, and the
            // client is blocked on EOF either way, so say what happened. "No information
            // available" asserts nothing about any user.
            if !wrote_output {
                log.warn(format!(
                    "FINGER produced no response for {} ({} failed action(s)); answering with a \
                     notice instead of closing silently",
                    peer_addr,
                    execution_result.failures.len()
                ));
                let _ = write_counted(
                    write_half,
                    NO_INFORMATION,
                    app_state,
                    server_id,
                    connection_id,
                )
                .await;
            }
        }
        Err(e) => {
            // The peer gets a category; the log gets the error. Fixed byte literals rather
            // than a format string, so there is no placeholder an error could ever reach.
            log.warn(format!("FINGER LLM call failed for {}: {}", peer_addr, e));
            let notice: &[u8] = match WireFailure::classify(&e) {
                WireFailure::Overloaded => b"finger: backend at capacity, retry later\r\n",
                WireFailure::Unavailable => b"finger: request could not be processed\r\n",
            };
            let _ = write_counted(write_half, notice, app_state, server_id, connection_id).await;
        }
    }

    // Return, and the caller shuts the socket down. RFC 1288: one query, one answer, close.
}
