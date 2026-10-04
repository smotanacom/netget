//! Socket.IO v5 over Engine.IO v4. Rust owns sessions, transports (long-polling, WebSocket,
//! upgrade), heartbeats, packet framing, namespaces, acknowledgement ids and rooms; the handler
//! decides connections and what to emit.
pub mod actions;
pub mod packet;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use packet::{Eio, Kind, Sio};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_tungstenite::tungstenite::{
    handshake::derive_accept_key,
    protocol::{Role, WebSocketConfig},
    Message,
};

pub const DEFAULT_PATH: &str = "/socket.io/";
pub const DEFAULT_NAMESPACES: &[&str] = &["/"];
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(25);
pub const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_SESSIONS: usize = 256;
const QUEUE: usize = 256;
const MAX_ROOMS: usize = 64;
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);

struct Session {
    sid: String,
    conn: ConnectionId,
    tx: mpsc::Sender<String>,
    rx: Mutex<mpsc::Receiver<String>>,
    websocket: AtomicBool,
    /// namespace → socket id
    sockets: Mutex<HashMap<String, String>>,
    last_pong: std::sync::Mutex<Instant>,
    closed: AtomicBool,
    /// Set when the outbound queue overflowed; the heartbeat task then closes the session.
    overflow: AtomicBool,
    closing: Notify,
    acks: Mutex<HashMap<u64, (String, String)>>,
    next_ack: AtomicU64,
    inbound: Mutex<()>,
}

struct Shared {
    ctx: SpawnContext,
    path: String,
    namespaces: Vec<String>,
    ping_interval: Duration,
    ping_timeout: Duration,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// socket id → (session id, namespace, rooms)
    sockets: Mutex<HashMap<String, (String, String, HashSet<String>)>>,
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let path = p
        .map(|p| p.get_optional_string("path"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_PATH.to_owned());
    anyhow::ensure!(
        path.starts_with('/')
            && path.len() <= 128
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)),
        "path must be an absolute path"
    );
    let path = format!("{}/", path.trim_end_matches('/'));
    let namespaces: Vec<String> = match p
        .map(|p| p.get_optional_array("namespaces"))
        .transpose()?
        .flatten()
    {
        None => DEFAULT_NAMESPACES.iter().map(|s| s.to_string()).collect(),
        Some(list) => list
            .iter()
            .map(|n| {
                n.as_str()
                    .filter(|n| packet::namespace_ok(n))
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("namespaces are paths like /admin"))
            })
            .collect::<anyhow::Result<_>>()?,
    };
    anyhow::ensure!(
        !namespaces.is_empty() && namespaces.len() <= 32,
        "1 to 32 namespaces"
    );
    let ms = |k: &str, d: Duration| -> anyhow::Result<Duration> {
        let v = p
            .map(|p| p.get_optional_u64(k))
            .transpose()?
            .flatten()
            .unwrap_or(d.as_millis() as u64);
        anyhow::ensure!((1000..=120_000).contains(&v), "{k} must be 1000..=120000");
        Ok(Duration::from_millis(v))
    };
    let ping_interval = ms("ping_interval_ms", DEFAULT_PING_INTERVAL)?;
    let ping_timeout = ms("ping_timeout_ms", DEFAULT_PING_TIMEOUT)?;
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Socket.IO server at http://{local}{path} (namespaces {})",
        namespaces.join(", ")
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        path,
        namespaces,
        ping_interval,
        ping_timeout,
        sessions: Mutex::default(),
        sockets: Mutex::default(),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "Socket.IO", Some(&shared.ctx.status_tx)).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let child = shared.clone();
            shared.ctx.state.spawn_server_task(server_id, async move {
                let _permit = permit;
                let svc = child.clone();
                let upgrade: UpgradeSlot = Arc::default();
                let svc_upgrade = upgrade.clone();
                let service = service_fn(move |req| {
                    let shared = svc.clone();
                    let upgrade = svc_upgrade.clone();
                    async move { Ok::<_, Infallible>(route(&shared, peer, req, &upgrade).await) }
                });
                let mut builder = http1::Builder::new();
                builder.timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_headers(64).max_buf_size(64 * 1024);
                let _ = builder.serve_connection(TokioIo::new(stream), service).with_upgrades().await;
                let pending = upgrade.lock().await.take();
                if let Some((on_upgrade, session, fresh)) = pending {
                    if let Ok(upgraded) = on_upgrade.await {
                        let mut cfg = WebSocketConfig::default();
                        cfg.max_message_size = Some(packet::MAX_PAYLOAD);
                        cfg.max_frame_size = Some(packet::MAX_PAYLOAD);
                        let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(cfg)).await;
                        websocket(&child, session, ws, fresh).await;
                    }
                }
            }).await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

