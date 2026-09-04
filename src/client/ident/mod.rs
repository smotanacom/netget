//! Ident (RFC 1413) client.
//!
//! Connect, send `<server-port> , <client-port>`, read one line, close. The whole protocol
//! fits in a paragraph, so what this file is really about is the two things a *client* can get
//! wrong:
//!
//! 1. **The reply must answer the question that was asked.** RFC 1413 §3 has the client match
//!    a reply to its query by the port pair. A reply carrying a different pair is an answer
//!    about somebody else's connection; accepting it would attribute an account to the wrong
//!    socket. [`parse_ident_reply`] extracts the pair, the session compares it with what went
//!    out, and a mismatch raises [`actions::IDENT_CLIENT_REPLY_MISMATCH_EVENT`] instead of a
//!    result. The same applies to a line that cannot be parsed, one that overflows the cap,
//!    and one that arrives with no query outstanding — rejected, never parsed loosely.
//! 2. **The model's answer is carried out.** The reply event is put to the model and whatever
//!    it asks for is executed, including another query — which RFC 1413 requires to be a fresh
//!    connection, since the server closes after one exchange. That chain is bounded by
//!    [`MAX_FOLLOWUP_DEPTH`] rather than by silence: the recursive step is boxed, which is what
//!    makes an action → event → action cycle expressible at all.
//!
//! Whitespace around the comma is tolerated in both directions, because the grammar admits it
//! and real peers send it.
//!
//! **Nothing here reads host state.** The port pair is the model's choice, never derived from a
//! local socket table; the userid that comes back is reported in the event and nowhere else.

pub mod actions;

pub use actions::IdentClientProtocol;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, StartupParams};
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

use actions::{
    IDENT_CLIENT_CONNECTED_EVENT, IDENT_CLIENT_ERROR_RECEIVED_EVENT,
    IDENT_CLIENT_REPLY_MISMATCH_EVENT, IDENT_CLIENT_RESPONSE_RECEIVED_EVENT,
};

/// The only port RFC 1413 defines. A real identd listens here and nowhere else, which is
/// exactly why `ident_port` exists as an override for a harness.
pub const DEFAULT_IDENT_PORT: u16 = 113;

/// Default wait for the single reply line.
const DEFAULT_RESPONSE_TIMEOUT_SECS: u64 = 30;

/// RFC 1413 §5 bounds a reply line; anything past this without a newline is not ident.
/// Deliberately the same cap the server half applies to queries.
pub const MAX_REPLY_BYTES: usize = 1024;

/// How many further queries the model may chain off a reply. Each one is a new TCP connection
/// (RFC 1413 is one exchange per connection), so this bounds real sockets, not just recursion.
pub const MAX_FOLLOWUP_DEPTH: usize = 4;

/// The four tokens RFC 1413 defines. Anything else is accepted only in the `X`-prefixed
/// implementation-defined form the RFC reserves.
const IDENT_ERROR_TOKENS: [&str; 4] = ["INVALID-PORT", "NO-USER", "HIDDEN-USER", "UNKNOWN-ERROR"];

/// The port pair this connection asked about, once something has sent one.
///
/// Shared between the session task and the command task, because either can be the thing that
/// puts a query on the wire and the reply check needs the pair whichever did.
type PendingQuery = Arc<std::sync::Mutex<Option<(u16, u16)>>>;

/// What a parsed reply line turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentReply {
    /// `<pair> : USERID : <opsys>[,<charset>] : <userid>`
    Userid {
        server_port: u16,
        client_port: u16,
        opsys: String,
        charset: Option<String>,
        userid: String,
    },
    /// `<pair> : ERROR : <token>`
    Error {
        server_port: u16,
        client_port: u16,
        token: String,
    },
    /// Not an ident reply. `reason` is a stable token, reported to the model and greppable in
    /// the log.
    Malformed { reason: &'static str },
}

impl IdentReply {
    /// The port pair the reply claims to be about, when it has one.
    pub fn port_pair(&self) -> Option<(u16, u16)> {
        match self {
            IdentReply::Userid {
                server_port,
                client_port,
                ..
            }
            | IdentReply::Error {
                server_port,
                client_port,
                ..
            } => Some((*server_port, *client_port)),
            IdentReply::Malformed { .. } => None,
        }
    }
}

