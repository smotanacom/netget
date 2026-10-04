//! WAMP v2 router, basic profile, JSON over WebSocket. Rust owns the transport, message
//! validation, realms, sessions, subscriptions, registrations and every routed call and event;
//! the handler admits sessions and answers calls to procedures no session has registered.
pub mod actions;
pub mod uri;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{bail, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, handshake::server as ws, Message as WsMessage};
use uri::*;

pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_MESSAGE: usize = 1024 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOX: usize = 1024;
const MAX_SUBSCRIPTIONS: usize = 1024;
const MAX_REGISTRATIONS: usize = 1024;

struct Subscription {
    id: u64,
    policy: String,
    topic: String,
    subscribers: HashSet<u64>,
}

struct Pending {
    caller: u64,
    request: u64,
    callee: u64,
}

#[derive(Default)]
struct Realm {
    sessions: HashMap<u64, mpsc::Sender<Value>>,
    subscriptions: Vec<Subscription>,
    /// procedure → (registration ID, callee session)
    registrations: HashMap<String, (u64, u64)>,
    /// invocation request ID → the call it serves
    invocations: HashMap<u64, Pending>,
}

impl Realm {
    fn send(&self, session: u64, msg: Value) {
        if let Some(tx) = self.sessions.get(&session) {
            // A session whose outbox is full is too slow to keep; its loop closes it.
            let _ = tx.try_send(msg);
        }
    }

    /// Route a publication; the number of sessions it reached.
    fn publish(
        &self,
        publisher: Option<u64>,
        opts: &Map<String, Value>,
        topic: &str,
        args: Option<&Value>,
        kwargs: Option<&Value>,
    ) -> (u64, usize) {
        let publication = random_id();
        let exclude_me = opts
            .get("exclude_me")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let list = |k: &str| -> Option<HashSet<u64>> {
            opts.get(k)
                .and_then(Value::as_array)
                .map(|l| l.iter().filter_map(id_of).collect())
        };
        let (exclude, eligible) = (list("exclude").unwrap_or_default(), list("eligible"));
        let mut reached = 0;
        for sub in self
            .subscriptions
            .iter()
            .filter(|s| matches(&s.policy, &s.topic, topic))
        {
            let mut details = json!({});
            if sub.policy != "exact" {
                details["topic"] = json!(topic);
            }
            if let (Some(p), Some(true)) =
                (publisher, opts.get("disclose_me").and_then(Value::as_bool))
            {
                details["publisher"] = json!(p);
            }
            for s in &sub.subscribers {
                if (exclude_me && Some(*s) == publisher)
                    || exclude.contains(s)
                    || eligible.as_ref().is_some_and(|e| !e.contains(s))
                {
                    continue;
                }
                self.send(
                    *s,
                    with_payload(
                        vec![
                            json!(EVENT),
                            json!(sub.id),
                            json!(publication),
                            details.clone(),
                        ],
                        args,
                        kwargs,
                    ),
                );
                reached += 1;
            }
        }
        (publication, reached)
    }

    /// Remove a session and everything it held; callers waiting on it get ERROR canceled.
    fn leave(&mut self, session: u64) {
        self.sessions.remove(&session);
        for s in &mut self.subscriptions {
            s.subscribers.remove(&session);
        }
        self.subscriptions.retain(|s| !s.subscribers.is_empty());
        self.registrations
            .retain(|_, (_, callee)| *callee != session);
        let lost: Vec<u64> = self
            .invocations
            .iter()
            .filter(|(_, p)| p.callee == session || p.caller == session)
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(p) = self.invocations.remove(&id) {
                if p.caller != session {
                    self.send(
                        p.caller,
                        json!([
                            ERROR,
                            CALL,
                            p.request,
                            {},
                            "wamp.error.canceled",
                            ["the callee left"]
                        ]),
                    );
                }
            }
        }
    }
}