type UpgradeSlot = Arc<Mutex<Option<(hyper::upgrade::OnUpgrade, Arc<Session>, bool)>>>;
type Reply = Response<Full<Bytes>>;

fn text(status: u16, body: String) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=UTF-8"),
    );
    r
}

/// Engine.IO's error body: `{"code": N, "message": ...}` with 400.
fn eio_error(code: u8, message: &str) -> Reply {
    let mut r = text(400, json!({"code": code, "message": message}).to_string());
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    r
}

fn outcome(ctx: &SpawnContext, conn: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("Socket.IO session {conn} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

fn query(req: &Request<Incoming>) -> HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

fn open_packet(shared: &Shared, sid: &str, upgrades: bool) -> String {
    let data = json!({"sid": sid, "upgrades": if upgrades { json!(["websocket"]) } else { json!([]) }, "pingInterval": shared.ping_interval.as_millis() as u64, "pingTimeout": shared.ping_timeout.as_millis() as u64, "maxPayload": packet::MAX_PAYLOAD});
    Eio::Open(data.to_string()).encode()
}

async fn new_session(
    shared: &Arc<Shared>,
    peer: SocketAddr,
    websocket: bool,
) -> Option<Arc<Session>> {
    let ctx = &shared.ctx;
    let mut sessions = shared.sessions.lock().await;
    if sessions.len() >= MAX_SESSIONS {
        return None;
    }
    let (tx, rx) = mpsc::channel(QUEUE);
    let conn = ConnectionId::new(ctx.state.get_next_unified_id().await);
    let now = Instant::now();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id: conn,
                remote_addr: peer,
                local_addr: peer,
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
    let session = Arc::new(Session {
        sid: packet::new_id(),
        conn,
        tx,
        rx: Mutex::new(rx),
        websocket: AtomicBool::new(websocket),
        sockets: Mutex::default(),
        last_pong: std::sync::Mutex::new(now),
        closed: AtomicBool::new(false),
        overflow: AtomicBool::new(false),
        closing: Notify::new(),
        acks: Mutex::default(),
        next_ack: AtomicU64::new(1),
        inbound: Mutex::new(()),
    });
    sessions.insert(session.sid.clone(), session.clone());
    drop(sessions);
    let peer_rx = crate::server::peer_support::register_peer_channel(
        &ctx.state,
        ctx.server_id,
        conn.as_u32(),
    )
    .await;
    ctx.state
        .spawn_server_task(
            ctx.server_id,
            peer_commands(shared.clone(), session.clone(), peer_rx),
        )
        .await;
    ctx.state
        .spawn_server_task(ctx.server_id, heartbeat(shared.clone(), session.clone()))
        .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Socket.IO session {conn} opened over {}",
        if websocket { "websocket" } else { "polling" }
    ));
    Some(session)
}