/// `<port> , <port>` with any amount of whitespace around the comma.
///
/// Both ports must be real TCP ports. `0` is out of range in RFC 1413 (there is no connection
/// on port 0 to own), and rejecting it here is what keeps a `0 , 0` reply from matching a
/// query nothing sent.
fn parse_port_pair(text: &str) -> Option<(u16, u16)> {
    let (left, right) = text.split_once(',')?;
    let server_port: u32 = left.trim().parse().ok()?;
    let client_port: u32 = right.trim().parse().ok()?;
    if !(1..=65535).contains(&server_port) || !(1..=65535).contains(&client_port) {
        return None;
    }
    Some((server_port as u16, client_port as u16))
}

/// An error token is one of the four, or an `X`-prefixed one the RFC reserves for
/// implementations. Anything else is a malformed reply, not free text to hand the model.
fn is_error_token(token: &str) -> bool {
    if IDENT_ERROR_TOKENS.contains(&token) {
        return true;
    }
    token.starts_with('X')
        && token.len() > 1
        && token
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
}

/// Parse one reply line.
///
/// Strict on purpose. Every field position in RFC 1413's grammar carries meaning to whoever
/// reads the result, so a line that does not fill them is reported as malformed rather than
/// half-read: a `USERID` line with no userid must not become an empty username, and an
/// unrecognised response type must not become an error token.
///
/// Whitespace is tolerated everywhere the grammar allows it — around the commas, around the
/// colons, and at either end of the line.
pub fn parse_ident_reply(line: &str) -> IdentReply {
    if line.len() > MAX_REPLY_BYTES {
        return IdentReply::Malformed {
            reason: "oversized",
        };
    }

    let line = line.trim();
    let Some((pair_text, rest)) = line.split_once(':') else {
        return IdentReply::Malformed {
            reason: "no_response_type",
        };
    };
    let Some((server_port, client_port)) = parse_port_pair(pair_text) else {
        return IdentReply::Malformed {
            reason: "port_pair",
        };
    };
    let Some((kind, addl)) = rest.split_once(':') else {
        return IdentReply::Malformed {
            reason: "no_addl_info",
        };
    };

    match kind.trim().to_ascii_uppercase().as_str() {
        "USERID" => {
            // splitn-style: the userid keeps any colon it contains, which RFC 1413 permits.
            let Some((opsys_field, userid)) = addl.split_once(':') else {
                return IdentReply::Malformed {
                    reason: "userid_fields",
                };
            };
            let userid = userid.trim();
            if userid.is_empty() {
                return IdentReply::Malformed {
                    reason: "empty_userid",
                };
            }
            let (opsys, charset) = match opsys_field.split_once(',') {
                Some((opsys, charset)) => {
                    let charset = charset.trim();
                    (
                        opsys.trim().to_string(),
                        (!charset.is_empty()).then(|| charset.to_string()),
                    )
                }
                None => (opsys_field.trim().to_string(), None),
            };
            if opsys.is_empty() {
                return IdentReply::Malformed {
                    reason: "empty_opsys",
                };
            }
            IdentReply::Userid {
                server_port,
                client_port,
                opsys,
                charset,
                userid: userid.to_string(),
            }
        }
        "ERROR" => {
            let token = addl.trim().to_ascii_uppercase();
            if !is_error_token(&token) {
                return IdentReply::Malformed {
                    reason: "error_token",
                };
            }
            IdentReply::Error {
                server_port,
                client_port,
                token,
            }
        }
        _ => IdentReply::Malformed {
            reason: "response_type",
        },
    }
}

/// Reduce a line to something safe to put in event data and the log: no control characters, no
/// unbounded length. It has already been rejected by the time this is called; this only makes
/// it readable.
fn sanitize_line(line: &str) -> String {
    let cleaned: String = line.chars().filter(|c| !c.is_control()).collect();
    crate::utils::truncate_for_log(cleaned.trim(), 256)
}

/// Split `host[:port]`, understanding `[::1]:113` and a bare IPv6 literal.
///
/// A colon suffix that is not a number is left as part of the host, so the connect failure
/// names what the caller actually typed rather than silently retargeting port 113.
fn split_host_port(addr: &str) -> (String, Option<u16>) {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.trim().parse().ok());
            return (host.to_string(), port);
        }
    }
    match addr.rsplit_once(':') {
        Some((head, tail)) if !head.contains(':') => match tail.trim().parse::<u16>() {
            Ok(port) => (head.to_string(), Some(port)),
            Err(_) => (addr.to_string(), None),
        },
        _ => (addr.to_string(), None),
    }
}

