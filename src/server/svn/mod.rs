//! SVN (Subversion) server implementation
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use actions::{
    SVN_AUTH_RESPONSE_EVENT, SVN_CLIENT_CAPABILITIES_EVENT, SVN_COMMAND_EVENT, SVN_GREETING_EVENT,
};
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

/// How long to wait for the client's answer to the greeting.
///
/// `svnserve` speaks first: the server's greeting goes out, and a real `svn` client answers
/// with its capabilities immediately — it has nothing to decide and nobody to ask. A peer that
/// has taken the greeting and gone quiet is holding a task and a connection slot on the
/// strength of nothing at all.
const FIRST_COMMAND_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long to wait for a *further* command once one has been answered.
///
/// ra_svn after the greeting is strictly request/response — the client sends the next command
/// as soon as it has consumed the last reply — so seconds of silence normally means the client
/// is gone. The exception, and the reason this is minutes rather than seconds, is that `svn`
/// prompts for credentials on the user's terminal *mid-session*: a human typing a password is
/// a legitimate multi-minute pause with the connection live and nothing on the wire. Three
/// minutes covers that and still bounds the hold.
const IDLE_BETWEEN_COMMANDS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Concurrent connections this server admits.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] is told before the socket closes.
///
/// ra_svn's own `( failure ( ( apr-err message file line ) ) )` tuple, built the same way
/// [`svn_failure_tuple`] builds it, with apr-err 210003 — the code this file already uses for
/// "the backend is at capacity" rather than "the request was wrong". A client that reads it
/// where it expected a greeting reports malformed data rather than parsing the reason, which
/// is a real limit of speaking before the greeting; what it buys is an operator, a packet
/// capture and a `nc` session that can all see *why* in the bytes, instead of a bare reset
/// indistinguishable from a crashed server.
const CONNECTION_CAP_REFUSAL: &[u8] =
    b"( failure ( ( 210003 28:netget: too many connections 0: 0 ) ) )\n";

/// Largest single ra_svn message this server will buffer, in bytes.
///
/// An unbounded read lets one peer grow the process without limit — no authentication, no
/// negotiation, just an open socket. The cap is applied by [`wire::ItemReader::read_item`] to
/// the whole tuple *and* to any string length the peer declares, before a byte is allocated
/// for it. A real ra_svn command tuple is a few hundred bytes; 64 KiB is far above anything
/// the subset implemented here can produce and far below a memory problem.
pub const MAX_COMMAND_BYTES: u64 = 64 * 1024;

/// The trivial auth-request that precedes every command response in a real ra_svn session.
///
/// ra_svn's client calls `handle_auth_request` after *each* command it sends: it reads one
/// `( success ( mechs:list realm:string ) )` and returns immediately, without replying, when
/// the mechanism list is empty. So an already-authenticated session's every response is two
/// tuples — this one, then the answer. Measured against `svn` 1.14.5: omit it and the client
/// reads this tuple where the answer should be and reports `E210004: Malformed network data`.
///
/// It is written by the server rather than by an action because it carries no decision: it is
/// the same bytes every time, and making the model emit it would be making the model do
/// framing. It is written **only** for a peer that completed the handshake, so a `nc` session
/// — or this repo's own mocked e2e tests, which never send a capability tuple — still sees
/// exactly the bytes its action produced.
const COMMAND_AUTH_PREFIX: &[u8] = b"( success ( ( ) 0: ) ) ";

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
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "SVN",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((socket, peer_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                        };
                        let now = crate::utils::clock::Instant::now();
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

                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Held for the life of the connection, so the cap counts live
                                // sessions rather than accepts.
                                let _permit = permit;
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
                            })
                            .await;
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