async fn heartbeat(shared: Arc<Shared>, session: Arc<Session>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(shared.ping_interval) => {}
            _ = session.closing.notified() => {
                if session.overflow.load(Ordering::SeqCst) {
                    close_session(&shared, &session, "transport error").await;
                }
                return;
            }
        }
        if session.closed.load(Ordering::SeqCst) {
            return;
        }
        let since_pong = session
            .last_pong
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default();
        if since_pong > shared.ping_interval + shared.ping_timeout {
            close_session(&shared, &session, "ping timeout").await;
            return;
        }
        if session
            .tx
            .try_send(Eio::Ping(String::new()).encode())
            .is_err()
        {
            close_session(&shared, &session, "transport error").await;
            return;
        }
        // The pong must arrive within pingTimeout.
        tokio::select! {
            _ = tokio::time::sleep(shared.ping_timeout) => {}
            _ = session.closing.notified() => {
                if session.overflow.load(Ordering::SeqCst) {
                    close_session(&shared, &session, "transport error").await;
                }
                return;
            }
        }
        let since_pong = session
            .last_pong
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default();
        if since_pong > shared.ping_timeout + Duration::from_millis(50)
            && !session.closed.load(Ordering::SeqCst)
        {
            close_session(&shared, &session, "ping timeout").await;
            return;
        }
    }
}

async fn close_session(shared: &Arc<Shared>, session: &Arc<Session>, reason: &str) {
    if session.closed.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = session.tx.try_send(Eio::Close.encode());
    session.closing.notify_waiters();
    shared.sessions.lock().await.remove(&session.sid);
    let ctx = &shared.ctx;
    ctx.state
        .remove_peer_handle(ctx.server_id, session.conn.as_u32())
        .await;
    ctx.state
        .update_connection_status(ctx.server_id, session.conn, ConnectionStatus::Closed)
        .await;
    let sockets: Vec<(String, String)> = session.sockets.lock().await.drain().collect();
    for (nsp, socket) in sockets {
        shared.sockets.lock().await.remove(&socket);
        notify_disconnect(shared, session, &nsp, &socket, reason).await;
    }
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Socket.IO session {} closed: {reason}",
        session.conn
    ));
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}

