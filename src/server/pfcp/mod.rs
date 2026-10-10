//! PFCP user plane function (UDP 8805). Rust owns the framing, the association and session
//! tables, heartbeats, the refusals that need no judgement (no association, unknown session,
//! missing F-SEID) and retransmissions; the model decides associations and sessions.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::connection::ConnectionId;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;

/// Responses kept for retransmitted requests.
pub const RESPONSE_CACHE: usize = 1024;
/// Sessions one UPF holds at once; establishment past it is refused (no_resources_available).
pub const MAX_SESSIONS: usize = 10_000;

#[derive(Clone)]
struct SessionRow {
    peer: IpAddr,
    cp_seid: u64,
}

enum Cached {
    InFlight,
    Done(Vec<u8>),
}

#[derive(Default)]
struct Tables {
    associations: HashMap<IpAddr, Value>,
    sessions: HashMap<u64, SessionRow>,
    next_seid: u64,
    cache: HashMap<(SocketAddr, u32), Cached>,
    order: VecDeque<(SocketAddr, u32)>,
}

#[derive(Clone)]
struct Shared {
    ctx: SpawnContext,
    socket: Arc<UdpSocket>,
    tables: Arc<Mutex<Tables>>,
    node_id: Value,
    ip: IpAddr,
    recovery: u64,
}

fn lock(t: &Mutex<Tables>) -> std::sync::MutexGuard<'_, Tables> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let port = ctx.port.unwrap_or_else(|| ctx.legacy_listen_addr().port());
    let host = ctx.host.clone().unwrap_or_else(|| "127.0.0.1".into());
    let bind = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve host {host}"))?
        .next()
        .with_context(|| format!("host {host} resolved to no address"))?;
    let socket = Arc::new(
        UdpSocket::bind(bind)
            .await
            .with_context(|| format!("PFCP failed to bind {bind}"))?,
    );
    let local = socket.local_addr()?;
    let ip = if local.ip().is_unspecified() {
        IpAddr::from([127, 0, 0, 1])
    } else {
        local.ip()
    };
    let node_id = match ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string("node_id"))
        .transpose()?
        .flatten()
    {
        Some(n) if n.parse::<std::net::Ipv4Addr>().is_ok() => json!({"ipv4": n}),
        Some(n) => json!({"fqdn": n}),
        None => match ip {
            IpAddr::V4(a) => json!({"ipv4": a.to_string()}),
            IpAddr::V6(a) => json!({"ipv6": a.to_string()}),
        },
    };
    wire::encode(6, None, 0, &json!({"node_id": node_id})).context("node_id")?;
    Log::new(Some(&ctx.status_tx))
        .info(format!("PFCP UPF listening on {local}, node id {node_id}"));
    let shared = Shared {
        ctx: ctx.clone(),
        socket: socket.clone(),
        tables: Arc::new(Mutex::new(Tables {
            next_seid: rand::random::<u32>() as u64 + 1,
            ..Default::default()
        })),
        node_id,
        ip,
        recovery: wire::now_unix(),
    };
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        loop {
            let (n, from) = match socket.recv_from(&mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    Log::new(Some(&shared.ctx.status_tx)).error(format!("PFCP receive error: {e}"));
                    break;
                }
            };
            let bytes = buf[..n].to_vec();
            if let Some(handle) = receive(&shared, bytes, from).await {
                shared
                    .ctx
                    .state
                    .register_server_task(shared.ctx.server_id, handle)
                    .await;
            }
        }
    });
    ctx.state.register_server_task(ctx.server_id, task).await;
    Ok(local)
}

