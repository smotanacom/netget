//! LwM2M 1.1 server over CoAP/UDP. Rust owns the registration interface (register, update,
//! deregister, lifetime expiry) and turns the handler's operations into CoAP requests to the
//! device; responses and notifications come back as events.
pub mod actions;
pub mod content;
pub mod exchange;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::coap::codec::{self, CoapMessage};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::ClientSendOutcome;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{bail, Context, Result};
use exchange::{path_options, uint_option, Exchange, OPT_OBSERVE};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Operations whose results raise further events chain at most this deep.
pub const MAX_CHAIN: usize = 4;
pub const MAX_REGISTRATIONS: usize = 1024;
/// A registration expires this long after its lifetime runs out without an update.
pub const EXPIRY_GRACE: Duration = Duration::from_secs(15);
const DEFAULT_LIFETIME: u64 = 86400;

struct Reg {
    endpoint: String,
    peer: SocketAddr,
    conn: ConnectionId,
    lifetime: u64,
    expires: tokio::time::Instant,
}

struct Shared {
    ctx: SpawnContext,
    ex: Arc<Exchange>,
    regs: Mutex<HashMap<String, Reg>>,
    observed: Mutex<HashMap<(String, String), (SocketAddr, Vec<u8>)>>,
    next_id: AtomicU64,
}

fn outcome(ctx: &SpawnContext, operation: &str, decision: &str) {
    let summary = format!("LwM2M operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn ask(
    shared: &Shared,
    conn: Option<ConnectionId>,
    event: Event,
    operation: &str,
) -> Result<Vec<Json>> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        conn,
        &event,
        &actions::Lwm2mProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, operation, "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, operation, "fail_closed_invalid_reply");
        bail!("the handler's answer was not valid");
    }
    let mut out = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => out.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    out.reverse();
    Ok(out)
}

fn query_params(m: &CoapMessage) -> HashMap<String, String> {
    m.option_values(codec::OPT_URI_QUERY)
        .iter()
        .filter_map(|q| {
            let s = String::from_utf8_lossy(q);
            let (k, v) = s.split_once('=').unwrap_or((&s, ""));
            Some((k.to_owned(), v.to_owned()))
        })
        .collect()
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let socket = Arc::new(UdpSocket::bind(ctx.legacy_listen_addr()).await?);
    let local = socket.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("LwM2M server on udp {local}"));
    let ex = Exchange::new(socket);
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        ex: ex.clone(),
        regs: Mutex::new(HashMap::new()),
        observed: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });
    let (tx, mut rx) = mpsc::channel(256);
    ctx.state.spawn_server_task(ctx.server_id, ex.run(tx)).await;
    let s = shared.clone();
    ctx.state
        .spawn_server_task(ctx.server_id, async move {
            while let Some((peer, m)) = rx.recv().await {
                let s2 = s.clone();
                let state = s.ctx.state.clone();
                state
                    .spawn_server_task(s.ctx.server_id, async move { request(&s2, peer, m).await })
                    .await;
            }
        })
        .await;
    let s = shared.clone();
    ctx.state
        .spawn_server_task(ctx.server_id, async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let now = tokio::time::Instant::now();
                let expired: Vec<String> = s
                    .regs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .filter(|(_, r)| now > r.expires)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in expired {
                    remove(&s, &id, "expired").await;
                }
            }
        })
        .await;
    Ok(local)
}

async fn remove(shared: &Shared, id: &str, reason: &str) {
    let Some(reg) = shared
        .regs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id)
    else {
        return;
    };
    shared
        .observed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(ep, _), _| *ep != reg.endpoint);
    let ctx = &shared.ctx;
    ctx.state
        .remove_peer_handle(ctx.server_id, reg.conn.as_u32())
        .await;
    ctx.state
        .update_connection_status(ctx.server_id, reg.conn, ConnectionStatus::Closed)
        .await;
    let _ = ask(
        shared,
        Some(reg.conn),
        Event::new(
            &actions::DEREGISTER_EVENT,
            json!({"endpoint": reg.endpoint, "reason": reason}),
        ),
        "deregister",
    )
    .await;
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}