async fn route(
    shared: &Arc<Shared>,
    peer: SocketAddr,
    mut req: Request<Incoming>,
    upgrade: &UpgradeSlot,
) -> Reply {
    let path = req.uri().path();
    if path != shared.path && format!("{path}/") != shared.path {
        return text(404, "not found".into());
    }
    let q = query(&req);
    if q.get("EIO").map(String::as_str) != Some(packet::EIO_VERSION) {
        return eio_error(5, "Unsupported protocol version");
    }
    let session = match q.get("sid") {
        None => None,
        Some(sid) => match shared.sessions.lock().await.get(sid).cloned() {
            Some(s) => Some(s),
            None => return eio_error(1, "Session ID unknown"),
        },
    };
    match (q.get("transport").map(String::as_str), req.method()) {
        (Some("websocket"), &Method::GET) => {
            let h = req.headers();
            let key = h
                .get(header::SEC_WEBSOCKET_KEY)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let is_ws = h
                .get(header::UPGRADE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
            let (Some(key), true) = (key, is_ws) else {
                return eio_error(3, "Bad request");
            };
            let (session, fresh) = match session {
                Some(s) => {
                    if s.websocket.load(Ordering::SeqCst) {
                        return eio_error(3, "Bad request");
                    }
                    (s, false)
                }
                None => match new_session(shared, peer, true).await {
                    Some(s) => (s, true),
                    None => return eio_error(3, "Too many sessions"),
                },
            };
            *upgrade.lock().await = Some((hyper::upgrade::on(&mut req), session, fresh));
            let mut r = Response::new(Full::new(Bytes::new()));
            *r.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
            let hs = r.headers_mut();
            hs.insert(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            );
            hs.insert(
                header::CONNECTION,
                header::HeaderValue::from_static("Upgrade"),
            );
            if let Ok(v) = header::HeaderValue::from_str(&derive_accept_key(key.as_bytes())) {
                hs.insert(header::SEC_WEBSOCKET_ACCEPT, v);
            }
            r
        }
        (Some("polling"), &Method::GET) => match session {
            None => match new_session(shared, peer, false).await {
                Some(s) => text(200, open_packet(shared, &s.sid, true)),
                None => eio_error(3, "Too many sessions"),
            },
            Some(s) => poll(shared, &s).await,
        },
        (Some("polling"), &Method::POST) => {
            let Some(s) = session else {
                return eio_error(1, "Session ID unknown");
            };
            let body = match tokio::time::timeout(
                BODY_TIMEOUT,
                Limited::new(req.into_body(), packet::MAX_PAYLOAD).collect(),
            )
            .await
            {
                Ok(Ok(b)) => b.to_bytes(),
                _ => {
                    close_session(shared, &s, "transport error").await;
                    return eio_error(3, "Bad request");
                }
            };
            let packets = std::str::from_utf8(&body).ok().map(packet::split_payload);
            match packets {
                Some(Ok(list)) => {
                    let ctx = &shared.ctx;
                    ctx.state
                        .update_connection_stats(
                            ctx.server_id,
                            s.conn,
                            Some(body.len() as u64),
                            None,
                            Some(list.len() as u64),
                            None,
                        )
                        .await;
                    for p in list {
                        process(shared, &s, p).await;
                    }
                    text(200, "ok".into())
                }
                _ => {
                    close_session(shared, &s, "parse error").await;
                    eio_error(3, "Bad request")
                }
            }
        }
        (Some("polling" | "websocket"), _) => eio_error(2, "Bad handshake method"),
        _ => eio_error(0, "Transport unknown"),
    }
}

/// A long-poll: wait for queued packets (a ping arrives within pingInterval at the latest) and
/// return them record-separated. A second concurrent poll on one session is a protocol error.
async fn poll(shared: &Arc<Shared>, s: &Arc<Session>) -> Reply {
    if s.websocket.load(Ordering::SeqCst) {
        return eio_error(3, "Bad request");
    }
    let Ok(mut rx) = s.rx.try_lock() else {
        close_session(shared, s, "transport error").await;
        return eio_error(3, "Bad request");
    };
    let first = tokio::select! {
        p = rx.recv() => p,
        _ = tokio::time::sleep(shared.ping_interval + shared.ping_timeout) => Some(Eio::Noop.encode()),
    };
    let Some(first) = first else {
        return text(200, Eio::Close.encode());
    };
    let mut packets = vec![first];
    let mut size = packets[0].len();
    while packets.len() < packet::MAX_PACKETS && size < packet::MAX_PAYLOAD / 2 {
        match rx.try_recv() {
            Ok(p) => {
                size += p.len();
                packets.push(p);
            }
            Err(_) => break,
        }
    }
    let body = packet::join_payload(&packets);
    let ctx = &shared.ctx;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            s.conn,
            None,
            Some(body.len() as u64),
            None,
            Some(packets.len() as u64),
        )
        .await;
    text(200, body)
}

