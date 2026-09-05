//! Gopher (RFC 1436) server.
//!
//! One connection, one selector line, one reply, then the server closes — which is what
//! RFC 1436 specifies ("the server sends the requested item and then closes the connection")
//! and what every real client needs, because a Gopher reply carries no length. `curl` reads
//! until EOF and exits 28 if the server does not hang up. WHOIS in this repo keeps reading
//! instead and documents the resulting hang as a known non-conformance;
//! `src/server/gopher/CLAUDE.md` explains why this one does not repeat it.
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::GOPHER_REQUEST_EVENT;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// A selector line longer than this is not a Gopher request. RFC 1436 puts no limit on it,
/// but an unbounded read is a memory hole reachable by anyone who can open a socket.
const MAX_REQUEST_BYTES: usize = 8192;

pub struct GopherServer;

impl GopherServer {
    /// Bind, then serve. Returns `Err` if the bind fails so `server_startup` can set
    /// `ServerStatus::Error` rather than reporting a server that never came up.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Gopher server listening on {}", local_addr));

        let protocol = Arc::new(actions::GopherProtocol::new());

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
                            .info(format!("Gopher client connected from {}", peer_addr));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        let conn_handle = tokio::spawn(async move {
                            handle_gopher_connection(
                                socket,
                                peer_addr,
                                llm_clone,
                                state_clone,
                                status_clone,
                                server_id,
                                protocol_clone,
                                connection_id,
                            )
                            .await
                        });