async fn request(shared: &Arc<Shared>, peer: SocketAddr, m: CoapMessage) {
    let segs = m.path_segments();
    let ex = &shared.ex;
    let result = match (
        m.code,
        segs.iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice(),
    ) {
        (codec::CODE_POST, ["rd"]) => register(shared, peer, &m).await,
        (codec::CODE_POST, ["rd", id]) => update(shared, peer, &m, id).await,
        (codec::CODE_DELETE, ["rd", id]) => {
            let known = shared
                .regs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(*id);
            if known {
                let _ = ex
                    .respond(peer, &m, codec::code(2, 2), vec![], vec![])
                    .await;
                remove(shared, id, "deregistered").await;
            } else {
                let _ = ex
                    .respond(peer, &m, codec::CODE_NOT_FOUND, vec![], vec![])
                    .await;
            }
            Ok(())
        }
        _ => {
            ex.respond(peer, &m, codec::CODE_NOT_FOUND, vec![], vec![])
                .await
        }
    };
    if let Err(e) = result {
        Log::new(Some(&shared.ctx.status_tx)).debug(format!("LwM2M request from {peer}: {e:#}"));
    }
}

async fn register(shared: &Arc<Shared>, peer: SocketAddr, m: &CoapMessage) -> Result<()> {
    let ex = &shared.ex;
    let q = query_params(m);
    let endpoint = q.get("ep").cloned().unwrap_or_default();
    let lifetime: u64 = q
        .get("lt")
        .and_then(|l| l.parse().ok())
        .unwrap_or(DEFAULT_LIFETIME);
    let objects = content::links_decode(&m.payload).unwrap_or_default();
    if endpoint.is_empty() || endpoint.len() > 256 || lifetime == 0 || objects.is_empty() {
        outcome(&shared.ctx, "register", "protocol_refusal");
        return ex
            .respond(peer, m, codec::CODE_BAD_REQUEST, vec![], vec![])
            .await;
    }
    let full = shared.regs.lock().unwrap_or_else(|e| e.into_inner()).len() >= MAX_REGISTRATIONS;
    if full {
        outcome(&shared.ctx, "register", "protocol_refusal");
        return ex
            .respond(peer, m, codec::CODE_SERVICE_UNAVAILABLE, vec![], vec![])
            .await;
    }
    let objects: Vec<Json> = objects.into_iter().filter(|o| o["path"] != "/").collect();
    let data = json!({
        "endpoint": endpoint,
        "address": peer.to_string(),
        "lifetime": lifetime,
        "version": q.get("lwm2m").cloned().unwrap_or_else(|| "1.0".into()),
        "binding": q.get("b").cloned().unwrap_or_else(|| "U".into()),
        "objects": objects,
    });
    let answers = match ask(
        shared,
        None,
        Event::new(&actions::REGISTER_EVENT, data),
        "register",
    )
    .await
    {
        Ok(a) => a,
        Err(_) => {
            return ex
                .respond(peer, m, codec::CODE_SERVICE_UNAVAILABLE, vec![], vec![])
                .await
        }
    };
    match answers
        .iter()
        .find(|a| matches!(a["type"].as_str(), Some("lwm2m_accept" | "lwm2m_reject")))
    {
        Some(a) if a["type"] == "lwm2m_accept" => {}
        Some(a) => {
            outcome(&shared.ctx, "register", "model_reject");
            let code = if a["reason"] == "bad_request" {
                codec::CODE_BAD_REQUEST
            } else {
                codec::code(4, 3)
            };
            return ex.respond(peer, m, code, vec![], vec![]).await;
        }
        None => {
            outcome(&shared.ctx, "register", "model_silent");
            return ex
                .respond(peer, m, codec::CODE_SERVICE_UNAVAILABLE, vec![], vec![])
                .await;
        }
    }
    outcome(&shared.ctx, "register", "model_answer");
    // A device registering again replaces its earlier registration.
    let previous: Vec<String> = shared
        .regs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(_, r)| r.endpoint == endpoint)
        .map(|(id, _)| id.clone())
        .collect();
    for id in previous {
        let old = shared
            .regs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if let Some(old) = old {
            shared
                .ctx
                .state
                .update_connection_status(shared.ctx.server_id, old.conn, ConnectionStatus::Closed)
                .await;
        }
    }
    let id = format!("{}", shared.next_id.fetch_add(1, Ordering::Relaxed));
    let conn = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
    let now = Instant::now();
    let local = shared.ex.socket().local_addr()?;
    shared
        .ctx
        .state
        .add_connection_to_server(
            shared.ctx.server_id,
            ConnectionState {
                id: conn,
                remote_addr: peer,
                local_addr: local,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: now,
                status: ConnectionStatus::Active,
                status_changed_at: now,
                protocol_info: ProtocolConnectionInfo::new(
                    json!({"endpoint": endpoint, "registration": id}),
                ),
            },
        )
        .await;
    shared
        .regs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id.clone(),
            Reg {
                endpoint: endpoint.clone(),
                peer,
                conn,
                lifetime,
                expires: tokio::time::Instant::now() + Duration::from_secs(lifetime) + EXPIRY_GRACE,
            },
        );
    let mut cmds = crate::server::peer_support::register_peer_channel(
        &shared.ctx.state,
        shared.ctx.server_id,
        conn.as_u32(),
    )
    .await;
    let s = shared.clone();
    let ep = endpoint.clone();
    shared
        .ctx
        .state
        .spawn_server_task(shared.ctx.server_id, async move {
            while let Some(c) = cmds.recv().await {
                let outcome = match actions::validate_operation(&c.action) {
                    Ok(()) => {
                        let (s2, ep2, a) = (s.clone(), ep.clone(), c.action.clone());
                        let state = s.ctx.state.clone();
                        state
                            .spawn_server_task(s.ctx.server_id, async move {
                                run_ops(&s2, &ep2, vec![a], 0).await
                            })
                            .await;
                        ClientSendOutcome::Sent { bytes_sent: 0 }
                    }
                    Err(e) => ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                };
                crate::client::command_support::reply(c, Ok(outcome));
            }
        })
        .await;
    ex.respond(
        peer,
        m,
        codec::code(2, 1),
        vec![
            (codec::OPT_LOCATION_PATH, b"rd".to_vec()),
            (codec::OPT_LOCATION_PATH, id.into_bytes()),
        ],
        vec![],
    )
    .await?;
    run_ops(shared, &endpoint, answers, 0).await;
    Ok(())
}