/// Decide what to actually connect to.
///
/// `ident_port` wins over a port written into `remote_addr`, and 113 is the default, because
/// that is the only port RFC 1413 knows about — `remote_addr` for this protocol is usually
/// just a host.
pub fn resolve_target(remote_addr: &str, ident_port: Option<u16>) -> Result<String> {
    let trimmed = remote_addr.trim();
    let trimmed = trimmed
        .strip_prefix("ident://")
        .unwrap_or(trimmed)
        .trim_end_matches('/');
    let (host, addr_port) = split_host_port(trimmed);
    if host.is_empty() {
        anyhow::bail!("remote_addr {remote_addr:?} names no host to query");
    }
    let port = ident_port.or(addr_port).unwrap_or(DEFAULT_IDENT_PORT);
    Ok(if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    })
}

/// What [`IdentClient::apply_action`] did with one action.
enum Applied {
    /// Bytes written (0 when the action produced no wire output).
    Sent(usize),
    /// The write side was shut down and the session should end.
    Disconnect,
}

/// How reading the single reply line ended.
enum ReplyRead {
    /// A line (newline-terminated, or everything received before a clean EOF).
    Line(String),
    /// The cap was hit with no newline. Rejected without parsing.
    Oversize(String),
    /// The peer closed without sending anything.
    Closed,
    /// Nothing arrived within the configured timeout.
    TimedOut,
    /// The socket errored.
    Failed(String),
}

/// Everything a reply handler needs, owned, so the recursive follow-up step has no lifetimes
/// to thread through a boxed future.
struct SessionCtx {
    protocol: Arc<IdentClientProtocol>,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    client_id: ClientId,
    instruction: String,
    target: String,
    response_timeout: Duration,
}

pub struct IdentClient;

