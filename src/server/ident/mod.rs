//! Ident (RFC 1413) server.
//!
//! One query, one reply, close. The client sends `<server-port> , <client-port>` and the
//! server answers either a USERID line or one of four ERROR tokens.
//!
//! Three properties this loop is responsible for, none of which live in `actions.rs`:
//!
//! 1. **`INVALID-PORT` is decided here, in Rust, with no LLM call.** A port outside
//!    `1..=65535` is a parse verdict, not a judgement anyone should be asked to make. The
//!    model never sees a malformed query.
//! 2. **The port pair on the wire is the pair that arrived.** RFC 1413 §3 has the client
//!    match a reply to its query by that pair, so a reply carrying a different one is
//!    unmatchable — to the client, indistinguishable from no reply. `enforce_port_pair`
//!    rewrites the model's answer if it echoed the pair wrongly, and says so at WARN.
//! 3. **Nothing is silent.** Ident has an error frame, so this is not the deliberate-silence
//!    class the root `CLAUDE.md` describes for ARP/OSPF/DHCP: an LLM outage, a model that
//!    answers with nothing, and an action that fails to execute all produce
//!    `ERROR : UNKNOWN-ERROR`. That is also the correct *fail-closed* answer, because it
//!    asserts nothing about any user — unlike a USERID line, which is a positive claim about
//!    an account.
//!
//! The distinction an outage would otherwise erase is kept in the **log**, the way
//! `src/server/radius/` keeps it: `decision=model_reject` is the model refusing,
//! `decision=model_silent` is the model answering with nothing, and
//! `decision=fail_closed_llm_error` is the backend being unreachable. All three look
//! identical on the wire because RFC 1413 gives them one token; grep `decision=` to tell
//! them apart. No error text ever reaches the peer.
//!
//! **NetGet consults no OS state.** See `actions.rs` — the model invents every userid.

pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::IDENT_QUERY_EVENT;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// RFC 1413 §5 caps a query line at 1000 characters. A peer that sends more without a
/// newline is not speaking ident; cap the buffer rather than growing it for them.
const MAX_QUERY_BYTES: usize = 1024;

/// How long to wait for the query line before giving up on a connected-but-silent peer.
/// Only covers the read: once the query has arrived, a manual handler may park the event for
/// as long as its own timeout allows.
const QUERY_READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct IdentServer;

impl IdentServer {
    /// Bind, then serve ident queries until the accept task is aborted.
    ///
    /// Awaits the bind and returns `Err` on failure, so `server_startup` can set
    /// `ServerStatus::Error` rather than reporting a server that is not listening.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("IDENT server listening on {}", local_addr));