/// Decode and route one datagram; a request that needs work gets its own task.
async fn receive(
    shared: &Shared,
    bytes: Vec<u8>,
    from: SocketAddr,
) -> Option<tokio::task::JoinHandle<()>> {
    if bytes.len() >= 2 && bytes[0] >> 5 != 1 {
        // TS 29.244 §7.4.4.7: Version Not Supported Response, header only.
        let seq = if bytes.len() >= 8 {
            u32::from_be_bytes([0, bytes[4], bytes[5], bytes[6]])
        } else {
            0
        };
        if let Ok(b) = wire::encode(11, None, seq, &Value::Null) {
            let _ = shared.socket.send_to(&b, from).await;
        }
        return None;
    }
    let msg = match wire::parse(&bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("PFCP dropped a datagram from {from}: {e:#}");
            return None;
        }
    };
    let t = msg.header.message_type;
    if !wire::is_request(t) {
        tracing::debug!("PFCP ignored a {} from {from}", wire::message_name(t));
        return None;
    }
    let key = (from, msg.header.sequence);
    let cached: Option<Option<Vec<u8>>> = {
        let mut tables = lock(&shared.tables);
        match tables.cache.get(&key) {
            Some(Cached::Done(response)) => Some(Some(response.clone())),
            Some(Cached::InFlight) => Some(None), // still being answered
            None => {
                tables.cache.insert(key, Cached::InFlight);
                tables.order.push_back(key);
                while tables.order.len() > RESPONSE_CACHE {
                    if let Some(old) = tables.order.pop_front() {
                        tables.cache.remove(&old);
                    }
                }
                None
            }
        }
    };
    match cached {
        Some(Some(response)) => {
            tracing::info!(
                "PFCP {} from {from} seq {} is a retransmission: answered from cache",
                wire::message_name(t),
                key.1
            );
            let _ = shared.socket.send_to(&response, from).await;
            return None;
        }
        Some(None) => return None,
        None => {}
    }
    let shared = shared.clone();
    Some(tokio::spawn(async move {
        let response = answer(&shared, &msg, from).await;
        let send = {
            let mut tables = lock(&shared.tables);
            match response {
                Some(bytes) => {
                    tables.cache.insert(key, Cached::Done(bytes.clone()));
                    Some(bytes)
                }
                None => {
                    tables.cache.remove(&key);
                    None
                }
            }
        };
        if let Some(bytes) = send {
            if let Err(e) = shared.socket.send_to(&bytes, from).await {
                tracing::warn!("PFCP response to {from} failed: {e}");
            }
        }
    }))
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// Build the response to `msg`, asking the model where judgement is needed.
async fn answer(s: &Shared, msg: &wire::Message, from: SocketAddr) -> Option<Vec<u8>> {
    let t = msg.header.message_type;
    let seq = msg.header.sequence;
    let rt = t + 1;
    let peer = from.ip();
    let log = Log::new(Some(&s.ctx.status_tx));
    let name = wire::message_name(t);
    let respond = |seid: Option<u64>, ies: Map<String, Value>| {
        wire::encode(rt, seid, seq, &Value::Object(ies)).ok()
    };
    let cause_only = |cause: &str| {
        let mut m = Map::new();
        m.insert("cause".into(), json!(cause));
        m
    };
    let associated = lock(&s.tables).associations.contains_key(&peer);
    match t {
        1 => {
            // Heartbeat: Rust answers, with this node's recovery time.
            return respond(None, obj(json!({"recovery_time_stamp": s.recovery})));
        }
        5 => {
            let node = msg.ies.get("node_id").cloned();
            let Some(node) = node else {
                log.info(format!(
                    "PFCP {name} from {from} decision=refused_missing_node_id"
                ));
                let mut ies = cause_only("mandatory_ie_missing");
                ies.insert("node_id".into(), s.node_id.clone());
                ies.insert("offending_ie".into(), json!(60));
                return respond(None, ies);
            };
            let data = json!({"peer": from.to_string(), "message": name, "sequence": seq, "ies": msg.ies, "node_id": node});
            let (cause, extra) = decide(s, &actions::ASSOCIATION_EVENT, data, &name, from).await;
            if cause == "request_accepted" {
                lock(&s.tables).associations.insert(peer, node);
            }
            let mut ies = extra;
            ies.insert("node_id".into(), s.node_id.clone());
            ies.insert("cause".into(), json!(cause));
            ies.insert("recovery_time_stamp".into(), json!(s.recovery));
            return respond(None, ies);
        }
        _ => {}
    }
    if !associated {
        log.info(format!(
            "PFCP {name} from {from} decision=refused_no_association"
        ));
        let seid = wire::has_seid(rt).then_some(0);
        let mut ies = cause_only("no_established_pfcp_association");
        if matches!(rt, 51 | 6 | 8 | 10) {
            ies.insert("node_id".into(), s.node_id.clone());
        }
        return respond(seid, ies);
    }
    match t {
        50 => {
            let Some(cp_seid) = msg
                .ies
                .get("f_seid")
                .and_then(|f| f.get("seid"))
                .and_then(Value::as_u64)
            else {
                log.info(format!(
                    "PFCP {name} from {from} decision=refused_missing_f_seid"
                ));
                let mut ies = cause_only("mandatory_ie_missing");
                ies.insert("node_id".into(), s.node_id.clone());
                ies.insert("offending_ie".into(), json!(57));
                return respond(Some(0), ies);
            };
            let up_seid = {
                let mut tables = lock(&s.tables);
                if tables.sessions.len() >= MAX_SESSIONS {
                    drop(tables);
                    log.warn(format!(
                        "PFCP {name} from {from} decision=refused_max_sessions"
                    ));
                    let mut ies = cause_only("no_resources_available");
                    ies.insert("node_id".into(), s.node_id.clone());
                    return respond(Some(cp_seid), ies);
                }
                let id = tables.next_seid;
                tables.next_seid += 1;
                id
            };
            let data = json!({"peer": from.to_string(), "message": name, "sequence": seq, "ies": msg.ies, "cp_seid": cp_seid, "up_seid": up_seid});
            let (cause, extra) = decide(s, &actions::ESTABLISHMENT_EVENT, data, &name, from).await;
            let mut ies = extra;
            ies.insert("node_id".into(), s.node_id.clone());
            ies.insert("cause".into(), json!(cause));
            if cause == "request_accepted" {
                let mut fseid = json!({"seid": up_seid});
                match s.ip {
                    IpAddr::V4(a) => fseid["ipv4"] = json!(a.to_string()),
                    IpAddr::V6(a) => fseid["ipv6"] = json!(a.to_string()),
                }
                ies.insert("f_seid".into(), fseid);
                lock(&s.tables)
                    .sessions
                    .insert(up_seid, SessionRow { peer, cp_seid });
            }
            respond(Some(cp_seid), ies)
        }
        52 | 54 | 56 => {
            let up_seid = msg.header.seid.unwrap_or(0);
            let row = lock(&s.tables)
                .sessions
                .get(&up_seid)
                .filter(|r| r.peer == peer)
                .cloned();
            let Some(row) = row else {
                log.info(format!(
                    "PFCP {name} from {from} for SEID {up_seid} decision=refused_unknown_session"
                ));
                return respond(Some(0), cause_only("session_context_not_found"));
            };
            let data = json!({"peer": from.to_string(), "message": name, "sequence": seq, "ies": msg.ies, "cp_seid": row.cp_seid, "up_seid": up_seid});
            let (cause, extra) = decide(s, &actions::SESSION_EVENT, data, &name, from).await;
            if t == 54 && cause == "request_accepted" {
                lock(&s.tables).sessions.remove(&up_seid);
            }
            let mut ies = extra;
            ies.insert("cause".into(), json!(cause));
            respond(Some(row.cp_seid), ies)
        }
        _ => {
            let data =
                json!({"peer": from.to_string(), "message": name, "sequence": seq, "ies": msg.ies});
            let (cause, extra) = decide(s, &actions::REQUEST_EVENT, data, &name, from).await;
            if t == 9 && cause == "request_accepted" {
                let mut tables = lock(&s.tables);
                tables.associations.remove(&peer);
                tables.sessions.retain(|_, r| r.peer != peer);
            }
            let mut ies = extra;
            ies.insert("cause".into(), json!(cause));
            if matches!(rt, 8 | 10) {
                ies.insert("node_id".into(), s.node_id.clone());
            }
            let seid = wire::has_seid(rt).then_some(msg.header.seid.unwrap_or(0));
            respond(seid, ies)
        }
    }
}