async fn websocket(
    shared: &Arc<Shared>,
    session: Arc<Session>,
    ws: tokio_tungstenite::WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>,
    fresh: bool,
) {
    let (mut sink, mut stream) = ws.split();
    if fresh {
        if sink
            .send(Message::Text(open_packet(shared, &session.sid, false)))
            .await
            .is_err()
        {
            close_session(shared, &session, "transport error").await;
            return;
        }
    } else {
        // Upgrade from polling: "2probe" → "3probe", flush the pending poll with a noop, then
        // wait for "5" before switching.
        let probe = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
        if !matches!(probe, Ok(Some(Ok(Message::Text(ref t)))) if t == "2probe") {
            return;
        }
        if sink.send(Message::Text("3probe".into())).await.is_err() {
            return;
        }
        let _ = session.tx.try_send(Eio::Noop.encode());
        let upgrade = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
        if !matches!(upgrade, Ok(Some(Ok(Message::Text(ref t)))) if t == "5") {
            return;
        }
        session.websocket.store(true, Ordering::SeqCst);
    }
    let writer_session = session.clone();
    let writer_shared = shared.clone();
    let writer = tokio::spawn(async move {
        let mut rx = writer_session.rx.lock().await;
        while let Some(p) = rx.recv().await {
            let closing = p == Eio::Close.encode();
            let n = p.len() as u64;
            if sink.send(Message::Text(p)).await.is_err() {
                break;
            }
            let ctx = &writer_shared.ctx;
            ctx.state
                .update_connection_stats(
                    ctx.server_id,
                    writer_session.conn,
                    None,
                    Some(n),
                    None,
                    Some(1),
                )
                .await;
            if closing {
                break;
            }
        }
        let _ = sink.close().await;
    });
    loop {
        let msg = tokio::select! {
            m = stream.next() => m,
            _ = session.closing.notified() => break,
        };
        let text = match msg {
            Some(Ok(Message::Text(t))) => t,
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            Some(Ok(Message::Binary(_))) => {
                Log::new(Some(&shared.ctx.status_tx)).warn(format!(
                    "Socket.IO session {}: binary frames are not supported",
                    session.conn
                ));
                continue;
            }
            _ => break,
        };
        let ctx = &shared.ctx;
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                session.conn,
                Some(text.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        match Eio::decode(&text) {
            Ok(p) => process(shared, &session, p).await,
            Err(_) => break,
        }
        if session.closed.load(Ordering::SeqCst) {
            break;
        }
    }
    close_session(shared, &session, "transport close").await;
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}

async fn send(session: &Session, p: Sio) -> bool {
    session
        .tx
        .try_send(Eio::Message(p.encode()).encode())
        .is_ok()
}

async fn process(shared: &Arc<Shared>, session: &Arc<Session>, p: Eio) {
    let _turn = session.inbound.lock().await;
    match p {
        Eio::Pong(_) => {
            if let Ok(mut t) = session.last_pong.lock() {
                *t = Instant::now();
            }
        }
        Eio::Ping(d) => {
            let _ = session.tx.try_send(Eio::Pong(d).encode());
        }
        Eio::Close => {
            drop(_turn);
            close_session(shared, session, "client disconnect").await;
        }
        Eio::Message(m) => match Sio::decode(&m) {
            Ok(sio) => socket_packet(shared, session, sio).await,
            Err(e) => Log::new(Some(&shared.ctx.status_tx))
                .warn(format!("Socket.IO session {}: {e}", session.conn)),
        },
        Eio::Open(_) | Eio::Upgrade | Eio::Noop => {}
    }
}

/// Ask the handler about `event`; `Err(())` after logging when it cannot answer.
async fn ask(
    shared: &Shared,
    session: &Session,
    event: Event,
    operation: &str,
    allowed: &[&str],
) -> Result<Vec<Value>, ()> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(session.conn),
        &event,
        &actions::SocketIoProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            outcome(ctx, session.conn, operation, "fail_closed_llm_error");
            return Err(());
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, session.conn, operation, "fail_closed_invalid_reply");
        return Err(());
    }
    let mut out = Vec::new();
    let mut pending: Vec<ActionResult> = result.protocol_results.into_iter().rev().collect();
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if allowed.contains(&name.as_str()) => {
                out.push(data)
            }
            ActionResult::Multiple(items) => pending.extend(items.into_iter().rev()),
            _ => {}
        }
    }
    Ok(out)
}

