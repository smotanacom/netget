//! Matrix homeserver, client-server API v3. Rust owns HTTP, the API's JSON shapes, access
//! tokens, room membership and per-user `/sync` delivery (`hub.rs`); the handler decides
//! logins (unless `user_passwords` is given), room creation, joins and every message, and
//! speaks in rooms as its own user.
pub mod actions;
pub mod hub;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hub::Hub;
use hyper::{
    body::Incoming,
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_HEADERS: usize = 64;
/// The longest a `/sync` waits for something new, whatever `timeout` the client asks for.
pub const MAX_SYNC_WAIT: Duration = Duration::from_secs(30);
/// Transaction ids remembered for idempotent sends.
pub const MAX_TXNS: usize = 10_000;
/// `/messages` returns at most this many events per page.
pub const MAX_PAGE: usize = 100;
const CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 61\r\nConnection: close\r\nRetry-After: 5\r\n\r\n{\"errcode\":\"M_LIMIT_EXCEEDED\",\"error\":\"Too many connections\"}";

type Reply = Response<Full<Bytes>>;

/// Sent transactions: (user, txn id) → event id, and their order for eviction.
type TxnLog = (
    HashMap<(String, String), String>,
    VecDeque<(String, String)>,
);

struct Shared {
    hub: Mutex<Hub>,
    wake: Notify,
    passwords: Option<HashMap<String, String>>,
    bot: String,
    txns: Mutex<TxnLog>,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let server_name = params
        .map(|p| p.get_optional_string("server_name"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_SERVER_NAME.to_string());
    let bot = params
        .map(|p| p.get_optional_string("bot_user"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_BOT_USER.to_string());
    let passwords = params
        .map(|p| p.get_optional_object("user_passwords"))
        .transpose()?
        .flatten()
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    let pw = v.as_str().map(str::to_string).ok_or_else(|| {
                        anyhow::anyhow!("user_passwords: the password for {k:?} must be a string")
                    })?;
                    Ok((
                        k.trim_start_matches('@')
                            .split(':')
                            .next()
                            .unwrap_or(k)
                            .to_lowercase(),
                        pw,
                    ))
                })
                .collect::<Result<HashMap<_, _>>>()
        })
        .transpose()?;
    let hub = Hub::new(&server_name);
    let bot = hub.user_id(&bot);
    let shared = Arc::new(Shared {
        hub: Mutex::new(hub),
        wake: Notify::new(),
        passwords,
        bot,
        txns: Mutex::new(Default::default()),
    });

    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Matrix homeserver {server_name} listening on {local}"
    ));
    let server_id = ctx.server_id;
    let state = ctx.state.clone();
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                CAP_REFUSAL,
                "Matrix",
                Some(&ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: local,
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
            let child = ctx.clone();
            let shared = shared.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let request_ctx = child.clone();
                    let service = service_fn(move |request| {
                        let ctx = request_ctx.clone();
                        let shared = shared.clone();
                        async move { Ok::<_, Infallible>(handle(request, id, &ctx, &shared).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(MAX_HEADERS)
                        .max_buf_size(MAX_HEADER_BYTES);
                    if let Err(e) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        Log::new(Some(&child.status_tx))
                            .debug(format!("Matrix connection {id} HTTP error: {e}"));
                    }
                    child
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn reply(status: u16, body: &Value) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(
        serde_json::to_vec(body).unwrap_or_default(),
    )));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut()
        .insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    r
}

fn error(status: u16, errcode: &str, message: &str) -> Reply {
    reply(status, &json!({"errcode": errcode, "error": message}))
}

fn failure(e: Option<&anyhow::Error>) -> Reply {
    let kind = e
        .map(crate::utils::WireFailure::classify)
        .unwrap_or(crate::utils::WireFailure::Unavailable);
    if kind.is_overloaded() {
        let mut r = error(503, "M_LIMIT_EXCEEDED", kind.prefixed_text());
        r.headers_mut()
            .insert("Retry-After", HeaderValue::from_static("5"));
        r
    } else {
        error(500, "M_UNKNOWN", kind.prefixed_text())
    }
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, op: &str, decision: &str) {
    let line = format!("Matrix connection {id} operation={op} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed") || decision == "model_silent" {
        log.error(line)
    } else {
        log.info(line)
    }
}

/// What the handler said about a request: allowed (with any messages to post), or refused.
struct Decision {
    refusal: Option<(String, String)>,
    sends: Vec<Value>,
}

async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event_type: &'static EventType,
    data: Value,
    op: &str,
) -> std::result::Result<Decision, Reply> {
    let event = Event::new(event_type, data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::MatrixProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, op, "fail_closed_llm_error");
            return Err(failure(Some(&e)));
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, op, "fail_closed_invalid_reply");
        return Err(failure(None));
    }
    let mut accepted = false;
    let mut refusal = None;
    let mut sends = Vec::new();
    let mut stack = result.protocol_results;
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { name, data } => match name.as_str() {
                actions::ACCEPT => accepted = true,
                actions::SEND => sends.push(data),
                actions::REJECT => {
                    refusal = Some((
                        data["errcode"]
                            .as_str()
                            .unwrap_or("M_FORBIDDEN")
                            .to_string(),
                        data["error"]
                            .as_str()
                            .unwrap_or("Refused by the server")
                            .to_string(),
                    ))
                }
                _ => {}
            },
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    sends.reverse();
    match (refusal, accepted || !sends.is_empty()) {
        (Some(_), true) => {
            outcome(ctx, id, op, "fail_closed_invalid_reply");
            Err(failure(None))
        }
        (Some(r), false) => {
            outcome(ctx, id, op, "model_reject");
            Ok(Decision {
                refusal: Some(r),
                sends,
            })
        }
        (None, true) => {
            outcome(ctx, id, op, "model_answer");
            Ok(Decision {
                refusal: None,
                sends,
            })
        }
        (None, false) => {
            outcome(ctx, id, op, "model_silent");
            Err(failure(None))
        }
    }
}