                        // Register the per-connection task too, not just the accept loop:
                        // `register_server_task` prunes finished handles on every call, so a
                        // one-request-per-connection protocol accumulates nothing, and
                        // `stop_server` then actually cancels a connection parked on a manual
                        // handler instead of leaving it running.
                        app_state.register_server_task(server_id, conn_handle).await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Gopher accept error: {}", e));
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

/// Write one reply and count it. The guard is dropped before the stats update, so nothing
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

/// The type-3 error item netget writes when it cannot ask the model, or when the model's
/// answer produced nothing.
///
/// `text` is a `&'static str` from `crate::utils::WireFailure` — a category, never the error
/// itself. Interpolating the backend's message here would put netget's model name, backend
/// URL and retry machinery on a stranger's screen. Callers pass the *prefixed* form, which
/// already names netget: a `format!` that prepended the name here would be a peer-visible
/// template one edit away from carrying the error too, and `tests/wire_failure_test.rs`
/// rejects exactly that shape.
fn wire_failure_item(text: &'static str) -> Vec<u8> {
    format!("3{}\t\terror.host\t1\r\n.\r\n", text).into_bytes()
}

#[allow(clippy::too_many_arguments)]
async fn handle_gopher_connection(
    socket: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::GopherProtocol>,
    connection_id: ConnectionId,
) {
    let (reader, write_half) = tokio::io::split(socket);
    let write_half = Arc::new(Mutex::new(write_half));

    // Peer messaging is registered before the first read: a Gopher server says nothing until
    // the selector arrives, and a manual `*` rule parks that request for as long as the
    // operator takes to answer it. Without a handle in place first, the dashboard's
    // "message this peer" is greyed out for exactly the window in which it is wanted.
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

    run_gopher_session(
        reader,
        &write_half,
        peer_addr,
        &llm_client,
        &app_state,
        &status_tx,
        server_id,
        &protocol,
        connection_id,
    )
    .await;

    // Every exit path lands here. Dropping the peer handle ends the peer command task, which
    // releases its clone of the write half; the explicit shutdown makes the FIN immediate,
    // and the FIN is the whole protocol - it is how the client learns the reply is complete.
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

/// Read the selector line, ask whoever is answering, write the reply. Returns when the
/// connection should close — which, for Gopher, is always after one reply.
#[allow(clippy::too_many_arguments)]
async fn run_gopher_session<R, W>(
    mut reader: R,
    write_half: &Arc<Mutex<W>>,
    peer_addr: SocketAddr,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: &Arc<actions::GopherProtocol>,
    connection_id: ConnectionId,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let log = Log::new(Some(status_tx));

    // --- Read exactly one request line -------------------------------------------------
    let mut request = Vec::new();
    let mut chunk = vec![0u8; 1024];
    let line_end = loop {
        match reader.read(&mut chunk).await {
            Ok(0) => {
                log.info(format!(
                    "Gopher client {} disconnected before sending a selector",
                    peer_addr
                ));
                return;
            }
            Ok(n) => {
                request.extend_from_slice(&chunk[..n]);
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

                if let Some(pos) = request.iter().position(|b| *b == b'\n') {
                    break pos;
                }
                if request.len() > MAX_REQUEST_BYTES {
                    log.warn(format!(
                        "Gopher request from {} exceeded {} bytes with no line ending; closing",
                        peer_addr, MAX_REQUEST_BYTES
                    ));
                    let _ = write_counted(
                        write_half,
                        &wire_failure_item("netget: selector line too long"),
                        app_state,
                        server_id,
                        connection_id,
                    )
                    .await;
                    return;
                }
            }
            Err(e) => {
                log.error(format!("Gopher read error from {}: {}", peer_addr, e));
                return;
            }
        }
    };

    // Anything after the first line is not part of a Gopher request; there is no second
    // request on this connection, so it is discarded with the socket.
    let line = String::from_utf8_lossy(&request[..line_end])
        .trim_end_matches('\r')
        .to_string();

    // A type-7 search request is `<selector>\t<query>`. Splitting on the first tab only:
    // a query may itself contain tabs, and they belong to the query.
    let (selector, search_query) = match line.split_once('\t') {
        Some((sel, query)) => (sel.to_string(), Some(query.to_string())),
        None => (line.clone(), None),
    };

    log.debug(format!(
        "Gopher received {} bytes from {}",
        request.len(),
        peer_addr
    ));
    log.trace(format!("Gopher request line: {:?}", line));

    let mut event_data = serde_json::json!({ "selector": selector });
    if let Some(query) = &search_query {
        // Present only for a type-7 request: its absence distinguishes "open the search
        // form" from "search for the empty string".
        event_data["search_query"] = serde_json::Value::String(query.clone());
    }
    let event = Event::new(&GOPHER_REQUEST_EVENT, event_data);

    // --- Answer it ----------------------------------------------------------------------
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
            let mut asked_to_close = false;
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
                            log.error(format!("Gopher write error: {}", e));
                            return;
                        }
                        wrote_output = true;
                        log.debug(format!(
                            "Gopher sent {} bytes to {}",
                            output_data.len(),
                            peer_addr
                        ));
                        log.trace(format!(
                            "Gopher response: {}",
                            String::from_utf8_lossy(&output_data)
                        ));
                        log.info(format!(
                            "Gopher response to {} ({} bytes)",
                            peer_addr,
                            output_data.len()
                        ));
                    }
                    crate::llm::actions::protocol_trait::ActionResult::CloseConnection => {
                        asked_to_close = true;
                        log.debug("Gopher close_connection requested");
                    }
                    _ => {}
                }
            }

            // Nothing reached the wire and nobody asked to hang up: every action failed, or
            // the answer was empty. A client that reads to EOF would take that as an empty
            // document, so say what happened in the one way Gopher has to say it.
            if !wrote_output && !asked_to_close {
                log.warn(format!(
                    "Gopher produced no response for {} ({} failed action(s)); \
                     answering with a type-3 error item",
                    peer_addr,
                    execution_result.failures.len()
                ));
                let _ = write_counted(
                    write_half,
                    &wire_failure_item(crate::utils::WireFailure::Unavailable.prefixed_text()),
                    app_state,
                    server_id,
                    connection_id,
                )
                .await;
            }
        }
        Err(e) => {
            // The category, never the error. `prefixed_wire_failure_text` returns `&'static str`
            // precisely so nothing derived from `e` can reach the peer.
            log.warn(format!("Gopher LLM call failed: {}", e));
            let _ = write_counted(
                write_half,
                &wire_failure_item(crate::utils::prefixed_wire_failure_text(&e)),
                app_state,
                server_id,
                connection_id,
            )
            .await;
        }
    }

    // Return unconditionally: RFC 1436 closes after the reply, and the caller's shutdown is
    // what tells the client the transfer is complete.
    log.debug(format!(
        "Gopher closing connection to {} after one reply (RFC 1436)",
        peer_addr
    ));
}