async fn update(shared: &Arc<Shared>, peer: SocketAddr, m: &CoapMessage, id: &str) -> Result<()> {
    let q = query_params(m);
    let found = {
        let mut regs = shared.regs.lock().unwrap_or_else(|e| e.into_inner());
        regs.get_mut(id).map(|r| {
            if let Some(lt) = q.get("lt").and_then(|l| l.parse().ok()) {
                r.lifetime = lt;
            }
            r.peer = peer;
            r.expires =
                tokio::time::Instant::now() + Duration::from_secs(r.lifetime) + EXPIRY_GRACE;
            (r.endpoint.clone(), r.conn, r.lifetime)
        })
    };
    let Some((endpoint, conn, lifetime)) = found else {
        return shared
            .ex
            .respond(peer, m, codec::CODE_NOT_FOUND, vec![], vec![])
            .await;
    };
    shared
        .ex
        .respond(peer, m, codec::code(2, 4), vec![], vec![])
        .await?;
    let mut data = json!({"endpoint": endpoint, "lifetime": lifetime});
    if !m.payload.is_empty() {
        data["objects"] = json!(content::links_decode(&m.payload).unwrap_or_default());
    }
    if let Ok(ops) = ask(
        shared,
        Some(conn),
        Event::new(&actions::UPDATE_EVENT, data),
        "update",
    )
    .await
    {
        run_ops(shared, &endpoint, ops, 0).await;
    }
    Ok(())
}

/// Run operations on a device; each answer becomes lwm2m_response and may chain further.
fn run_ops<'a>(
    shared: &'a Arc<Shared>,
    endpoint: &'a str,
    ops: Vec<Json>,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        for op in ops {
            if !op["type"].as_str().is_some_and(|t| {
                t.starts_with("lwm2m_") && !matches!(t, "lwm2m_accept" | "lwm2m_reject")
            }) {
                continue;
            }
            let result = operate(shared, endpoint, &op).await;
            if depth >= MAX_CHAIN {
                continue;
            }
            let conn = shared
                .regs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .find(|r| r.endpoint == endpoint)
                .map(|r| r.conn);
            if let Ok(next) = ask(
                shared,
                conn,
                Event::new(&actions::RESPONSE_EVENT, result),
                "response",
            )
            .await
            {
                run_ops(shared, endpoint, next, depth + 1).await;
            }
        }
    })
}

async fn operate(shared: &Arc<Shared>, endpoint: &str, op: &Json) -> Json {
    let operation = op["type"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches("lwm2m_")
        .to_owned();
    let path = op["path"].as_str().unwrap_or_default().to_owned();
    let mut result =
        json!({"endpoint": endpoint, "operation": operation, "path": path, "code": null});
    match operate_inner(shared, endpoint, op, &operation, &path).await {
        Ok((code, extra)) => {
            result["code"] = json!(codec::code_to_string(code));
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    result[k] = v.clone();
                }
            }
        }
        Err(e) => result["error"] = json!(format!("{e:#}")),
    }
    result
}