/// Post the handler's messages as its own user, joining it to each room first.
fn apply_sends(ctx: &SpawnContext, shared: &Shared, default_room: &str, sends: &[Value]) {
    if sends.is_empty() {
        return;
    }
    {
        let mut hub = shared.hub.lock().unwrap_or_else(|e| e.into_inner());
        for s in sends {
            let room = s["room_id"].as_str().unwrap_or(default_room).to_string();
            if !hub.rooms.contains_key(&room) {
                Log::new(Some(&ctx.status_tx)).warn(format!(
                    "Matrix: matrix_send to unknown room {room}; dropped"
                ));
                continue;
            }
            if !hub.is_member(&room, &shared.bot) {
                hub.join(&room, &shared.bot);
            }
            let kind = s["event_type"].as_str().unwrap_or("m.room.message");
            hub.post(&room, &shared.bot, kind, actions::send_content(s), None);
        }
    }
    shared.wake.notify_waiters();
}

fn query(q: Option<&str>) -> HashMap<String, String> {
    q.unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let d = |s: &str| {
                urlencoding::decode(&s.replace('+', " "))
                    .map(|c| c.into_owned())
                    .unwrap_or_default()
            };
            (d(k), d(v))
        })
        .collect()
}

fn segment(s: &str) -> String {
    urlencoding::decode(s)
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

async fn handle(
    request: Request<Incoming>,
    id: ConnectionId,
    ctx: &SpawnContext,
    shared: &Shared,
) -> Reply {
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(body, actions::MAX_BODY_BYTES).collect(),
    )
    .await
    {
        Err(_) => return error(408, "M_UNKNOWN", "request body deadline exceeded"),
        Ok(Err(e)) => {
            return if e
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                error(413, "M_TOO_LARGE", "Request body too large")
            } else {
                error(400, "M_UNKNOWN", "incomplete HTTP body")
            }
        }
        Ok(Ok(b)) => b.to_bytes(),
    };
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(bytes.len() as u64),
            None,
            Some(1),
            None,
        )
        .await;
    let q = query(parts.uri.query());
    let path = parts.uri.path().to_string();
    let method = parts.method.clone();
    if method == Method::OPTIONS {
        let mut r = reply(200, &json!({}));
        let h = r.headers_mut();
        h.insert(
            "Access-Control-Allow-Methods",
            HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
        );
        h.insert(
            "Access-Control-Allow-Headers",
            HeaderValue::from_static("Authorization, Content-Type"),
        );
        return r;
    }
    if path == "/_matrix/client/versions" {
        return reply(
            200,
            &json!({"versions": ["r0.6.1", "v1.1", "v1.2", "v1.3", "v1.4", "v1.5", "v1.6", "v1.7", "v1.8", "v1.9", "v1.10", "v1.11"],
                    "unstable_features": {}}),
        );
    }
    let Some(rest) = path
        .strip_prefix("/_matrix/client/v3/")
        .or_else(|| path.strip_prefix("/_matrix/client/r0/"))
    else {
        return error(404, "M_UNRECOGNIZED", "Unrecognized request");
    };
    let body: Value = if bytes.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(v @ Value::Object(_)) => v,
            _ => return error(400, "M_NOT_JSON", "Content not JSON."),
        }
    };
    match (&method, rest) {
        (&Method::GET, "login") => {
            return reply(200, &json!({"flows": [{"type": "m.login.password"}]}))
        }
        (&Method::POST, "login") => return login(ctx, id, shared, &body).await,
        _ => {}
    }

    // Everything else needs an access token.
    let token = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| q.get("access_token").cloned());
    let Some(token) = token else {
        return error(401, "M_MISSING_TOKEN", "Missing access token");
    };
    let Some((user, device)) = shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .whoami(&token)
    else {
        return error(401, "M_UNKNOWN_TOKEN", "Unrecognised access token");
    };
    let segs: Vec<&str> = rest.split('/').collect();
    match (&method, segs.as_slice()) {
        (&Method::POST, ["logout"]) => {
            shared
                .hub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .logout(&token);
            reply(200, &json!({}))
        }
        (&Method::GET, ["account", "whoami"]) => {
            reply(200, &json!({"user_id": user, "device_id": device}))
        }
        (&Method::GET, ["sync"]) => sync(shared, &user, &q).await,
        (&Method::GET, ["joined_rooms"]) => {
            let rooms = shared
                .hub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .joined(&user);
            reply(200, &json!({"joined_rooms": rooms}))
        }
        (&Method::POST, ["user", _, "filter"]) => reply(200, &json!({"filter_id": "0"})),
        (&Method::GET, ["user", _, "filter", _]) => reply(200, &json!({})),
        (&Method::POST, ["createRoom"]) => create_room(ctx, id, shared, &user, &body).await,
        (&Method::POST, ["join", room]) | (&Method::POST, ["rooms", room, "join"]) => {
            join(ctx, id, shared, &user, &segment(room)).await
        }
        (&Method::POST, ["rooms", room, "leave"]) => {
            let room = segment(room);
            if !shared
                .hub
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .leave(&room, &user)
            {
                return error(403, "M_FORBIDDEN", "You are not in this room");
            }
            shared.wake.notify_waiters();
            reply(200, &json!({}))
        }
        (&Method::POST, ["rooms", room, "invite"]) => {
            let room = segment(room);
            let Some(invitee) = body["user_id"].as_str() else {
                return error(400, "M_MISSING_PARAM", "user_id is required");
            };
            let mut hub = shared.hub.lock().unwrap_or_else(|e| e.into_inner());
            if !hub.is_member(&room, &user) {
                return error(403, "M_FORBIDDEN", "You are not in this room");
            }
            hub.invite(&room, &user, invitee);
            drop(hub);
            shared.wake.notify_waiters();
            reply(200, &json!({}))
        }
        (&Method::PUT, ["rooms", room, "send", kind, txn]) => {
            send(
                ctx,
                id,
                shared,
                &user,
                &segment(room),
                &segment(kind),
                &segment(txn),
                body,
            )
            .await
        }
        (&Method::GET, ["rooms", room, "messages"]) => {
            let room = segment(room);
            let hub = shared.hub.lock().unwrap_or_else(|e| e.into_inner());
            if !hub.is_member(&room, &user) {
                return error(403, "M_FORBIDDEN", "You are not in this room");
            }
            let limit = q
                .get("limit")
                .and_then(|l| l.parse::<usize>().ok())
                .unwrap_or(10)
                .min(MAX_PAGE);
            let history: Vec<Value> = hub.rooms[&room].history.iter().cloned().collect();
            let chunk: Vec<Value> = if q.get("dir").map(String::as_str) == Some("f") {
                history.into_iter().take(limit).collect()
            } else {
                history.into_iter().rev().take(limit).collect()
            };
            reply(200, &json!({"chunk": chunk, "start": "t0", "end": "t0"}))
        }
        (&Method::GET, ["rooms", room, "joined_members"]) => {
            let room = segment(room);
            let hub = shared.hub.lock().unwrap_or_else(|e| e.into_inner());
            if !hub.is_member(&room, &user) {
                return error(403, "M_FORBIDDEN", "You are not in this room");
            }
            let joined: serde_json::Map<String, Value> = hub
                .members(&room)
                .into_iter()
                .map(|m| (m, json!({})))
                .collect();
            reply(200, &json!({"joined": joined}))
        }
        _ => error(404, "M_UNRECOGNIZED", "Unrecognized request"),
    }
}

