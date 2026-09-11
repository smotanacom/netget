//! WHOIS (RFC 3912) server.
//!
//! The client sends one line, the server answers with free text. Three properties of the read
//! loop are worth knowing before changing it, because none of them is obvious from the RFC:
//!
//! 1. **A query is a *line*, not a TCP segment.** The loop used to raise one `whois_query`
//!    event per `read()`, so a query split across two segments became two events with two
//!    partial queries, and a peer dripping one byte at a time bought one LLM call per byte.
//!    That is unmetered work for anyone who can open a socket, so reads now accumulate to a
//!    newline under [`MAX_QUERY_BYTES`].
//! 2. **Reads are bounded in time as well as size.** A peer that connects and says nothing
//!    otherwise holds a task and a connection slot for as long as it likes.
//! 3. **The idle timeout is what rescues a real client from this server's one known
//!    non-conformance.** RFC 3912 has the server close as soon as its output is finished, and
//!    `whois(1)` reads until EOF — but this server keeps reading so several queries can share
//!    a connection, so a handler that answers without `close_connection` would otherwise
//!    block a real client forever. It now blocks for [`IDLE_AFTER_REPLY_TIMEOUT`] instead.
//!    Pairing the answer with `close_connection` is still the right thing to do, and is what
//!    both `send_*` action descriptions say.
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::WHOIS_QUERY_EVENT;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// A WHOIS query is a domain, a handle or an IP. Anything past this is not one, and buffering
/// it for a peer who may never send a newline is a memory hole reachable by anyone.
const MAX_QUERY_BYTES: usize = 4096;

/// How long to wait for the first query from a peer that has only connected.
const FIRST_QUERY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a *further* query after one has been answered.
///
/// Shorter than the first wait on purpose: RFC 3912 expects the connection to be over at this
/// point, so anyone still holding it open is the exception. See the module note — this is the
/// bound that turns "blocked forever" into "blocked briefly" for a client whose handler forgot
/// `close_connection`.
const IDLE_AFTER_REPLY_TIMEOUT: Duration = Duration::from_secs(15);

pub struct WhoisServer;

impl WhoisServer {
    /// Spawn WHOIS server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("WHOIS server listening on {}", local_addr));

        let protocol = Arc::new(actions::WhoisProtocol::new());

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
                            .info(format!("WHOIS client connected from {}", peer_addr));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let connection_id_clone = connection_id;

                        tokio::spawn(async move {
                            handle_whois_connection(
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
                        Log::new(Some(&status_tx)).error(format!("WHOIS accept error: {}", e));
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

/// Write one reply to the peer and count it. The guard is dropped before the
/// stats update so nothing awaits while holding the write half.
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
async fn handle_whois_connection(
    socket: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::WhoisProtocol>,
    connection_id: ConnectionId,
) {
    let (reader, write_half) = tokio::io::split(socket);
    let write_half = Arc::new(Mutex::new(write_half));

    // Peer messaging: the dashboard's "message this peer" / "disconnect this peer" inject
    // actions into THIS connection through the same executor the LLM path uses. Registered
    // before the first read, because a WHOIS server says nothing until the client speaks
    // and a manual `*` rule can then park the query for minutes - the operator must be
    // able to reach the connection while it waits.
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

    run_whois_session(
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

    // Every exit path - EOF, read error, write error, close_connection, LLM failure - lands
    // here. Dropping the handle also ends the peer command task, which releases its clone of
    // the write half; the explicit shutdown makes the FIN immediate rather than waiting on it.
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

#[allow(clippy::too_many_arguments)]
async fn run_whois_session<R, W>(
    mut reader: R,
    write_half: &Arc<Mutex<W>>,
    peer_addr: SocketAddr,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: &Arc<actions::WhoisProtocol>,
    connection_id: ConnectionId,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let log = Log::new(Some(status_tx));
    let mut lines = QueryLineReader::new(&mut reader);
    let mut answered_one = false;

    loop {
        let read_timeout = if answered_one {
            IDLE_AFTER_REPLY_TIMEOUT
        } else {
            FIRST_QUERY_READ_TIMEOUT
        };

        let (query, n) = match lines.next_query(read_timeout).await {
            QueryRead::Line(line, n) => (line, n),
            QueryRead::Eof => {
                log.info(format!("WHOIS client {} disconnected", peer_addr));
                break;
            }
            QueryRead::TimedOut => {
                log.info(format!(
                    "WHOIS client {} sent nothing further within {}s; closing",
                    peer_addr,
                    read_timeout.as_secs()
                ));
                break;
            }
            QueryRead::TooLong => {
                // A '%' line is a comment in every WHOIS dialect, so a client reads this as a
                // remark and never as a record. Fixed text, so there is no placeholder an
                // internal error could reach.
                log.warn(format!(
                    "WHOIS query from {} exceeded {} bytes with no newline; refusing",
                    peer_addr, MAX_QUERY_BYTES
                ));
                let _ = write_counted(
                    write_half,
                    b"% netget: query too long\r\n",
                    app_state,
                    server_id,
                    connection_id,
                )
                .await;
                break;
            }
            QueryRead::Failed(e) => {
                log.error(format!("WHOIS read error from {}: {}", peer_addr, e));
                break;
            }
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

        // Summary + payload are FileOnly: the whois_query event template surfaces the query
        // to the TUI.
        log.debug(format!("WHOIS received {} bytes from {}", n, peer_addr));
        log.trace(format!("WHOIS query data: {}", query));

        let event = Event::new(
            &WHOIS_QUERY_EVENT,
            serde_json::json!({
                "query": query,
            }),
        );

        log.debug(format!("WHOIS calling LLM for query from {}", peer_addr));

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
                    "WHOIS got {} protocol results",
                    execution_result.protocol_results.len()
                ));

                // Send all outputs to the client and note whether a close was asked for.
                //
                // WHOIS is one query, one response, then close (RFC 3912), so a connection
                // that closes without writing anything is indistinguishable to the client
                // from a server that is broken. Track whether anything reached the wire and
                // answer below if nothing did.
                let mut should_close = false;
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
                                log.error(format!("WHOIS write error: {}", e));
                                return;
                            }
                            wrote_output = true;

                            // Summary + payload FileOnly; access line below on TUI.
                            log.debug(format!(
                                "WHOIS sent {} bytes to {}",
                                output_data.len(),
                                peer_addr
                            ));
                            log.trace(format!(
                                "WHOIS response: {}",
                                String::from_utf8_lossy(&output_data)
                            ));
                            log.info(format!(
                                "WHOIS response to {} ({} bytes)",
                                peer_addr,
                                output_data.len()
                            ));
                        }
                        crate::llm::actions::protocol_trait::ActionResult::CloseConnection => {
                            should_close = true;
                            log.debug("WHOIS closing connection per LLM request");
                        }
                        _ => {} // Ignore other action results
                    }
                }

                // Nothing reached the wire: the model answered with only a close, or with
                // actions that all failed. Say so in a WHOIS comment line rather than hanging
                // up silently — '%' is the conventional comment marker, so a client reads it
                // as a remark and never as a record.
                if !wrote_output {
                    log.warn(format!(
                        "WHOIS {:?} from {} decision=model_silent ({} failed action(s)); \
                         answering with a comment instead of closing silently",
                        query,
                        peer_addr,
                        execution_result.failures.len()
                    ));
                    let notice = b"% netget: no data was produced for this query\r\n";
                    if write_counted(write_half, notice, app_state, server_id, connection_id)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }

                answered_one = true;
                if should_close {
                    break;
                }
            }
            Err(e) => {
                // The peer gets a category, the log gets the error. Two fixed byte literals
                // rather than one, because WHOIS has no status code and a comment line is the
                // only place the distinction can live at all: a client (or the human reading
                // it) should know whether to come back. Byte literals, so there is no
                // placeholder anything derived from `e` could reach —
                // `crate::utils::WireFailure` exists for exactly this, and returns
                // `&'static str` for the same reason.
                let (category, notice): (&str, &[u8]) =
                    match crate::utils::WireFailure::classify(&e) {
                        crate::utils::WireFailure::Overloaded => (
                            "overloaded",
                            b"% netget: backend at capacity, retry later\r\n",
                        ),
                        crate::utils::WireFailure::Unavailable => (
                            "unavailable",
                            b"% netget: the query could not be answered\r\n",
                        ),
                    };
                log.warn(format!(
                    "WHOIS {:?} from {} decision=fail_closed_llm_error category={}",
                    query, peer_addr, category
                ));
                log.debug(format!("WHOIS LLM call failed: {}", e));
                let _ =
                    write_counted(write_half, notice, app_state, server_id, connection_id).await;
                break;
            }
        }
    }
}