async fn socket_packet(shared: &Arc<Shared>, session: &Arc<Session>, p: Sio) {
    let ctx = &shared.ctx;
    let socket = session.sockets.lock().await.get(&p.nsp).cloned();
    match p.kind {
        Kind::Connect => {
            if socket.is_some() {
                return;
            }
            if !shared.namespaces.contains(&p.nsp) {
                outcome(ctx, session.conn, "connect", "protocol_refusal");
                send(
                    session,
                    Sio::new(
                        Kind::ConnectError,
                        &p.nsp,
                        None,
                        Some(json!({"message": "Invalid namespace"})),
                    ),
                )
                .await;
                return;
            }
            let event = Event::new(
                &actions::CONNECT_EVENT,
                json!({"namespace": p.nsp, "auth": p.data, "session_id": session.sid, "transport": if session.websocket.load(Ordering::SeqCst) { "websocket" } else { "polling" }}),
            );
            let answers = match ask(
                shared,
                session,
                event,
                "connect",
                &[
                    "socketio_accept",
                    "socketio_reject",
                    "socketio_emit",
                    "socketio_join",
                ],
            )
            .await
            {
                Ok(a) => a,
                Err(()) => {
                    send(session, Sio::new(Kind::ConnectError, &p.nsp, None, Some(json!({"message": crate::utils::wire_failure::WireFailure::Unavailable.text()})))).await;
                    return;
                }
            };
            let accept = answers.iter().position(|a| a["type"] == "socketio_accept");
            let reject = answers.iter().find(|a| a["type"] == "socketio_reject");
            match (accept, reject) {
                (Some(_), None) => {
                    let sid = packet::new_id();
                    session
                        .sockets
                        .lock()
                        .await
                        .insert(p.nsp.clone(), sid.clone());
                    shared.sockets.lock().await.insert(
                        sid.clone(),
                        (session.sid.clone(), p.nsp.clone(), HashSet::new()),
                    );
                    send(
                        session,
                        Sio::new(Kind::Connect, &p.nsp, None, Some(json!({"sid": sid}))),
                    )
                    .await;
                    outcome(ctx, session.conn, "connect", "model_answer");
                    let rest: Vec<Value> = answers
                        .into_iter()
                        .filter(|a| a["type"] != "socketio_accept")
                        .collect();
                    apply(shared, session, &p.nsp, &sid, None, rest).await;
                }
                (None, Some(r)) => {
                    outcome(ctx, session.conn, "connect", "model_reject");
                    let mut data = json!({"message": r["message"]});
                    if let Some(d) = r.get("data").filter(|d| d.is_object()) {
                        data["data"] = d.clone();
                    }
                    send(
                        session,
                        Sio::new(Kind::ConnectError, &p.nsp, None, Some(data)),
                    )
                    .await;
                }
                _ => {
                    outcome(
                        ctx,
                        session.conn,
                        "connect",
                        if answers.is_empty() {
                            "model_silent"
                        } else {
                            "fail_closed_invalid_reply"
                        },
                    );
                    send(session, Sio::new(Kind::ConnectError, &p.nsp, None, Some(json!({"message": crate::utils::wire_failure::WireFailure::Unavailable.text()})))).await;
                }
            }
        }
        Kind::Disconnect => {
            if let Some(sid) = socket {
                session.sockets.lock().await.remove(&p.nsp);
                shared.sockets.lock().await.remove(&sid);
                notify_disconnect(shared, session, &p.nsp, &sid, "client namespace disconnect")
                    .await;
            }
        }
        Kind::Event => {
            let Some(sid) = socket else {
                Log::new(Some(&ctx.status_tx)).warn(format!(
                    "Socket.IO session {}: event for unconnected namespace {}",
                    session.conn, p.nsp
                ));
                return;
            };
            let (name, args) = match p.event() {
                Ok(e) => e,
                Err(e) => {
                    Log::new(Some(&ctx.status_tx))
                        .warn(format!("Socket.IO session {}: {e}", session.conn));
                    return;
                }
            };
            let rooms: Vec<String> = shared
                .sockets
                .lock()
                .await
                .get(&sid)
                .map(|s| s.2.iter().cloned().collect())
                .unwrap_or_default();
            let event = Event::new(
                &actions::EVENT_EVENT,
                json!({"socket_id": sid, "namespace": p.nsp, "event": name, "args": args, "ack_requested": p.id.is_some(), "rooms": rooms}),
            );
            if let Ok(answers) = ask(
                shared,
                session,
                event,
                "event",
                &[
                    "socketio_emit",
                    "socketio_ack",
                    "socketio_join",
                    "socketio_leave",
                    "socketio_disconnect_socket",
                ],
            )
            .await
            {
                outcome(ctx, session.conn, "event", "model_answer");
                apply(shared, session, &p.nsp, &sid, p.id, answers).await;
            }
        }
        Kind::Ack => {
            let (Some(sid), Some(id)) = (socket, p.id) else {
                return;
            };
            let Some((nsp, event_name)) = session.acks.lock().await.remove(&id) else {
                return;
            };
            if nsp != p.nsp {
                return;
            }
            let args = p
                .data
                .as_ref()
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let event = Event::new(
                &actions::ACK_EVENT,
                json!({"socket_id": sid, "namespace": nsp, "event": event_name, "args": args}),
            );
            if let Ok(answers) = ask(
                shared,
                session,
                event,
                "ack",
                &[
                    "socketio_emit",
                    "socketio_join",
                    "socketio_leave",
                    "socketio_disconnect_socket",
                ],
            )
            .await
            {
                outcome(ctx, session.conn, "ack", "model_answer");
                apply(shared, session, &nsp, &sid, None, answers).await;
            }
        }
        Kind::ConnectError | Kind::BinaryEvent | Kind::BinaryAck => {}
    }
}