async fn login(ctx: &SpawnContext, id: ConnectionId, shared: &Shared, body: &Value) -> Reply {
    if body["type"] != "m.login.password" {
        return error(400, "M_UNKNOWN", "Only m.login.password is supported");
    }
    let user = body["identifier"]["user"]
        .as_str()
        .or_else(|| body["user"].as_str());
    let (Some(user), Some(password)) = (user, body["password"].as_str()) else {
        return error(
            400,
            "M_MISSING_PARAM",
            "identifier.user and password are required",
        );
    };
    let user_id = shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .user_id(user);
    let device = body["device_id"]
        .as_str()
        .filter(|d| !d.is_empty() && d.len() <= 64)
        .map(str::to_string)
        .unwrap_or_else(|| hub::random_id(10).to_uppercase());
    match &shared.passwords {
        Some(table) => {
            let local = user_id
                .trim_start_matches('@')
                .split(':')
                .next()
                .unwrap_or_default();
            if table.get(local).map(String::as_str) != Some(password) {
                outcome(ctx, id, "login", "password_mismatch");
                return error(403, "M_FORBIDDEN", "Invalid username or password");
            }
            outcome(ctx, id, "login", "password_match");
        }
        None => {
            let decision = match ask(
                ctx,
                id,
                &actions::LOGIN_EVENT,
                json!({"user_id": user_id, "device_id": device}),
                "login",
            )
            .await
            {
                Ok(d) => d,
                Err(r) => return r,
            };
            if let Some((code, msg)) = decision.refusal {
                return error(403, &code, &msg);
            }
        }
    }
    let token = shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issue_token(&user_id, &device);
    let Some(token) = token else {
        return error(429, "M_LIMIT_EXCEEDED", "Too many sessions");
    };
    let server_name = shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .server_name
        .clone();
    reply(
        200,
        &json!({"user_id": user_id, "access_token": token, "device_id": device, "home_server": server_name}),
    )
}