/// What the server expects from the peer next.
///
/// ra_svn messages are not self-describing: `( ANONYMOUS ( … ) )` and `( get-dir ( … ) )` are
/// the same shape, and only the position in the handshake tells them apart. The one exception
/// is the client's capability tuple, which begins with a **number** where every other client
/// message begins with a word — so a peer that never sends one (a `nc` session, this repo's own
/// mocked e2e tests) is taken straight into the command loop instead of being held at a
/// handshake it does not know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The greeting has gone out. Next is the client's capability tuple — or, from a peer that
    /// skips the handshake, a command.
    AwaitingCapabilities,
    /// The capability tuple arrived and was answered. ra_svn says the next message is the
    /// client's choice of mechanism.
    AwaitingAuthResponse,
    /// Commands from here on.
    Session,
}

/// One connection's identity, so the write helpers below take a handful of arguments rather
/// than a paragraph of them.
#[derive(Clone, Copy)]
struct ConnRef<'a> {
    app_state: &'a Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    peer_addr: SocketAddr,
}

/// What happened to one handler's answer on the wire.
struct Delivered {
    /// At least one byte was written.
    wrote: bool,
    /// The handler asked to hang up.
    close_requested: bool,
    /// The socket failed; the connection is finished either way.
    write_failed: bool,
}

/// Write one execution result's outputs to the peer.
///
/// `prefix` is written once, immediately before the first output, and is how the trivial
/// auth-request that precedes every command response in a real session gets onto the wire
/// without every action having to know about it.
async fn deliver_results(
    results: Vec<crate::llm::actions::protocol_trait::ActionResult>,
    prefix: Option<&[u8]>,
    write_half: &Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
    conn: &ConnRef<'_>,
    log: &Log<'_>,
) -> Delivered {
    let ConnRef {
        app_state,
        server_id,
        connection_id,
        peer_addr,
    } = *conn;
    use crate::llm::actions::protocol_trait::ActionResult;

    let mut delivered = Delivered {
        wrote: false,
        close_requested: false,
        write_failed: false,
    };

    for result in results {
        match result {
            ActionResult::Output(output_data) => {
                let mut payload = Vec::new();
                if !delivered.wrote {
                    if let Some(prefix) = prefix {
                        payload.extend_from_slice(prefix);
                    }
                }
                payload.extend_from_slice(&output_data);
                delivered.wrote = true;

                {
                    let mut writer = write_half.lock().await;
                    if let Err(e) = writer.write_all(&payload).await {
                        log.error(format!("SVN write error to {}: {}", peer_addr, e));
                        delivered.write_failed = true;
                        return delivered;
                    }
                    let _ = writer.flush().await;
                }

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

                // Summary + full payload FileOnly: the send_svn_* action template already
                // reports the send to the TUI.
                log.debug(format!("SVN sent {} bytes to {}", payload.len(), peer_addr));
                log.trace(format!(
                    "SVN response: {}",
                    String::from_utf8_lossy(&payload)
                ));
            }
            ActionResult::CloseConnection => {
                delivered.close_requested = true;
            }
            _ => {}
        }
    }

    delivered
}