        let protocol = Arc::new(actions::IdentProtocol::new());

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
                            .info(format!("IDENT client connected from {}", peer_addr));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        let registrar = app_state.clone();
                        let conn_handle = tokio::spawn(async move {
                            handle_ident_connection(
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

                        // Register every task we spawn, not just the accept loop. Aborting a
                        // task does not abort tasks it spawned, so an unregistered connection
                        // task survives `stop_server` with its socket — and an ident query
                        // parked on a manual handler can sit there for its full timeout.
                        // `register_server_task` prunes finished handles on every call, and
                        // an ident connection is one exchange long, so nothing accumulates.
                        //
                        // (A comment here claimed the repo has no per-connection handle
                        // store. It does, and `finger` and `gopher` next door both use it.)
                        registrar.register_server_task(server_id, conn_handle).await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("IDENT accept error: {}", e));
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

/// What the client's line turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum ParsedQuery {
    /// Two ports, both in `1..=65535`.
    Valid { server_port: u16, client_port: u16 },
    /// Anything else. `echo` is the pair as the client wrote it, sanitised, so the
    /// `INVALID-PORT` reply can still fill the port-pair position of the reply grammar.
    Invalid { echo: String },
}

/// Reduce one query field to something safe to echo into a reply line.
///
/// The reply is a single CRLF-terminated line with `:`-separated fields, so a field echoed
/// verbatim could otherwise forge a second reply or shift the ones after it.
fn echo_field(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && *c != ':' && *c != ',')
        .take(16)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Parse `<server-port> , <client-port>`.
///
/// Whitespace around the comma is legal (RFC 1413 §4's grammar admits it) and is what real
/// clients send, so it is tolerated rather than rejected.
fn parse_query(line: &str) -> ParsedQuery {
    let trimmed = line.trim();
    let mut halves = trimmed.splitn(2, ',');
    let left = halves.next().unwrap_or("").trim();
    let Some(right) = halves.next().map(str::trim) else {
        // No comma at all: not an ident query. Echo the whole line rather than inventing a
        // pair, so the client can at least see what the server read.
        return ParsedQuery::Invalid {
            echo: echo_field(trimmed),
        };
    };

    match (left.parse::<i64>(), right.parse::<i64>()) {
        (Ok(sp), Ok(cp)) if (1..=65535).contains(&sp) && (1..=65535).contains(&cp) => {
            ParsedQuery::Valid {
                server_port: sp as u16,
                client_port: cp as u16,
            }
        }
        _ => ParsedQuery::Invalid {
            echo: format!("{} , {}", echo_field(left), echo_field(right)),
        },
    }
}

/// Force the reply's port pair to be the one the client asked about.
///
/// Returns `Some(corrected)` only when the reply carried a parseable pair that differed from
/// the query's. A reply with no parseable pair is left alone: it did not come from this
/// protocol's own actions (a dashboard injection, say) and mangling it would be worse than
/// passing it through.
fn enforce_port_pair(reply: &[u8], server_port: u16, client_port: u16) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(reply).ok()?;
    let colon = text.find(':')?;
    let (pair, rest) = text.split_at(colon);

    let mut halves = pair.splitn(2, ',');
    let left = halves.next()?.trim().parse::<u16>().ok()?;
    let right = halves.next()?.trim().parse::<u16>().ok()?;

    if left == server_port && right == client_port {
        return None;
    }
    Some(format!("{} , {} {}", server_port, client_port, rest).into_bytes())
}

/// Which action the model actually answered with, for the `decision=` log token.
///
/// Read off `raw_actions` (the batch as submitted) rather than off the produced bytes, so a
/// refusal is identified by what the model asked for, not by what the wire happened to carry.
fn classify_decision(raw_actions: &[serde_json::Value]) -> Option<&'static str> {
    let mut saw_error = false;
    for action in raw_actions {
        match action.get("type").and_then(|v| v.as_str()) {
            Some("send_ident_userid") => return Some("model_userid"),
            Some("send_ident_error") => saw_error = true,
            _ => {}
        }
    }
    saw_error.then_some("model_reject")
}