struct Shared {
    ctx: SpawnContext,
    realms: Mutex<HashMap<String, Realm>>,
    hello_timeout: Duration,
    max_message: usize,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let hello_timeout = p
        .map(|p| p.get_optional_u64("hello_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(HELLO_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=120).contains(&hello_timeout),
        "hello_timeout_secs must be 1..=120"
    );
    let max_message = p
        .map(|p| p.get_optional_u64("max_message_bytes"))
        .transpose()?
        .flatten()
        .unwrap_or(MAX_MESSAGE as u64);
    anyhow::ensure!(
        (1024..=16 * 1024 * 1024).contains(&max_message),
        "max_message_bytes must be 1 KiB to 16 MiB"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("WAMP router at ws://{local}/ ({SUBPROTOCOL})"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        realms: Mutex::default(),
        hello_timeout: Duration::from_secs(hello_timeout),
        max_message: max_message as usize,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "WAMP", Some(&shared.ctx.status_tx)).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
            shared
                .ctx
                .state
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
            let child = shared.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = connection(&child, id, stream).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("WAMP connection {id}: {e:#}"));
                    }
                    child
                        .ctx
                        .state
                        .remove_peer_handle(server_id, id.as_u32())
                        .await;
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("WAMP connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn ask(shared: &Shared, id: ConnectionId, event: Event, operation: &str) -> Result<Value> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::WampProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, operation, "fail_closed_invalid_reply");
        bail!("the handler's answer was invalid");
    }
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match answers.len() {
        1 => Ok(answers.remove(0)),
        0 => {
            outcome(ctx, id, operation, "model_silent");
            bail!("the handler gave no answer")
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            bail!("the handler gave several answers")
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn send(ws: &mut Ws, shared: &Shared, id: ConnectionId, msg: &Value) -> Result<()> {
    let text = msg.to_string();
    let n = text.len() as u64;
    ws.send(WsMessage::Text(text)).await?;
    shared
        .ctx
        .state
        .update_connection_stats(shared.ctx.server_id, id, None, Some(n), None, Some(1))
        .await;
    Ok(())
}

/// The next WAMP message: Ok(None) on close; errors are protocol violations.
async fn recv(
    ws: &mut Ws,
    shared: &Shared,
    id: ConnectionId,
) -> Result<Option<Vec<Value>>, String> {
    loop {
        let frame = match ws.next().await {
            None | Some(Err(_)) => return Ok(None),
            Some(Ok(f)) => f,
        };
        let text = match frame {
            WsMessage::Text(t) => t,
            WsMessage::Binary(_) => return Err("binary frames are not wamp.2.json".into()),
            WsMessage::Close(_) => return Ok(None),
            _ => continue,
        };
        shared
            .ctx
            .state
            .update_connection_stats(
                shared.ctx.server_id,
                id,
                Some(text.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))?;
        if !crate::utils::json_budget::within_budget(&v, shared.max_message, 200_000, 64) {
            return Err("message exceeds the router's bounds".into());
        }
        let arr = v
            .as_array()
            .filter(|a| !a.is_empty() && a[0].is_u64())
            .ok_or("a WAMP message is an array starting with its type")?;
        return Ok(Some(arr.clone()));
    }
}

async fn abort(ws: &mut Ws, shared: &Shared, id: ConnectionId, reason: &str, message: &str) {
    let _ = send(
        ws,
        shared,
        id,
        &json!([ABORT, {"message": message}, reason]),
    )
    .await;
    let _ = ws.close(None).await;
}

async fn connection(
    shared: &Shared,
    id: ConnectionId,
    stream: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let callback =
        |req: &ws::Request, mut resp: ws::Response| -> Result<ws::Response, ws::ErrorResponse> {
            let offered = req
                .headers()
                .get_all("sec-websocket-protocol")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .any(|p| p.trim() == SUBPROTOCOL);
            if !offered {
                let mut e = ws::ErrorResponse::new(Some(format!(
                    "this router speaks the {SUBPROTOCOL} subprotocol only"
                )));
                *e.status_mut() = tungstenite::http::StatusCode::BAD_REQUEST;
                return Err(e);
            }
            resp.headers_mut().insert(
                "sec-websocket-protocol",
                tungstenite::http::HeaderValue::from_static(SUBPROTOCOL),
            );
            Ok(resp)
        };
    let config = tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(shared.max_message),
        max_frame_size: Some(shared.max_message),
        ..Default::default()
    };
    let mut ws = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(config)),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => {
            outcome(ctx, id, "handshake", "protocol_refusal");
            bail!("WebSocket handshake: {e}");
        }
        Err(_) => bail!("WebSocket handshake timed out"),
    };
    // HELLO.
    let hello = match tokio::time::timeout(shared.hello_timeout, recv(&mut ws, shared, id)).await {
        Err(_) => {
            outcome(ctx, id, "hello", "protocol_refusal");
            abort(
                &mut ws,
                shared,
                id,
                "wamp.error.protocol_violation",
                "no HELLO in time",
            )
            .await;
            return Ok(());
        }
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => {
            abort(&mut ws, shared, id, "wamp.error.protocol_violation", &e).await;
            return Ok(());
        }
        Ok(Ok(Some(m))) => m,
    };
    let realm = hello
        .get(1)
        .and_then(Value::as_str)
        .filter(|r| uri_ok(r, false))
        .map(str::to_owned);
    let details = hello.get(2).and_then(Value::as_object).cloned();
    let (Some(realm), Some(details), true) =
        (realm, details, hello[0] == HELLO && hello.len() == 3)
    else {
        outcome(ctx, id, "hello", "protocol_refusal");
        abort(
            &mut ws,
            shared,
            id,
            "wamp.error.protocol_violation",
            "the first message must be HELLO [1, Realm|uri, Details|dict]",
        )
        .await;
        return Ok(());
    };
    let roles: Vec<String> = details
        .get("roles")
        .and_then(Value::as_object)
        .map(|r| {
            r.keys()
                .filter(|k| matches!(k.as_str(), "caller" | "callee" | "publisher" | "subscriber"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if roles.is_empty() {
        outcome(ctx, id, "hello", "protocol_refusal");
        abort(
            &mut ws,
            shared,
            id,
            "wamp.error.no_such_role",
            "HELLO announces none of caller, callee, publisher, subscriber",
        )
        .await;
        return Ok(());
    }
    let authid = details
        .get("authid")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let event = Event::new(
        &actions::HELLO_EVENT,
        json!({"realm": realm, "roles": roles, "authid": authid, "authmethods": details.get("authmethods")}),
    );
    let authrole = match ask(shared, id, event, "hello").await {
        Ok(a) if a["type"] == "wamp_welcome" => {
            outcome(ctx, id, "hello", "model_answer");
            a["authrole"].as_str().unwrap_or("anonymous").to_owned()
        }
        Ok(a) if a["type"] == "wamp_abort" => {
            outcome(ctx, id, "hello", "model_reject");
            abort(
                &mut ws,
                shared,
                id,
                a["reason"].as_str().unwrap_or("wamp.error.not_authorized"),
                a["message"].as_str().unwrap_or("refused"),
            )
            .await;
            return Ok(());
        }
        other => {
            if other.is_ok() {
                outcome(ctx, id, "hello", "fail_closed_invalid_reply");
            }
            let message = match &other {
                Err(e) => crate::utils::WireFailure::classify(e).text(),
                Ok(_) => "the router could not decide this session",
            };
            abort(&mut ws, shared, id, "wamp.error.not_authorized", message).await;
            return Ok(());
        }
    };
    let session = random_id();
    let (tx, mut outbox) = mpsc::channel::<Value>(OUTBOX);
    shared
        .realms
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(realm.clone())
        .or_default()
        .sessions
        .insert(session, tx);
    let mut welcome_details = json!({
        "roles": {"broker": {"features": {"publisher_exclusion": true, "subscriber_blackwhite_listing": true, "pattern_based_subscription": true, "publisher_identification": true}},
                  "dealer": {"features": {"caller_identification": true}}},
        "authrole": authrole, "authmethod": "anonymous", "realm": realm,
    });
    if let Some(a) = &authid {
        welcome_details["authid"] = json!(a);
    }
    send(
        &mut ws,
        shared,
        id,
        &json!([WELCOME, session, welcome_details]),
    )
    .await?;
    let mut commands =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    let result = session_loop(
        shared,
        id,
        &mut ws,
        &realm,
        session,
        &mut outbox,
        &mut commands,
    )
    .await;
    if let Some(r) = shared
        .realms
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&realm)
    {
        r.leave(session);
    }
    let _ = ws.close(None).await;
    result
}

enum Wake {
    Message(Result<Option<Vec<Value>>, String>),
    Outbox(Option<Value>),
    Command(Option<ClientCommand>),
}

async fn session_loop(
    shared: &Shared,
    id: ConnectionId,
    ws: &mut Ws,
    realm: &str,
    session: u64,
    outbox: &mut mpsc::Receiver<Value>,
    commands: &mut mpsc::Receiver<ClientCommand>,
) -> Result<()> {
    let ctx = &shared.ctx;
    loop {
        let wake = tokio::select! {
            m = recv(ws, shared, id) => Wake::Message(m),
            o = outbox.recv() => Wake::Outbox(o),
            c = commands.recv() => Wake::Command(c),
        };
        let msg = match wake {
            Wake::Outbox(Some(m)) => {
                send(ws, shared, id, &m).await?;
                continue;
            }
            Wake::Outbox(None) | Wake::Command(None) => continue,
            Wake::Command(Some(cmd)) => {
                let a = cmd.action.clone();
                let outcome = match a["type"].as_str() {
                    Some("wamp_publish") => match actions::check_answer(&a) {
                        Ok(()) => {
                            let realms = shared.realms.lock().unwrap_or_else(|e| e.into_inner());
                            let (_, reached) = realms
                                .get(realm)
                                .map(|r| {
                                    r.publish(
                                        None,
                                        &Map::new(),
                                        a["topic"].as_str().unwrap_or_default(),
                                        a.get("args"),
                                        a.get("kwargs"),
                                    )
                                })
                                .unwrap_or((0, 0));
                            ClientSendOutcome::Sent {
                                bytes_sent: reached,
                            }
                        }
                        Err(e) => ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                    },
                    Some("disconnect") => ClientSendOutcome::Disconnected,
                    _ => ClientSendOutcome::Rejected {
                        error: "a WAMP session accepts wamp_publish or disconnect".into(),
                    },
                };
                ctx.state
                    .record_access_log(
                        crate::state::AccessLogOwner::Server(ctx.server_id.as_u32()),
                        "WAMP",
                        Some(id.as_u32()),
                        "injected_action",
                        json!({"type": a["type"], "topic": a["topic"]}),
                        vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                    )
                    .await;
                let done = matches!(outcome, ClientSendOutcome::Disconnected);
                let _ = cmd.reply_tx.send(Ok(outcome));
                if done {
                    send(ws, shared, id, &json!([GOODBYE, {"message": "the router operator closed this session"}, "wamp.close.system_shutdown"])).await?;
                    return Ok(());
                }
                continue;
            }
            Wake::Message(Ok(None)) => return Ok(()),
            Wake::Message(Err(e)) => {
                abort(ws, shared, id, "wamp.error.protocol_violation", &e).await;
                return Ok(());
            }
            Wake::Message(Ok(Some(m))) => m,
        };
        match handle(shared, id, realm, session, &msg).await {
            Ok(Some(reply)) => {
                let goodbye = reply[0] == GOODBYE;
                send(ws, shared, id, &reply).await?;
                if goodbye {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(violation) => {
                abort(ws, shared, id, "wamp.error.protocol_violation", &violation).await;
                return Ok(());
            }
        }
    }
}

fn request_id(m: &[Value]) -> Result<u64, String> {
    m.get(1)
        .and_then(id_of)
        .ok_or_else(|| "Request|id must be an integer in [1, 2^53]".into())
}

fn options(m: &[Value], i: usize) -> Result<Map<String, Value>, String> {
    m.get(i)
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| "Options|dict missing".into())
}

fn payload(m: &[Value], i: usize) -> Result<(Option<Value>, Option<Value>), String> {
    let args = m.get(i).cloned();
    let kwargs = m.get(i + 1).cloned();
    if args.as_ref().is_some_and(|a| !a.is_array())
        || kwargs.as_ref().is_some_and(|k| !k.is_object())
        || m.len() > i + 2
    {
        return Err("Arguments|list and ArgumentsKw|dict are malformed".into());
    }
    Ok((args, kwargs))
}

fn wamp_error(kind: u64, request: u64, error: &str, message: &str) -> Value {
    json!([ERROR, kind, request, {}, error, [message]])
}

/// What routing one message produced.
enum Routed {
    Reply(Option<Value>),
    /// A call nobody registered, for the handler.
    RouterCall {
        req: u64,
        procedure: String,
        args: Option<Value>,
        kwargs: Option<Value>,
    },
}

/// One client message; the reply to send this session, or a protocol violation.
async fn handle(
    shared: &Shared,
    id: ConnectionId,
    realm: &str,
    session: u64,
    m: &[Value],
) -> Result<Option<Value>, String> {
    let routed = {
        let mut realms = shared.realms.lock().unwrap_or_else(|e| e.into_inner());
        let r = realms.get_mut(realm).ok_or("the realm is gone")?;
        route(r, session, m)?
    };
    match routed {
        Routed::Reply(reply) => Ok(reply),
        Routed::RouterCall {
            req,
            procedure,
            args,
            kwargs,
        } => Ok(Some(
            router_call(shared, id, realm, session, req, &procedure, args, kwargs).await,
        )),
    }
}

fn route(r: &mut Realm, session: u64, m: &[Value]) -> Result<Routed, String> {
    let kind = m[0].as_u64().unwrap_or(0);
    let reply = |v: Value| -> Result<Routed, String> { Ok(Routed::Reply(Some(v))) };
    match kind {
        GOODBYE => reply(json!([GOODBYE, {}, "wamp.close.goodbye_and_out"])),
        SUBSCRIBE => {
            let (req, opts) = (request_id(m)?, options(m, 2)?);
            let topic = m
                .get(3)
                .and_then(Value::as_str)
                .ok_or("Topic|uri missing")?;
            let policy = opts.get("match").and_then(Value::as_str).unwrap_or("exact");
            if !matches!(policy, "exact" | "prefix" | "wildcard") {
                return reply(wamp_error(
                    SUBSCRIBE,
                    req,
                    "wamp.error.option_not_allowed",
                    "match must be exact, prefix or wildcard",
                ));
            }
            if !uri_ok(topic, policy == "wildcard") {
                return reply(wamp_error(
                    SUBSCRIBE,
                    req,
                    "wamp.error.invalid_uri",
                    "not a valid topic URI",
                ));
            }
            if let Some(s) = r
                .subscriptions
                .iter_mut()
                .find(|s| s.topic == topic && s.policy == policy)
            {
                s.subscribers.insert(session);
                return reply(json!([SUBSCRIBED, req, s.id]));
            }
            if r.subscriptions.len() >= MAX_SUBSCRIPTIONS {
                return reply(wamp_error(
                    SUBSCRIBE,
                    req,
                    "wamp.error.option_not_allowed",
                    "the realm holds its maximum number of subscriptions",
                ));
            }
            let sid = random_id();
            r.subscriptions.push(Subscription {
                id: sid,
                policy: policy.into(),
                topic: topic.into(),
                subscribers: HashSet::from([session]),
            });
            reply(json!([SUBSCRIBED, req, sid]))
        }
        UNSUBSCRIBE => {
            let req = request_id(m)?;
            let sub = m
                .get(2)
                .and_then(id_of)
                .ok_or("SUBSCRIBED.Subscription|id missing")?;
            match r
                .subscriptions
                .iter_mut()
                .find(|s| s.id == sub && s.subscribers.contains(&session))
            {
                Some(s) => {
                    s.subscribers.remove(&session);
                    r.subscriptions.retain(|s| !s.subscribers.is_empty());
                    reply(json!([UNSUBSCRIBED, req]))
                }
                None => reply(wamp_error(
                    UNSUBSCRIBE,
                    req,
                    "wamp.error.no_such_subscription",
                    "no such subscription in this session",
                )),
            }
        }
        PUBLISH => {
            let (req, opts) = (request_id(m)?, options(m, 2)?);
            let topic = m
                .get(3)
                .and_then(Value::as_str)
                .ok_or("Topic|uri missing")?;
            let (args, kwargs) = payload(m, 4)?;
            let ack = opts
                .get("acknowledge")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !uri_ok(topic, false) {
                return Ok(Routed::Reply(ack.then(|| {
                    wamp_error(
                        PUBLISH,
                        req,
                        "wamp.error.invalid_uri",
                        "not a valid topic URI",
                    )
                })));
            }
            let (publication, _) =
                r.publish(Some(session), &opts, topic, args.as_ref(), kwargs.as_ref());
            Ok(Routed::Reply(
                ack.then(|| json!([PUBLISHED, req, publication])),
            ))
        }
        REGISTER => {
            let (req, opts) = (request_id(m)?, options(m, 2)?);
            let procedure = m
                .get(3)
                .and_then(Value::as_str)
                .ok_or("Procedure|uri missing")?;
            if opts
                .get("match")
                .and_then(Value::as_str)
                .is_some_and(|p| p != "exact")
                || opts
                    .get("invoke")
                    .and_then(Value::as_str)
                    .is_some_and(|p| p != "single")
            {
                return reply(wamp_error(
                    REGISTER,
                    req,
                    "wamp.error.option_not_allowed",
                    "only exact, single registrations",
                ));
            }
            if !uri_ok(procedure, false) {
                return reply(wamp_error(
                    REGISTER,
                    req,
                    "wamp.error.invalid_uri",
                    "not a valid procedure URI",
                ));
            }
            if r.registrations.contains_key(procedure) {
                return reply(wamp_error(
                    REGISTER,
                    req,
                    "wamp.error.procedure_already_exists",
                    "another session registered this procedure",
                ));
            }
            if r.registrations.len() >= MAX_REGISTRATIONS {
                return reply(wamp_error(
                    REGISTER,
                    req,
                    "wamp.error.option_not_allowed",
                    "the realm holds its maximum number of registrations",
                ));
            }
            let rid = random_id();
            r.registrations.insert(procedure.to_owned(), (rid, session));
            reply(json!([REGISTERED, req, rid]))
        }
        UNREGISTER => {
            let req = request_id(m)?;
            let reg = m
                .get(2)
                .and_then(id_of)
                .ok_or("REGISTERED.Registration|id missing")?;
            let before = r.registrations.len();
            r.registrations
                .retain(|_, (rid, callee)| !(*rid == reg && *callee == session));
            reply(if r.registrations.len() < before {
                json!([UNREGISTERED, req])
            } else {
                wamp_error(
                    UNREGISTER,
                    req,
                    "wamp.error.no_such_registration",
                    "no such registration in this session",
                )
            })
        }
        CALL => {
            let (req, opts) = (request_id(m)?, options(m, 2)?);
            let procedure = m
                .get(3)
                .and_then(Value::as_str)
                .ok_or("Procedure|uri missing")?
                .to_owned();
            let (args, kwargs) = payload(m, 4)?;
            if !uri_ok(&procedure, false) {
                return reply(wamp_error(
                    CALL,
                    req,
                    "wamp.error.invalid_uri",
                    "not a valid procedure URI",
                ));
            }
            if let Some((rid, callee)) = r.registrations.get(&procedure).copied() {
                let inv = random_id();
                r.invocations.insert(
                    inv,
                    Pending {
                        caller: session,
                        request: req,
                        callee,
                    },
                );
                let mut details = json!({});
                if opts.get("disclose_me").and_then(Value::as_bool) == Some(true) {
                    details["caller"] = json!(session);
                }
                r.send(
                    callee,
                    with_payload(
                        vec![json!(INVOCATION), json!(inv), json!(rid), details],
                        args.as_ref(),
                        kwargs.as_ref(),
                    ),
                );
                return Ok(Routed::Reply(None));
            }
            Ok(Routed::RouterCall {
                req,
                procedure,
                args,
                kwargs,
            })
        }
        YIELD => {
            let inv = request_id(m)?;
            let (args, kwargs) = payload(m, 3)?;
            options(m, 2)?;
            match r.invocations.remove(&inv) {
                Some(p) if p.callee == session => {
                    r.send(
                        p.caller,
                        with_payload(
                            vec![json!(RESULT), json!(p.request), json!({})],
                            args.as_ref(),
                            kwargs.as_ref(),
                        ),
                    );
                    Ok(Routed::Reply(None))
                }
                Some(p) => {
                    r.invocations.insert(inv, p);
                    Err("YIELD for an invocation sent to another session".into())
                }
                None => Ok(Routed::Reply(None)),
            }
        }
        ERROR => {
            if m.get(1).and_then(Value::as_u64) != Some(INVOCATION) {
                return Err("a client sends ERROR only for an INVOCATION".into());
            }
            let inv = m
                .get(2)
                .and_then(id_of)
                .ok_or("INVOCATION.Request|id missing")?;
            let error = m
                .get(4)
                .and_then(Value::as_str)
                .filter(|e| uri_ok(e, false))
                .ok_or("Error|uri missing")?
                .to_owned();
            let (args, kwargs) = payload(m, 5)?;
            if let Some(p) = r.invocations.remove(&inv) {
                if p.callee == session {
                    r.send(
                        p.caller,
                        with_payload(
                            vec![
                                json!(ERROR),
                                json!(CALL),
                                json!(p.request),
                                json!({}),
                                json!(error),
                            ],
                            args.as_ref(),
                            kwargs.as_ref(),
                        ),
                    );
                } else {
                    r.invocations.insert(inv, p);
                }
            }
            Ok(Routed::Reply(None))
        }
        CANCEL => Ok(Routed::Reply(None)),
        HELLO => Err("HELLO on an established session".into()),
        other => Err(format!("message type {other} is not one a client sends")),
    }
}

/// A call nobody registered: the handler answers it.
#[allow(clippy::too_many_arguments)]
async fn router_call(
    shared: &Shared,
    id: ConnectionId,
    realm: &str,
    session: u64,
    req: u64,
    procedure: &str,
    args: Option<Value>,
    kwargs: Option<Value>,
) -> Value {
    let event = Event::new(
        &actions::CALL_EVENT,
        json!({"realm": realm, "procedure": procedure, "args": args.clone().unwrap_or_else(|| json!([])), "kwargs": kwargs.clone().unwrap_or_else(|| json!({})), "caller_session": session}),
    );
    match ask(shared, id, event, "call").await {
        Ok(a) if a["type"] == "wamp_result" => {
            outcome(&shared.ctx, id, "call", "model_answer");
            with_payload(
                vec![json!(RESULT), json!(req), json!({})],
                a.get("args"),
                a.get("kwargs"),
            )
        }
        Ok(a) if a["type"] == "wamp_error" => {
            outcome(&shared.ctx, id, "call", "model_reject");
            with_payload(
                vec![
                    json!(ERROR),
                    json!(CALL),
                    json!(req),
                    json!({}),
                    a["error"].clone(),
                ],
                a.get("args"),
                a.get("kwargs"),
            )
        }
        Ok(_) => {
            outcome(&shared.ctx, id, "call", "fail_closed_invalid_reply");
            wamp_error(
                CALL,
                req,
                "wamp.error.unavailable",
                "the router could not answer this call",
            )
        }
        Err(e) => wamp_error(
            CALL,
            req,
            "wamp.error.unavailable",
            crate::utils::WireFailure::classify(&e).text(),
        ),
    }
}