/// The client's capability tuple, as event data for the model.
///
/// `( version:number ( cap:word … ) url:string ? ra-client:string ( ? client:string ) )`
fn capabilities_event_data(
    item: &wire::Item,
    client_ip: &str,
) -> (serde_json::Value, Option<String>) {
    let elements: &[wire::Item] = match item {
        wire::Item::List(items) => items,
        _ => &[],
    };

    let version = match elements.first() {
        Some(wire::Item::Number(n)) => *n,
        _ => 0,
    };
    let capabilities: Vec<String> = match elements.get(1) {
        Some(wire::Item::List(caps)) => caps
            .iter()
            .map(|c| match c {
                wire::Item::Word(w) => w.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let text = |i: Option<&wire::Item>| -> Option<String> {
        match i {
            Some(wire::Item::Str(bytes)) => Some(String::from_utf8_lossy(bytes).into_owned()),
            Some(wire::Item::Word(w)) => Some(w.clone()),
            _ => None,
        }
    };
    let url = text(elements.get(2));
    let ra_client = text(elements.get(3));

    (
        serde_json::json!({
            "version": version,
            "capabilities": capabilities,
            "url": url.clone().unwrap_or_default(),
            "ra_client": ra_client.unwrap_or_default(),
            "client_ip": client_ip,
        }),
        url,
    )
}

/// The client's answer to the auth-request: `( mech:word [ ( token:string ) ] )`.
///
/// The token is a counted string that may contain anything — the real `svn` client's
/// `ANONYMOUS` token is base64 with a trailing newline, which is precisely the case a line
/// reader cannot survive. It is trimmed for display only; the bytes are the model's to judge.
fn auth_event_data(item: &wire::Item, url: Option<&str>, client_ip: &str) -> serde_json::Value {
    let elements: &[wire::Item] = match item {
        wire::Item::List(items) => items,
        _ => &[],
    };

    let mechanism = match elements.first() {
        Some(wire::Item::Word(w)) => w.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let token = match elements.get(1) {
        Some(wire::Item::List(inner)) => match inner.first() {
            Some(wire::Item::Str(bytes)) => String::from_utf8_lossy(bytes).trim().to_string(),
            Some(other) => other.to_string(),
            None => String::new(),
        },
        Some(wire::Item::Str(bytes)) => String::from_utf8_lossy(bytes).trim().to_string(),
        _ => String::new(),
    };

    serde_json::json!({
        "mechanism": mechanism,
        "token": token,
        "url": url.unwrap_or_default(),
        "client_ip": client_ip,
    })
}

/// A command tuple, as event data.
///
/// ra_svn spells a command `( command-name:word ( params… ) )`, so the params list is unwrapped
/// into `args` — that is the shape the model is being asked to answer, and leaving it wrapped
/// would make every argument index one deeper than the protocol document says. A tuple that is
/// not that shape (`( get-latest-rev )`, or a peer improvising) keeps its remaining elements.
///
/// Public for `fuzz/fuzz_targets/svn_tuple.rs`: this is where a parsed item is walked
/// recursively (`to_string`, `to_json`), so it is the half of the path `MAX_TUPLE_DEPTH`
/// protects — the parser itself is iterative.
pub fn command_event_data(item: &wire::Item, client_ip: &str) -> (String, serde_json::Value) {
    let elements: &[wire::Item] = match item {
        wire::Item::List(items) => items,
        _ => std::slice::from_ref(item),
    };

    let command = match elements.first() {
        Some(wire::Item::Word(w)) => w.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };

    let rest = elements.get(1..).unwrap_or(&[]);
    let args: Vec<serde_json::Value> = match rest {
        [wire::Item::List(params)] => params.iter().map(wire::Item::to_json).collect(),
        other => other.iter().map(wire::Item::to_json).collect(),
    };

    (
        command.clone(),
        serde_json::json!({
            "command_line": item.to_string(),
            "command": command,
            "args": args,
            "client_ip": client_ip,
        }),
    )
}

/// Does this look like `( MECHANISM ( token ) )` rather than a command?
///
/// ra_svn's mechanism names come from the SASL registry and are upper-case (`ANONYMOUS`,
/// `CRAM-MD5`, `EXTERNAL`); its command names are lower-case and hyphenated (`get-latest-rev`,
/// `stat`, `check-path`). The distinction is needed because a handler may answer the
/// capability tuple with an **empty** mechanism list, which ra_svn reads as "no authentication
/// required" — the client then sends no auth response at all and its next message is a
/// command. Position alone would misread that as a mechanism choice.
fn looks_like_auth_response(item: &wire::Item) -> bool {
    match item.head() {
        Some(wire::Item::Word(word)) => {
            !word.is_empty() && !word.chars().any(|c| c.is_ascii_lowercase())
        }
        _ => false,
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
    // Framed on tuple structure, not on newlines: see `wire.rs`.
    let mut items = wire::ItemReader::new(reader);
    let log = Log::new(Some(&status_tx));
    let client_ip = peer_addr.ip().to_string();
    let conn = ConnRef {
        app_state: &app_state,
        server_id,
        connection_id,
        peer_addr,
    };

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
        serde_json::json!({ "client_ip": client_ip }),
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
            let delivered = deliver_results(
                execution_result.protocol_results,
                None,
                &write_half,
                &conn,
                &log,
            )
            .await;

            if delivered.write_failed {
                app_state
                    .remove_peer_handle(server_id, connection_id.as_u32())
                    .await;
                return;
            }

            if delivered.close_requested {
                log.info(format!(
                    "SVN greeting from {} decision=model_close",
                    peer_addr
                ));
                close_svn_connection(&app_state, server_id, connection_id, &status_tx).await;
                return;
            }

            if !delivered.wrote {
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
                None,
            )
            .await;
            close_svn_connection(&app_state, server_id, connection_id, &status_tx).await;
            return;
        }
    }

    // Main message loop.
    let mut phase = Phase::AwaitingCapabilities;
    // True once the peer has completed the ra_svn handshake, which is what decides whether a
    // command response carries the trivial auth-request prefix.
    let mut ra_svn_session = false;
    // The URL the client asked for, carried from the capability tuple into the auth event so a
    // handler can answer with a repository root the client will accept.
    let mut session_url: Option<String> = None;
    // False until this client has sent something, which is what separates "took the greeting
    // and went quiet" from "is in a session".
    let mut answered_one = false;

    loop {
        // Bounded in size *and* in time. `read_item` refuses at MAX_COMMAND_BYTES, at
        // MAX_TUPLE_DEPTH nested lists and at any declared string length that would exceed
        // the budget; the deadline stops a peer that sends no bytes at all.
        //
        // The deadline wraps this read alone. The LLM round-trip that answers the message, and
        // a `manual` rule parking it for a human (`src/state/intercepts.rs`, 300s by default),
        // both happen below once a whole tuple has arrived — neither is inside the deadline, so
        // neither can be cut short by it.
        let read_timeout = if answered_one {
            IDLE_BETWEEN_COMMANDS_TIMEOUT
        } else {
            FIRST_COMMAND_READ_TIMEOUT
        };

        let read =
            match tokio::time::timeout(read_timeout, items.read_item(MAX_COMMAND_BYTES)).await {
                Ok(read) => read,
                Err(_) => {
                    log.info(format!(
                        "SVN client {} sent nothing for {}s; closing idle connection",
                        peer_addr,
                        read_timeout.as_secs()
                    ));
                    break;
                }
            };
        answered_one = true;

        let (item, consumed) = match read {
            Ok(Some(message)) => message,
            Ok(None) => {
                log.info(format!("SVN client {} disconnected", peer_addr));
                break;
            }
            Err(wire::WireError::Io(e)) => {
                log.error(format!("SVN read error from {}: {}", peer_addr, e));
                break;
            }
            Err(wire::WireError::UnexpectedEof) => {
                log.info(format!(
                    "SVN client {} closed the connection mid-tuple",
                    peer_addr
                ));
                break;
            }
            Err(e) => {
                // A refused frame is answered in svn's own vocabulary — apr-err 210004,
                // SVN_ERR_RA_SVN_MALFORMED_DATA — with a fixed string. The reason the bound
                // fired goes to the log; the peer learns only that the message was refused.
                log.warn(format!(
                    "SVN refused a message from {} decision=fail_closed_framing: {}",
                    peer_addr, e
                ));
                write_svn_malformed(&write_half, &app_state, server_id, connection_id, &log).await;
                break;
            }
        };

        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(consumed),
                None,
                Some(1),
                None,
            )
            .await;

        log.debug(format!(
            "SVN received {} bytes from {}",
            consumed, peer_addr
        ));
        log.trace(format!("SVN message: {}", item));

        // Which of the three client messages this is. Only the capability tuple can be
        // recognised from its own bytes (it opens with a number); the rest is position.
        let looks_like_capabilities = matches!(item.head(), Some(wire::Item::Number(_)));
        let (event, label, prefix): (Event, String, Option<&[u8]>) = match phase {
            Phase::AwaitingCapabilities if looks_like_capabilities => {
                let (data, url) = capabilities_event_data(&item, &client_ip);
                session_url = url;
                phase = Phase::AwaitingAuthResponse;
                (
                    Event::new(&SVN_CLIENT_CAPABILITIES_EVENT, data),
                    "capabilities".to_string(),
                    None,
                )
            }
            Phase::AwaitingAuthResponse if looks_like_auth_response(&item) => {
                let data = auth_event_data(&item, session_url.as_deref(), &client_ip);
                phase = Phase::Session;
                // The handshake is complete from here: every command response the client
                // reads is preceded by an auth-request it consumes silently.
                ra_svn_session = true;
                (
                    Event::new(&SVN_AUTH_RESPONSE_EVENT, data),
                    "auth-response".to_string(),
                    None,
                )
            }
            _ => {
                if phase == Phase::AwaitingAuthResponse {
                    // The handler answered the capability tuple with an empty mechanism list,
                    // which ra_svn reads as "no authentication required": the client sends no
                    // auth response and its next message is already a command. The handshake
                    // is complete all the same, so command replies still need the prefix.
                    ra_svn_session = true;
                }
                phase = Phase::Session;
                let (command, data) = command_event_data(&item, &client_ip);
                (
                    Event::new(&SVN_COMMAND_EVENT, data),
                    command,
                    if ra_svn_session {
                        Some(COMMAND_AUTH_PREFIX)
                    } else {
                        None
                    },
                )
            }
        };

        log.debug(format!("SVN calling LLM for {} from {}", label, peer_addr));

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

                let delivered = deliver_results(
                    execution_result.protocol_results,
                    prefix,
                    &write_half,
                    &conn,
                    &log,
                )
                .await;

                if delivered.write_failed {
                    app_state
                        .remove_peer_handle(server_id, connection_id.as_u32())
                        .await;
                    return;
                }

                if delivered.close_requested {
                    log.debug(format!(
                        "SVN '{}' from {} decision=model_close",
                        label, peer_addr
                    ));
                    break;
                }

                if !delivered.wrote {
                    // Answered with nothing: a real answer (static handler with no
                    // actions, or a human choosing silence), kept distinct in the
                    // log from the backend-failure path below.
                    log.warn(format!(
                        "SVN '{}' from {} decision=no_action (nothing written)",
                        label, peer_addr
                    ));
                }
            }
            Err(e) => {
                // Same rule as the greeting: category on the wire, error in the
                // log. Without this the peer sat blocked on a message it had
                // already sent until its own timeout expired.
                let failure = WireFailure::classify(&e);
                log.error(format!(
                    "SVN '{}' from {} decision=fail_closed_llm_error category={} error: {}",
                    label,
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
                    prefix,
                )
                .await;
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
    prefix: Option<&[u8]>,
) {
    // In a real session the client is waiting for the trivial auth-request before the answer,
    // so a bare failure tuple would be read as that auth-request and desynchronise the stream:
    // the peer would report malformed data instead of the refusal it was actually sent.
    let mut payload = prefix.unwrap_or(&[]).to_vec();
    payload.extend_from_slice(&svn_failure_tuple(failure));
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

/// Refuse a message the framing layer would not accept.
///
/// apr-err 210004 is `SVN_ERR_RA_SVN_MALFORMED_DATA` — svn's own name for "that was not a
/// tuple I can read", which is exactly what a depth bomb, an oversized message or a stray
/// paren is. The string is a byte literal for the same reason [`WireFailure::text`] returns
/// `&'static str`: nothing about *why* the bound fired may reach the peer.
const MALFORMED_REFUSAL: &[u8] =
    b"( failure ( ( 210004 33:netget: refused malformed message 0: 0 ) ) )\n";

async fn write_svn_malformed(
    write_half: &Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
    app_state: &Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    log: &Log<'_>,
) {
    let written = {
        let mut writer = write_half.lock().await;
        match writer.write_all(MALFORMED_REFUSAL).await {
            Ok(()) => {
                let _ = writer.flush().await;
                true
            }
            Err(e) => {
                log.debug(format!("SVN could not write refusal tuple: {}", e));
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
                Some(MALFORMED_REFUSAL.len() as u64),
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