#[allow(clippy::too_many_arguments)]
async fn handle_ident_connection(
    socket: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::IdentProtocol>,
    connection_id: ConnectionId,
) {
    let (reader, write_half) = tokio::io::split(socket);
    let write_half = Arc::new(Mutex::new(write_half));

    // Registered before the first read. An ident server says nothing until asked, and a
    // dashboard-created instance defaults to a `*` -> manual rule, so the query can park for
    // minutes waiting for the operator — who must be able to reach the connection meanwhile.
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

    run_ident_session(
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

    // Every exit path lands here. Dropping the handle ends the peer command task, which
    // releases its clone of the write half; the explicit shutdown makes the FIN immediate.
    // RFC 1413 has the server close once it has answered, and real clients read to EOF.
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

/// Write one reply and count it. The guard is dropped before the stats update so nothing
/// awaits while holding the write half.
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

/// Read the query line, answer it, return. One exchange per connection, per RFC 1413.
#[allow(clippy::too_many_arguments)]
async fn run_ident_session<R, W>(
    mut reader: R,
    write_half: &Arc<Mutex<W>>,
    peer_addr: SocketAddr,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: &Arc<actions::IdentProtocol>,
    connection_id: ConnectionId,
) where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let log = Log::new(Some(status_tx));

    let Some(line) = read_query_line(
        &mut reader,
        peer_addr,
        app_state,
        server_id,
        connection_id,
        &log,
    )
    .await
    else {
        return;
    };

    log.trace(format!("IDENT query line from {}: {:?}", peer_addr, line));

    let (server_port, client_port) = match parse_query(&line) {
        ParsedQuery::Invalid { echo } => {
            // Decided in Rust: a port outside 1..=65535 (or a line with no port pair at all)
            // is a parse error, not a decision, so the model is never asked and no LLM
            // budget is spent.
            log.warn(format!(
                "IDENT query from {} decision=invalid_port (rejected in-process, no LLM call)",
                peer_addr
            ));
            let reply = actions::format_error_reply_raw_pair(&echo, "INVALID-PORT");
            let _ = write_counted(write_half, &reply, app_state, server_id, connection_id).await;
            return;
        }
        ParsedQuery::Valid {
            server_port,
            client_port,
        } => (server_port, client_port),
    };

    let event = Event::new(
        &IDENT_QUERY_EVENT,
        serde_json::json!({
            "server_port": server_port,
            "client_port": client_port,
            "source_addr": peer_addr.to_string(),
        }),
    );

    log.debug(format!(
        "IDENT calling LLM for {},{} from {}",
        server_port, client_port, peer_addr
    ));

    let outcome = call_llm(
        llm_client,
        app_state,
        server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await;

    // `decision` is the token an operator greps for. `reply` is what goes on the wire; it is
    // never derived from an error, only ever chosen from this protocol's fixed vocabulary.
    let (decision, reply) = match outcome {
        Ok(execution_result) => {
            for message in &execution_result.messages {
                log.info(message);
            }

            let mut output: Option<Vec<u8>> = None;
            for protocol_result in &execution_result.protocol_results {
                if let crate::llm::actions::protocol_trait::ActionResult::Output(bytes) =
                    protocol_result
                {
                    // Concatenating rather than taking the first: a handler that answers
                    // with two lines is malformed ident, but dropping the second silently
                    // would hide that from the operator, and the pair check below still runs
                    // on the head of the stream.
                    output.get_or_insert_with(Vec::new).extend_from_slice(bytes);
                }
            }

            match output {
                Some(bytes) => {
                    let decision =
                        classify_decision(&execution_result.raw_actions).unwrap_or("model_answer");
                    // RFC 1413 §3: the client matches on the port pair. A model that echoed
                    // it wrongly would produce a reply the client discards, which looks
                    // exactly like the server never answering.
                    let bytes = match enforce_port_pair(&bytes, server_port, client_port) {
                        Some(corrected) => {
                            log.warn(format!(
                                "IDENT {},{} from {}: the answer carried a different port \
                                 pair; rewritten to the one queried, which is what the \
                                 client matches on",
                                server_port, client_port, peer_addr
                            ));
                            corrected
                        }
                        None => bytes,
                    };
                    (decision, bytes)
                }
                None => {
                    // The model answered with nothing usable (no actions, only a close, or
                    // every action failed). Fail closed: UNKNOWN-ERROR asserts nothing about
                    // any user, and a reply beats leaving the client to time out.
                    log.warn(format!(
                        "IDENT {},{} from {}: {} action(s) failed and nothing reached the wire",
                        server_port,
                        client_port,
                        peer_addr,
                        execution_result.failures.len()
                    ));
                    (
                        "model_silent",
                        actions::format_error_reply(server_port, client_port, "UNKNOWN-ERROR"),
                    )
                }
            }
        }
        Err(e) => {
            // The peer gets a category; the log gets the error. Nothing derived from `e`
            // reaches the wire — UNKNOWN-ERROR is a fixed token from the protocol's own
            // vocabulary, and is the fail-closed answer because it names no account.
            let category = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                "overloaded"
            } else {
                "unavailable"
            };
            log.error(format!(
                "IDENT {},{} from {} decision=fail_closed_llm_error category={} (answering \
                 UNKNOWN-ERROR because no decision was produced)",
                server_port, client_port, peer_addr, category
            ));
            log.debug(format!("IDENT LLM call failed: {}", e));
            (
                "fail_closed_llm_error",
                actions::format_error_reply(server_port, client_port, "UNKNOWN-ERROR"),
            )
        }
    };

    // Log the decision before writing it, so a reply that then fails to send is still
    // attributable. `decision=` tokens are stable: grep `decision=fail_closed_` for every
    // query the model did not actually answer.
    let summary = format!(
        "IDENT {},{} from {} decision={}",
        server_port, client_port, peer_addr, decision
    );
    if decision == "fail_closed_llm_error" || decision == "model_silent" {
        // Already logged at error/warn above with its reason; keep the summary at debug so
        // the grep-able token appears exactly once per outcome at INFO or louder.
        log.debug(&summary);
    } else {
        log.info(&summary);
    }

    log.trace(format!(
        "IDENT reply to {}: {}",
        peer_addr,
        String::from_utf8_lossy(&reply).trim_end()
    ));

    if let Err(e) = write_counted(write_half, &reply, app_state, server_id, connection_id).await {
        log.error(format!("IDENT write error to {}: {}", peer_addr, e));
    }
}

/// Accumulate until a newline, EOF, the size cap, or the read timeout.
///
/// Returns `None` only when there is nobody left to answer — a peer that connected and said
/// nothing, a peer that hung up before writing, a read error, or the read timeout. An
/// oversize line is *returned*, not dropped: the parser rejects it and the caller answers
/// `INVALID-PORT`, which is the one place a client is still waiting.
async fn read_query_line<R>(
    reader: &mut R,
    peer_addr: SocketAddr,
    app_state: &Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    log: &Log<'_>,
) -> Option<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = vec![0u8; 512];
    let mut accumulated: Vec<u8> = Vec::new();

    loop {
        let read = match tokio::time::timeout(QUERY_READ_TIMEOUT, reader.read(&mut buffer)).await {
            Err(_) => {
                log.info(format!(
                    "IDENT client {} sent no query within {}s; closing",
                    peer_addr,
                    QUERY_READ_TIMEOUT.as_secs()
                ));
                return None;
            }
            Ok(result) => result,
        };

        match read {
            Ok(0) => {
                // Half-close after an unterminated query is common enough to tolerate: if
                // anything arrived, treat it as the line rather than dropping a client that
                // simply omitted the CRLF.
                if accumulated.is_empty() {
                    log.info(format!(
                        "IDENT client {} disconnected without sending a query",
                        peer_addr
                    ));
                    return None;
                }
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
                log.debug(format!("IDENT received {} bytes from {}", n, peer_addr));

                accumulated.extend_from_slice(&buffer[..n]);
                if accumulated.contains(&b'\n') {
                    break;
                }
                if accumulated.len() > MAX_QUERY_BYTES {
                    // Stop reading, but hand back what we have: `parse_query` will reject it
                    // and the caller answers INVALID-PORT. Returning `None` here would hang
                    // up on a client that is still waiting for a reply.
                    log.warn(format!(
                        "IDENT query from {} exceeded {} bytes with no newline; \
                         answering INVALID-PORT",
                        peer_addr, MAX_QUERY_BYTES
                    ));
                    break;
                }
            }
            Err(e) => {
                log.error(format!("IDENT read error from {}: {}", peer_addr, e));
                return None;
            }
        }
    }

    // Only the first line matters: RFC 1413 is one query per connection.
    let text = String::from_utf8_lossy(&accumulated).to_string();
    Some(text.lines().next().unwrap_or("").to_string())
}