/// What one attempt to read a query line produced.
enum QueryRead {
    /// A query (trimmed) and the wire bytes it consumed.
    Line(String, usize),
    /// The peer hung up with nothing pending.
    Eof,
    /// Nothing arrived within the caller's deadline.
    TimedOut,
    /// [`MAX_QUERY_BYTES`] arrived with no newline in them.
    TooLong,
    /// The socket errored.
    Failed(std::io::Error),
}

/// Accumulates reads into whole lines.
///
/// A WHOIS query is a line, and the previous loop treated each `read()` as one — so a query
/// split across two TCP segments raised two `whois_query` events carrying two fragments, and a
/// peer sending a byte at a time bought one LLM call per byte from an unauthenticated socket.
/// Leftovers after the newline are kept, because a client may send a second query.
struct QueryLineReader<'a, R> {
    reader: &'a mut R,
    pending: Vec<u8>,
    chunk: Vec<u8>,
}

impl<'a, R: tokio::io::AsyncRead + Unpin> QueryLineReader<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            chunk: vec![0u8; 1024],
        }
    }

    /// The `timeout` bounds the wait for *more bytes*, not the whole line: a peer that is
    /// still sending keeps the connection alive, which is what a client on a slow link needs.
    async fn next_query(&mut self, timeout: Duration) -> QueryRead {
        loop {
            if let Some(idx) = self.pending.iter().position(|b| *b == b'\n') {
                let consumed = idx + 1;
                let line: Vec<u8> = self.pending.drain(..consumed).collect();
                return QueryRead::Line(
                    String::from_utf8_lossy(&line[..idx]).trim().to_string(),
                    consumed,
                );
            }
            if self.pending.len() > MAX_QUERY_BYTES {
                return QueryRead::TooLong;
            }

            let read = match tokio::time::timeout(timeout, self.reader.read(&mut self.chunk)).await
            {
                Err(_) => return QueryRead::TimedOut,
                Ok(read) => read,
            };

            match read {
                Ok(0) => {
                    if self.pending.is_empty() {
                        return QueryRead::Eof;
                    }
                    // A query with no terminator, followed by a half-close. Real clients send
                    // CRLF, but answering is better than dropping someone who did not.
                    let line = std::mem::take(&mut self.pending);
                    let consumed = line.len();
                    return QueryRead::Line(
                        String::from_utf8_lossy(&line).trim().to_string(),
                        consumed,
                    );
                }
                Ok(n) => self.pending.extend_from_slice(&self.chunk[..n]),
                Err(e) => return QueryRead::Failed(e),
            }
        }
    }
}