async fn operate_inner(
    shared: &Arc<Shared>,
    endpoint: &str,
    op: &Json,
    operation: &str,
    path: &str,
) -> Result<(u8, Json)> {
    actions::validate_operation(op)?;
    let peer = shared
        .regs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .find(|r| r.endpoint == endpoint)
        .map(|r| r.peer)
        .context("the device is not registered")?;
    let mut options = path_options(path);
    let mut payload = vec![];
    let mut observe = None;
    let code = match operation {
        "read" => {
            let text = op["format"] == "text";
            options.push(uint_option(
                codec::OPT_ACCEPT,
                if text {
                    content::CF_TEXT
                } else {
                    content::CF_SENML_JSON
                } as u32,
            ));
            codec::CODE_GET
        }
        "discover" => {
            options.push(uint_option(codec::OPT_ACCEPT, content::CF_LINK as u32));
            codec::CODE_GET
        }
        "observe" => {
            options.push(uint_option(OPT_OBSERVE, 0));
            options.push(uint_option(
                codec::OPT_ACCEPT,
                content::CF_SENML_JSON as u32,
            ));
            let (tx, rx) = mpsc::channel(64);
            observe = Some(tx);
            spawn_notifications(shared, endpoint, path, rx).await;
            codec::CODE_GET
        }
        "cancel_observe" => {
            let token = shared
                .observed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(endpoint.to_owned(), path.to_owned()));
            return match token {
                Some((p, t)) => {
                    shared.ex.forget(p, &t);
                    Ok((codec::code(2, 2), json!({})))
                }
                None => bail!("{path} is not observed"),
            };
        }
        "write" => {
            if let Some(values) = op.get("values").filter(|v| !v.is_null()) {
                options.push(uint_option(
                    codec::OPT_CONTENT_FORMAT,
                    content::CF_SENML_JSON as u32,
                ));
                payload = content::senml_encode(
                    values.as_array().map(Vec::as_slice).unwrap_or_default(),
                )?;
                if op["mode"] == "update" {
                    codec::CODE_POST
                } else {
                    codec::CODE_PUT
                }
            } else {
                options.push(uint_option(
                    codec::OPT_CONTENT_FORMAT,
                    content::CF_TEXT as u32,
                ));
                payload = content::text_encode(op)?;
                codec::CODE_PUT
            }
        }
        "execute" => {
            payload = op
                .get("arguments")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            if !payload.is_empty() {
                options.push(uint_option(
                    codec::OPT_CONTENT_FORMAT,
                    content::CF_TEXT as u32,
                ));
            }
            codec::CODE_POST
        }
        "create" => {
            options.push(uint_option(
                codec::OPT_CONTENT_FORMAT,
                content::CF_SENML_JSON as u32,
            ));
            payload = content::senml_encode(
                op["values"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
            codec::CODE_POST
        }
        "delete" => codec::CODE_DELETE,
        other => bail!("{other} is not an operation"),
    };
    let response = shared
        .ex
        .request(peer, code, options, payload, observe)
        .await?;
    let mut extra = json!({});
    if codec::code_class(response.code) == 2 && !response.payload.is_empty() {
        let format = response.option_uint(codec::OPT_CONTENT_FORMAT);
        if format == Some(content::CF_LINK as u32) {
            extra["links"] = json!(content::links_decode(&response.payload)?);
        } else {
            extra["values"] = json!(content::decode(format, path, &response.payload)?);
        }
    }
    if operation == "observe" && codec::code_class(response.code) == 2 {
        shared
            .observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (endpoint.to_owned(), path.to_owned()),
                (peer, response.token.clone()),
            );
    }
    Ok((response.code, extra))
}

async fn spawn_notifications(
    shared: &Arc<Shared>,
    endpoint: &str,
    path: &str,
    mut rx: mpsc::Receiver<CoapMessage>,
) {
    let (s, ep, p) = (shared.clone(), endpoint.to_owned(), path.to_owned());
    let state = shared.ctx.state.clone();
    state
        .spawn_server_task(shared.ctx.server_id, async move {
            while let Some(n) = rx.recv().await {
                let values =
                    content::decode(n.option_uint(codec::OPT_CONTENT_FORMAT), &p, &n.payload)
                        .unwrap_or_default();
                let sequence = n.option_uint(OPT_OBSERVE).unwrap_or(0);
                let conn = s
                    .regs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .values()
                    .find(|r| r.endpoint == ep)
                    .map(|r| r.conn);
                if let Ok(ops) = ask(
                    &s,
                    conn,
                    Event::new(
                        &actions::NOTIFICATION_EVENT,
                        json!({"endpoint": ep, "path": p, "values": values, "sequence": sequence}),
                    ),
                    "notification",
                )
                .await
                {
                    run_ops(&s, &ep, ops, 1).await;
                }
            }
        })
        .await;
}