async fn notify_disconnect(
    shared: &Arc<Shared>,
    session: &Arc<Session>,
    nsp: &str,
    socket: &str,
    reason: &str,
) {
    let event = Event::new(
        &actions::DISCONNECT_EVENT,
        json!({"socket_id": socket, "namespace": nsp, "reason": reason}),
    );
    if let Ok(answers) = ask(shared, session, event, "disconnect", &["socketio_emit"]).await {
        apply(shared, session, nsp, socket, None, answers).await;
    }
}

/// Carry out the handler's actions for socket `sid` in `nsp`. `ack` is the id the client
/// waits on, answered at most once.
async fn apply(
    shared: &Arc<Shared>,
    session: &Arc<Session>,
    nsp: &str,
    sid: &str,
    mut ack: Option<u64>,
    answers: Vec<Value>,
) {
    let ctx = &shared.ctx;
    for a in answers {
        match a["type"].as_str().unwrap_or_default() {
            "socketio_emit" => {
                let ns = a["namespace"].as_str().unwrap_or(nsp).to_owned();
                let to = a["to"].as_str().unwrap_or("sender");
                let include_sender = a["include_sender"].as_bool().unwrap_or(true);
                let mut payload = vec![a["event"].clone()];
                payload.extend(a["args"].as_array().cloned().unwrap_or_default());
                let targets: Vec<String> = {
                    let sockets = shared.sockets.lock().await;
                    match to {
                        "sender" => vec![sid.to_owned()],
                        "namespace" => sockets
                            .iter()
                            .filter(|(_, v)| v.1 == ns)
                            .map(|(k, _)| k.clone())
                            .collect(),
                        t if t.starts_with("room:") => {
                            let room = &t[5..];
                            sockets
                                .iter()
                                .filter(|(_, v)| v.1 == ns && v.2.contains(room))
                                .map(|(k, _)| k.clone())
                                .collect()
                        }
                        t => vec![t.trim_start_matches("socket:").to_owned()],
                    }
                };
                let targets: Vec<String> = targets
                    .into_iter()
                    .filter(|t| include_sender || t != sid)
                    .collect();
                let want_ack = a["ack"].as_bool().unwrap_or(false) && targets.len() == 1;
                for t in targets {
                    let Some((tsid, tns)) = shared
                        .sockets
                        .lock()
                        .await
                        .get(&t)
                        .map(|v| (v.0.clone(), v.1.clone()))
                    else {
                        continue;
                    };
                    let Some(ts) = shared.sessions.lock().await.get(&tsid).cloned() else {
                        continue;
                    };
                    let id = if want_ack {
                        let id = ts.next_ack.fetch_add(1, Ordering::Relaxed);
                        ts.acks.lock().await.insert(
                            id,
                            (
                                tns.clone(),
                                a["event"].as_str().unwrap_or_default().to_owned(),
                            ),
                        );
                        Some(id)
                    } else {
                        None
                    };
                    if !send(
                        &ts,
                        Sio::new(Kind::Event, &tns, id, Some(Value::Array(payload.clone()))),
                    )
                    .await
                    {
                        Log::new(Some(&ctx.status_tx)).warn(format!(
                            "Socket.IO session {}: queue full, closing",
                            ts.conn
                        ));
                        ts.overflow.store(true, Ordering::SeqCst);
                        ts.closing.notify_waiters();
                    }
                }
            }
            "socketio_ack" => {
                if let Some(id) = ack.take() {
                    let args = a["args"].as_array().cloned().unwrap_or_default();
                    send(
                        session,
                        Sio::new(Kind::Ack, nsp, Some(id), Some(Value::Array(args))),
                    )
                    .await;
                }
            }
            "socketio_join" | "socketio_leave" => {
                let room = a["room"].as_str().unwrap_or_default().to_owned();
                if let Some(entry) = shared.sockets.lock().await.get_mut(sid) {
                    if a["type"] == "socketio_join" {
                        if entry.2.len() < MAX_ROOMS {
                            entry.2.insert(room);
                        }
                    } else {
                        entry.2.remove(&room);
                    }
                }
            }
            "socketio_disconnect_socket" => {
                if session.sockets.lock().await.remove(nsp).is_some() {
                    shared.sockets.lock().await.remove(sid);
                    send(session, Sio::new(Kind::Disconnect, nsp, None, None)).await;
                }
            }
            _ => {}
        }
    }
}