impl IdentClient {
    /// Connect to an ident server and run one query/reply exchange under LLM control.
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        startup_params: Option<StartupParams>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        let ident_port = match &startup_params {
            Some(params) => params.get_optional_u64("ident_port")?,
            None => None,
        };
        let ident_port = match ident_port {
            Some(port) if (1..=65535).contains(&port) => Some(port as u16),
            Some(port) => anyhow::bail!("ident_port must be a TCP port in 1-65535, got {port}"),
            None => None,
        };
        let response_timeout = match &startup_params {
            Some(params) => params.get_optional_u64("response_timeout_secs")?,
            None => None,
        }
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_RESPONSE_TIMEOUT_SECS);
        let response_timeout = Duration::from_secs(response_timeout);

        let target = resolve_target(&remote_addr, ident_port)?;

        let stream = TcpStream::connect(&target)
            .await
            .with_context(|| format!("Failed to connect to ident server at {target}"))?;
        let local_addr = stream.local_addr()?;
        let peer_addr = stream.peer_addr()?;

        Log::new(Some(&status_tx)).info(format!(
            "Ident client {client_id} connected to {peer_addr} (local: {local_addr})"
        ));
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        let (read_half, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(Mutex::new(write_half));
        let protocol = Arc::new(IdentClientProtocol::new());
        let pending: PendingQuery = Arc::new(std::sync::Mutex::new(None));

        // Registered BEFORE the connected-event LLM call. A dashboard-created client can park
        // that event on a manual rule for minutes, and the operator has to be able to send the
        // query meanwhile; a separate task also means an injected query never queues behind an
        // LLM round-trip.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let command_task = tokio::spawn(Self::command_loop(
            command_rx,
            protocol.clone(),
            write_half.clone(),
            pending.clone(),
            client_id,
            app_state.clone(),
            status_tx.clone(),
        ));
        app_state
            .register_client_task(client_id, command_task)
            .await;

        let session_registrar = app_state.clone();
        let session_task = tokio::spawn(async move {
            Self::session(
                read_half,
                write_half,
                protocol,
                pending,
                target,
                response_timeout,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                client_id,
            )
            .await;
            // Session over: drop the handle so the rail stops offering [ send ], which also
            // ends the command task with its channel.
            app_state.remove_client_handle(client_id).await;
            let _ = status_tx.send("__UPDATE_UI__".to_string());
        });
        session_registrar
            .register_client_task(client_id, session_task)
            .await;

        Ok(local_addr)
    }

    /// Connected event → query → the one reply → the model's answer to it (and any follow-ups).
    #[allow(clippy::too_many_arguments)]
    async fn session(
        mut read_half: tokio::io::ReadHalf<TcpStream>,
        write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
        protocol: Arc<IdentClientProtocol>,
        pending: PendingQuery,
        target: String,
        response_timeout: Duration,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) {
        let log = Log::new(Some(&status_tx));

        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let ctx = Arc::new(SessionCtx {
            protocol: protocol.clone(),
            llm_client: llm_client.clone(),
            app_state: app_state.clone(),
            status_tx: status_tx.clone(),
            client_id,
            instruction: instruction.clone(),
            target: target.clone(),
            response_timeout,
        });

        // === The connected event: the model chooses the port pair ===
        let event = Event::new(
            &IDENT_CLIENT_CONNECTED_EVENT,
            serde_json::json!({ "remote_addr": target }),
        );

        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        match call_llm_for_client(
            &llm_client,
            &app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            protocol.as_ref(),
            &status_tx,
        )
        .await
        {
            Ok(ClientLlmResult {
                actions,
                memory_updates,
            }) => {
                if let Some(memory) = memory_updates {
                    app_state.set_memory_for_client(client_id, memory).await;
                }
                for action in actions {
                    let result = match protocol.execute_action(action) {
                        Ok(result) => result,
                        Err(e) => {
                            log.error(format!("Ident client {client_id} rejected action: {e}"));
                            continue;
                        }
                    };
                    match Self::apply_action(result, &write_half, &pending, client_id, &log).await {
                        Ok(Applied::Sent(_)) => {}
                        Ok(Applied::Disconnect) => {
                            log.info(format!(
                                "Ident client {client_id} disconnected before asking anything"
                            ));
                            app_state
                                .update_client_status(client_id, ClientStatus::Disconnected)
                                .await;
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            return;
                        }
                        Err(e) => {
                            log.error(format!(
                                "Ident client {client_id} failed to send query: {e}"
                            ));
                            app_state
                                .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                                .await;
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                // Stay connected: the operator can still inject the query from the dashboard,
                // and the server will close on its own otherwise.
                log.error(format!(
                    "Ident client {client_id} LLM error on connect: {e} (still connected; a \
                     query can be injected)"
                ));
            }
        }

        // === The one reply ===
        match Self::read_reply(&mut read_half, response_timeout).await {
            ReplyRead::Line(line) => {
                log.trace(format!("Ident client {client_id} reply line: {line:?}"));
                let asked = pending.lock().ok().and_then(|p| *p);
                let reply = parse_ident_reply(&line);
                Self::handle_reply(ctx, 0, reply, asked, line).await;
            }
            ReplyRead::Oversize(line) => {
                let asked = pending.lock().ok().and_then(|p| *p);
                log.warn(format!(
                    "Ident client {client_id} reply exceeded {MAX_REPLY_BYTES} bytes with no \
                     newline; rejected without parsing"
                ));
                Self::handle_reply(
                    ctx,
                    0,
                    IdentReply::Malformed {
                        reason: "oversized",
                    },
                    asked,
                    line,
                )
                .await;
            }
            ReplyRead::Closed => {
                let asked = pending.lock().ok().and_then(|p| *p);
                match asked {
                    Some((server_port, client_port)) => log.info(format!(
                        "Ident client {client_id} got no answer for {server_port},{client_port}: \
                         the server closed first"
                    )),
                    None => log.info(format!(
                        "Ident client {client_id} was closed by the server before any query went \
                         out"
                    )),
                }
            }
            ReplyRead::TimedOut => log.warn(format!(
                "Ident client {client_id} gave up after {}s with no reply",
                response_timeout.as_secs()
            )),
            ReplyRead::Failed(e) => {
                log.error(format!("Ident client {client_id} read error: {e}"));
                app_state
                    .update_client_status(client_id, ClientStatus::Error(e))
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                return;
            }
        }

        // RFC 1413 is one exchange per connection: there is nothing left to do here.
        let _ = write_half.lock().await.shutdown().await;
        app_state
            .update_client_status(client_id, ClientStatus::Disconnected)
            .await;
        log.info(format!("Ident client {client_id} disconnected"));
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Turn a reply into the right event, put it to the model, and carry out the answer.
    ///
    /// Boxed because it is mutually recursive with [`Self::followup_query`]: an `async fn` that
    /// can reach itself has an infinitely-sized future (E0391), and `+ Send` is spelled out
    /// because this is awaited inside a `tokio::spawn` and inference will not supply it.
    fn handle_reply(
        ctx: Arc<SessionCtx>,
        depth: usize,
        reply: IdentReply,
        asked: Option<(u16, u16)>,
        line: String,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let log = Log::new(Some(&ctx.status_tx));
            let client_id = ctx.client_id;

            // The port-pair check. This is the one correctness property an ident client has:
            // a reply about a different pair is an answer to a question nobody asked.
            let event = match (&reply, asked, reply.port_pair()) {
                (IdentReply::Malformed { reason }, _, _) => {
                    log.warn(format!(
                        "Ident client {client_id} rejected a reply ({reason}): {}",
                        sanitize_line(&line)
                    ));
                    Self::mismatch_event(reason, asked, None, &line)
                }
                (_, None, got) => {
                    log.warn(format!(
                        "Ident client {client_id} received a reply with no query outstanding; \
                         rejected"
                    ));
                    Self::mismatch_event("unsolicited", None, got, &line)
                }
                (_, Some(query), Some(got)) if query != got => {
                    log.warn(format!(
                        "Ident client {client_id} asked about {},{} and was answered about \
                         {},{}; rejected (RFC 1413 matches a reply by its port pair)",
                        query.0, query.1, got.0, got.1
                    ));
                    Self::mismatch_event("port_pair_mismatch", asked, Some(got), &line)
                }
                (
                    IdentReply::Userid {
                        server_port,
                        client_port,
                        opsys,
                        charset,
                        userid,
                    },
                    _,
                    _,
                ) => {
                    log.info(format!(
                        "Ident client {client_id} {server_port},{client_port} USERID on {opsys}"
                    ));
                    let mut data = serde_json::json!({
                        "server_port": server_port,
                        "client_port": client_port,
                        "userid": userid,
                        "opsys": opsys,
                    });
                    if let Some(charset) = charset {
                        data["charset"] = serde_json::Value::String(charset.clone());
                    }
                    Event::new(&IDENT_CLIENT_RESPONSE_RECEIVED_EVENT, data)
                }
                (
                    IdentReply::Error {
                        server_port,
                        client_port,
                        token,
                    },
                    _,
                    _,
                ) => {
                    log.info(format!(
                        "Ident client {client_id} {server_port},{client_port} ERROR {token}"
                    ));
                    Event::new(
                        &IDENT_CLIENT_ERROR_RECEIVED_EVENT,
                        serde_json::json!({
                            "server_port": server_port,
                            "client_port": client_port,
                            "error_token": token,
                        }),
                    )
                }
            };

            let memory = ctx
                .app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            let outcome = call_llm_for_client(
                &ctx.llm_client,
                &ctx.app_state,
                client_id.to_string(),
                &ctx.instruction,
                &memory,
                Some(&event),
                ctx.protocol.as_ref(),
                &ctx.status_tx,
            )
            .await;

            let ClientLlmResult {
                actions,
                memory_updates,
            } = match outcome {
                Ok(result) => result,
                Err(e) => {
                    log.error(format!(
                        "Ident client {client_id} LLM error on {}: {e}",
                        event.id()
                    ));
                    return;
                }
            };
            if let Some(memory) = memory_updates {
                ctx.app_state.set_memory_for_client(client_id, memory).await;
            }

            // Carry out what the model asked for. A follow-up query is a real new connection,
            // because the server closed this one after answering.
            for action in actions {
                match ctx.protocol.execute_action(action) {
                    Ok(ClientActionResult::Custom { name, data }) if name == "ident_query" => {
                        let (Some(server_port), Some(client_port)) = (
                            data.get("server_port").and_then(|v| v.as_u64()),
                            data.get("client_port").and_then(|v| v.as_u64()),
                        ) else {
                            continue;
                        };
                        if depth + 1 > MAX_FOLLOWUP_DEPTH {
                            log.warn(format!(
                                "Ident client {client_id} stopped at follow-up depth \
                                 {MAX_FOLLOWUP_DEPTH}; the request for {server_port},\
                                 {client_port} was not sent"
                            ));
                            continue;
                        }
                        Self::followup_query(
                            ctx.clone(),
                            depth + 1,
                            server_port as u16,
                            client_port as u16,
                        )
                        .await;
                    }
                    Ok(ClientActionResult::Disconnect) => {
                        log.debug(format!(
                            "Ident client {client_id} was asked to disconnect; the connection is \
                             already finished"
                        ));
                        break;
                    }
                    Ok(ClientActionResult::WaitForMore) => log.debug(format!(
                        "Ident client {client_id} asked to wait, but RFC 1413 sends one line and \
                         closes; nothing more will arrive"
                    )),
                    Ok(_) => {}
                    Err(e) => log.error(format!("Ident client {client_id} rejected action: {e}")),
                }
            }
        })
    }

    /// One further query on a **fresh** connection, then straight back into
    /// [`Self::handle_reply`] so the answer raises its event like any other.
    ///
    /// The new connection is not a shortcut: RFC 1413 is one query per connection and the
    /// server has already closed. Raising the event (rather than logging the answer and
    /// stopping, which is what `whois` does) is what keeps the chain alive; the depth bound is
    /// what keeps it finite.
    async fn followup_query(
        ctx: Arc<SessionCtx>,
        depth: usize,
        server_port: u16,
        client_port: u16,
    ) {
        let log = Log::new(Some(&ctx.status_tx));
        let client_id = ctx.client_id;

        let mut stream = match TcpStream::connect(&ctx.target).await {
            Ok(stream) => stream,
            Err(e) => {
                log.error(format!(
                    "Ident client {client_id} follow-up could not reach {}: {e}",
                    ctx.target
                ));
                return;
            }
        };

        let query = format!("{server_port} , {client_port}\r\n");
        if let Err(e) = stream.write_all(query.as_bytes()).await {
            log.error(format!(
                "Ident client {client_id} follow-up write failed: {e}"
            ));
            return;
        }
        if let Err(e) = stream.flush().await {
            log.error(format!(
                "Ident client {client_id} follow-up flush failed: {e}"
            ));
            return;
        }
        log.debug(format!(
            "Ident client {client_id} follow-up #{depth} asked about {server_port},{client_port}"
        ));

        let (mut read_half, write_half) = tokio::io::split(stream);
        let outcome = Self::read_reply(&mut read_half, ctx.response_timeout).await;
        let mut write_half = write_half;
        let _ = write_half.shutdown().await;

        match outcome {
            ReplyRead::Line(line) => {
                let reply = parse_ident_reply(&line);
                Self::handle_reply(ctx, depth, reply, Some((server_port, client_port)), line).await;
            }
            ReplyRead::Oversize(line) => {
                Self::handle_reply(
                    ctx,
                    depth,
                    IdentReply::Malformed {
                        reason: "oversized",
                    },
                    Some((server_port, client_port)),
                    line,
                )
                .await;
            }
            ReplyRead::Closed => log.info(format!(
                "Ident client {client_id} follow-up for {server_port},{client_port} was closed \
                 unanswered"
            )),
            ReplyRead::TimedOut => log.warn(format!(
                "Ident client {client_id} follow-up for {server_port},{client_port} timed out"
            )),
            ReplyRead::Failed(e) => log.error(format!(
                "Ident client {client_id} follow-up read error: {e}"
            )),
        }
    }

    /// Build the "this is not my answer" event.
    fn mismatch_event(
        reason: &str,
        asked: Option<(u16, u16)>,
        got: Option<(u16, u16)>,
        line: &str,
    ) -> Event {
        let mut data = serde_json::json!({
            "reason": reason,
            "reply_line": sanitize_line(line),
        });
        if let Some((server_port, client_port)) = asked {
            data["queried_server_port"] = serde_json::json!(server_port);
            data["queried_client_port"] = serde_json::json!(client_port);
        }
        if let Some((server_port, client_port)) = got {
            data["reply_server_port"] = serde_json::json!(server_port);
            data["reply_client_port"] = serde_json::json!(client_port);
        }
        Event::new(&IDENT_CLIENT_REPLY_MISMATCH_EVENT, data)
    }

    /// Accumulate until a newline, EOF, the byte cap, or the timeout.
    async fn read_reply<R>(reader: &mut R, timeout: Duration) -> ReplyRead
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut accumulated: Vec<u8> = Vec::new();
        let mut buffer = vec![0u8; 512];

        loop {
            let read = match tokio::time::timeout(timeout, reader.read(&mut buffer)).await {
                Err(_) => return ReplyRead::TimedOut,
                Ok(read) => read,
            };
            match read {
                Ok(0) => {
                    if accumulated.is_empty() {
                        return ReplyRead::Closed;
                    }
                    // A peer that omitted the CRLF and hung up still said something; the
                    // parser gets to judge it.
                    break;
                }
                Ok(n) => {
                    accumulated.extend_from_slice(&buffer[..n]);
                    if accumulated.contains(&b'\n') {
                        break;
                    }
                    if accumulated.len() > MAX_REPLY_BYTES {
                        let text = String::from_utf8_lossy(&accumulated).into_owned();
                        return ReplyRead::Oversize(text);
                    }
                }
                Err(e) => return ReplyRead::Failed(e.to_string()),
            }
        }

        let text = String::from_utf8_lossy(&accumulated).into_owned();
        // Only the first line matters: RFC 1413 is one reply per connection.
        ReplyRead::Line(text.lines().next().unwrap_or("").to_string())
    }

    /// Put one executed action on the wire.
    ///
    /// Shared by the LLM path and by injected dashboard commands, so the encoding of
    /// `send_ident_query` — and the record of which pair was asked about — exists exactly once.
    async fn apply_action<W>(
        result: ClientActionResult,
        write_half: &Arc<Mutex<W>>,
        pending: &PendingQuery,
        client_id: ClientId,
        log: &Log<'_>,
    ) -> Result<Applied>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        match result {
            ClientActionResult::Custom { name, data } if name == "ident_query" => {
                let server_port =
                    data.get("server_port")
                        .and_then(|v| v.as_u64())
                        .context("ident_query without a server_port")? as u16;
                let client_port =
                    data.get("client_port")
                        .and_then(|v| v.as_u64())
                        .context("ident_query without a client_port")? as u16;

                let query = format!("{server_port} , {client_port}\r\n");
                {
                    let mut writer = write_half.lock().await;
                    writer.write_all(query.as_bytes()).await?;
                    writer.flush().await?;
                }
                log.debug(format!(
                    "Ident client {client_id} asked about {server_port},{client_port}"
                ));
                if let Ok(mut slot) = pending.lock() {
                    // The first query is the one the reply answers; RFC 1413 allows only one
                    // per connection, so a second is written but cannot change what we match.
                    slot.get_or_insert((server_port, client_port));
                }
                Ok(Applied::Sent(query.len()))
            }
            ClientActionResult::Disconnect => {
                // Half-close: the server reads EOF and closes, and the read loop then sees 0
                // and runs its normal path.
                let _ = write_half.lock().await.shutdown().await;
                Ok(Applied::Disconnect)
            }
            // WaitForMore, NoAction, an unknown Custom, SendData, nested Multiple.
            _ => Ok(Applied::Sent(0)),
        }
    }

    /// Drain injected commands until the channel closes or an injected `disconnect` ends the
    /// session.
    ///
    /// `command_support::handle_stream_client_command` cannot run this vocabulary, because
    /// `send_ident_query` yields a `Custom` result: it has to be routed through
    /// [`Self::apply_action`], the same function the LLM path uses, so that the pending port
    /// pair is recorded either way. The access-log entry and the reply match the generic arm.
    #[allow(clippy::too_many_arguments)]
    async fn command_loop<W>(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        protocol: Arc<IdentClientProtocol>,
        write_half: Arc<Mutex<W>>,
        pending: PendingQuery,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) where
        W: tokio::io::AsyncWrite + Unpin,
    {
        use crate::llm::actions::protocol_trait::Protocol;

        while let Some(command) = command_rx.recv().await {
            let log = Log::new(Some(&status_tx));
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => Self::apply_action(result, &write_half, &pending, client_id, &log)
                    .await
                    .map(|applied| match applied {
                        Applied::Disconnect => ClientSendOutcome::Disconnected,
                        Applied::Sent(0) => ClientSendOutcome::Executed {
                            detail: "executed (nothing to write)".to_string(),
                        },
                        Applied::Sent(bytes_sent) => ClientSendOutcome::Sent { bytes_sent },
                    }),
            };

            let outcome_json = match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            };
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![outcome_json],
                )
                .await;

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            if let Err(e) = &outcome {
                log.error(format!(
                    "Ident client {client_id} injected action failed: {e}"
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                // Do not wait for the server's own FIN: the rail must stop offering [ send ].
                app_state.remove_client_handle(client_id).await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                break;
            }
        }
    }
}