async fn create_room(
    ctx: &SpawnContext,
    id: ConnectionId,
    shared: &Shared,
    user: &str,
    body: &Value,
) -> Reply {
    let name = body["name"].as_str().map(str::to_string);
    let invite: Vec<String> = body["invite"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|u| u.starts_with('@') && u.contains(':'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let decision = match ask(
        ctx,
        id,
        &actions::CREATE_ROOM_EVENT,
        json!({"user_id": user, "name": name, "invite": invite}),
        "create_room",
    )
    .await
    {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let Some((code, msg)) = decision.refusal {
        return error(403, &code, &msg);
    }
    let room = shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .create_room(user, name, &invite);
    let Some(room) = room else {
        return error(429, "M_LIMIT_EXCEEDED", "Too many rooms");
    };
    shared.wake.notify_waiters();
    apply_sends(ctx, shared, &room, &decision.sends);
    reply(200, &json!({"room_id": room}))
}

async fn join(
    ctx: &SpawnContext,
    id: ConnectionId,
    shared: &Shared,
    user: &str,
    room: &str,
) -> Reply {
    let (exists, member, invited, name) = {
        let hub = shared.hub.lock().unwrap_or_else(|e| e.into_inner());
        match hub.rooms.get(room) {
            Some(r) => (
                true,
                r.members.contains(user),
                r.invited.contains(user),
                r.name.clone(),
            ),
            None => (false, false, false, None),
        }
    };
    if !exists {
        return error(404, "M_NOT_FOUND", "No such room");
    }
    if member {
        return reply(200, &json!({"room_id": room}));
    }
    let decision = match ask(
        ctx,
        id,
        &actions::JOIN_EVENT,
        json!({"user_id": user, "room_id": room, "room_name": name, "invited": invited}),
        "join",
    )
    .await
    {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let Some((code, msg)) = decision.refusal {
        return error(403, &code, &msg);
    }
    shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .join(room, user);
    shared.wake.notify_waiters();
    apply_sends(ctx, shared, room, &decision.sends);
    reply(200, &json!({"room_id": room}))
}

#[allow(clippy::too_many_arguments)]
async fn send(
    ctx: &SpawnContext,
    id: ConnectionId,
    shared: &Shared,
    user: &str,
    room: &str,
    kind: &str,
    txn: &str,
    content: Value,
) -> Reply {
    let key = (user.to_string(), txn.to_string());
    if let Some(event_id) = shared
        .txns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .get(&key)
    {
        return reply(200, &json!({"event_id": event_id}));
    }
    let (member, name, members) = {
        let hub = shared.hub.lock().unwrap_or_else(|e| e.into_inner());
        (
            hub.is_member(room, user),
            hub.room_name(room),
            hub.members(room),
        )
    };
    if !member {
        return error(403, "M_FORBIDDEN", "You are not in this room");
    }
    let decision = match ask(
        ctx,
        id,
        &actions::MESSAGE_EVENT,
        json!({"room_id": room, "room_name": name, "sender": user, "event_type": kind,
               "content": content, "members": members}),
        "send",
    )
    .await
    {
        Ok(d) => d,
        Err(r) => return r,
    };
    if let Some((code, msg)) = decision.refusal {
        return error(403, &code, &msg);
    }
    let event_id = shared
        .hub
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .post(room, user, kind, content, None);
    let Some(event_id) = event_id else {
        return error(404, "M_NOT_FOUND", "No such room");
    };
    {
        let mut t = shared.txns.lock().unwrap_or_else(|e| e.into_inner());
        t.0.insert(key.clone(), event_id.clone());
        t.1.push_back(key);
        while t.1.len() > MAX_TXNS {
            if let Some(old) = t.1.pop_front() {
                t.0.remove(&old);
            }
        }
    }
    shared.wake.notify_waiters();
    apply_sends(ctx, shared, room, &decision.sends);
    reply(200, &json!({"event_id": event_id}))
}

async fn sync(shared: &Shared, user: &str, q: &HashMap<String, String>) -> Reply {
    let since = hub::parse_batch(q.get("since").map(String::as_str));
    let wait = q
        .get("timeout")
        .and_then(|t| t.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_default()
        .min(MAX_SYNC_WAIT);
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let notified = shared.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let body = shared
            .hub
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sync(user, since);
        if let Some(body) = body {
            return reply(200, &body);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return reply(
                200,
                &json!({"next_batch": format!("s{}", since.unwrap_or(0)),
                        "rooms": {"join": {}, "invite": {}, "leave": {}},
                        "presence": {"events": []}, "account_data": {"events": []},
                        "to_device": {"events": []}, "device_lists": {"changed": [], "left": []},
                        "device_one_time_keys_count": {}}),
            );
        }
    }
}