async fn peer_commands(
    shared: Arc<Shared>,
    session: Arc<Session>,
    mut rx: mpsc::Receiver<ClientCommand>,
) {
    while let Some(command) = rx.recv().await {
        let a = command.action.clone();
        let result = actions::check_action(&a).and_then(|()| match a["type"].as_str() {
            Some("socketio_emit" | "socketio_disconnect_socket" | "disconnect") => Ok(()),
            _ => anyhow::bail!("a Socket.IO session accepts socketio_emit, socketio_disconnect_socket or disconnect"),
        });
        let outcome = match result {
            Err(e) => ClientSendOutcome::Rejected {
                error: e.to_string(),
            },
            Ok(()) if a["type"] == "disconnect" => {
                close_session(&shared, &session, "server disconnect").await;
                ClientSendOutcome::Disconnected
            }
            Ok(()) => {
                let nsp = a["namespace"].as_str().unwrap_or("/").to_owned();
                match session.sockets.lock().await.get(&nsp).cloned() {
                    None => ClientSendOutcome::Rejected {
                        error: format!("this session has no socket in {nsp}"),
                    },
                    Some(sid) => {
                        apply(&shared, &session, &nsp, &sid, None, vec![a.clone()]).await;
                        ClientSendOutcome::Sent {
                            bytes_sent: a.to_string().len(),
                        }
                    }
                }
            }
        };
        shared
            .ctx
            .state
            .record_access_log(
                crate::state::AccessLogOwner::Server(shared.ctx.server_id.as_u32()),
                "Socket.IO",
                Some(session.conn.as_u32()),
                "injected_action",
                json!({"type": a["type"], "event": a["event"]}),
                vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
            )
            .await;
        let _ = command.reply_tx.send(Ok(outcome));
        if session.closed.load(Ordering::SeqCst) {
            return;
        }
    }
}