/// Ask the model; returns the cause and the extra IEs. Silence and failure are rejections.
async fn decide(
    s: &Shared,
    t: &'static EventType,
    data: Value,
    name: &str,
    from: SocketAddr,
) -> (String, Map<String, Value>) {
    let ctx = &s.ctx;
    let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
    record(ctx, id, from).await;
    let log = Log::new(Some(&ctx.status_tx));
    let summary = format!("PFCP {name} from {from}");
    let event = Event::new(t, data);
    match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::PfcpProtocol,
    )
    .await
    {
        Ok(execution) => {
            let answer = execution.protocol_results.iter().find_map(|r| match r {
                ActionResult::Custom { data, .. } => Some(data.clone()),
                _ => None,
            });
            match answer {
                Some(a) => {
                    let cause = a["cause"]
                        .as_str()
                        .unwrap_or("request_accepted")
                        .to_string();
                    log.info(format!("{summary} decision=model_answered cause={cause}"));
                    // Mandatory IEs are Rust's; the model's copies of them are dropped.
                    let mut ies = obj(a.get("ies").cloned().unwrap_or(Value::Null));
                    for k in ["cause", "node_id", "f_seid", "recovery_time_stamp"] {
                        ies.remove(k);
                    }
                    (cause, ies)
                }
                None => {
                    log.info(format!(
                        "{summary} decision=model_silent (answered request_rejected)"
                    ));
                    ("request_rejected".into(), Map::new())
                }
            }
        }
        Err(e) => {
            let cause = if crate::utils::wire_failure::WireFailure::classify(&e).is_overloaded() {
                "pfcp_entity_in_congestion"
            } else {
                "system_failure"
            };
            log.error(format!(
                "{summary} decision=fail_closed_llm_error cause={cause}: {e}"
            ));
            (cause.into(), Map::new())
        }
    }
}

async fn record(ctx: &SpawnContext, id: ConnectionId, from: SocketAddr) {
    use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
    let now = crate::utils::clock::Instant::now();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id,
                remote_addr: from,
                local_addr: from,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 1,
                last_activity: now,
                status: ConnectionStatus::Active,
                status_changed_at: now,
                protocol_info: ProtocolConnectionInfo::empty(),
            },
        )
        .await;
}
