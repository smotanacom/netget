//! Consul agent HTTP API. Rust owns HTTP, Consul's JSON shapes, base64 values, the index
//! header and the agent's own description; the handler answers every KV, catalog and
//! registration request. NetGet stores nothing.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming,
    header::{HeaderValue, CONTENT_TYPE},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_HEADERS: usize = 64;
const CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\nRetry-After: 5\r\n\r\n";

type Reply = Response<Full<Bytes>>;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Consul agent API listening on {local}"));
    let index = Arc::new(AtomicU64::new(1));
    let server_id = ctx.server_id;
    let state = ctx.state.clone();
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                CAP_REFUSAL,
                "Consul",
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
            let index = index.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let request_ctx = child.clone();
                    let service = service_fn(move |request| {
                        let ctx = request_ctx.clone();
                        let index = index.clone();
                        async move { Ok::<_, Infallible>(handle(request, id, &ctx, &index).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .keep_alive(false)
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(MAX_HEADERS)
                        .max_buf_size(MAX_HEADER_BYTES);
                    if let Err(e) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("Consul connection {id} HTTP error: {e}"));
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

fn reply(status: u16, content_type: &str, body: Vec<u8>, index: Option<u64>) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(v) = HeaderValue::from_str(content_type) {
        r.headers_mut().insert(CONTENT_TYPE, v);
    }
    if let Some(i) = index {
        let h = r.headers_mut();
        h.insert("X-Consul-Index", HeaderValue::from(i));
        h.insert("X-Consul-KnownLeader", HeaderValue::from_static("true"));
        h.insert("X-Consul-LastContact", HeaderValue::from_static("0"));
    }
    r
}

fn json_reply(v: &Value, index: u64) -> Reply {
    reply(
        200,
        "application/json",
        serde_json::to_vec(v).unwrap_or_default(),
        Some(index),
    )
}

fn text(status: u16, body: &str) -> Reply {
    reply(
        status,
        "text/plain; charset=utf-8",
        body.as_bytes().to_vec(),
        None,
    )
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, op: &str, decision: &str) {
    let line = format!("Consul connection {id} operation={op} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed") || decision == "model_silent" {
        log.error(line)
    } else {
        log.info(line)
    }
}

fn failure(e: Option<&anyhow::Error>) -> Reply {
    let message = match e {
        Some(e) => crate::utils::wire_failure::prefixed_wire_failure_text(e),
        None => crate::utils::WireFailure::Unavailable.prefixed_text(),
    };
    text(500, message)
}

/// Ask the handler; one `consul_*` answer, or the reply a failure earns.
async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    op: &str,
) -> std::result::Result<(String, Value), Reply> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::ConsulProtocol,
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
    let mut answers = Vec::new();
    let mut stack = result.protocol_results;
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("consul_") => {
                answers.push((name, data))
            }
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    match answers.len() {
        1 => {
            let (name, data) = answers.pop().unwrap();
            if name == "consul_error" {
                outcome(ctx, id, op, "model_reject");
                return Err(text(
                    data["status"].as_u64().unwrap_or(500) as u16,
                    data["message"].as_str().unwrap_or_default(),
                ));
            }
            Ok((name, data))
        }
        0 => {
            outcome(ctx, id, op, "model_silent");
            Err(failure(None))
        }
        _ => {
            outcome(ctx, id, op, "fail_closed_invalid_reply");
            Err(failure(None))
        }
    }
}

fn query(q: Option<&str>) -> HashMap<String, String> {
    q.unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (
                urlencoding::decode(k)
                    .map(|c| c.into_owned())
                    .unwrap_or_default(),
                urlencoding::decode(v)
                    .map(|c| c.into_owned())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

async fn handle(
    request: Request<Incoming>,
    id: ConnectionId,
    ctx: &SpawnContext,
    index: &AtomicU64,
) -> Reply {
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(body, wire::MAX_VALUE_BYTES).collect(),
    )
    .await
    {
        Err(_) => return text(408, "request body deadline exceeded"),
        Ok(Err(e)) => {
            return if e
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                text(413, "Request body too large, max size: 524288 bytes")
            } else {
                text(400, "incomplete HTTP body")
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
    let path = parts.uri.path().to_string();
    let q = query(parts.uri.query());
    let method = parts.method.clone();
    let now = index.load(Ordering::SeqCst);
    let Some(rest) = path.strip_prefix("/v1/") else {
        return text(404, "Not found");
    };
    if let Some(key) = rest.strip_prefix("kv/") {
        let key = urlencoding::decode(key)
            .map(|c| c.into_owned())
            .unwrap_or_default();
        if wire::check_key(&key).is_err() {
            return text(400, "Invalid key");
        }
        return kv(ctx, id, index, &method, &key, &q, &bytes).await;
    }
    match (method.clone(), rest) {
        (Method::GET, "status/leader") => json_reply(&json!("127.0.0.1:8300"), now),
        (Method::GET, "status/peers") => json_reply(&json!(["127.0.0.1:8300"]), now),
        (Method::GET, "agent/self") => json_reply(&wire::agent_self(), now),
        (Method::GET, "catalog/datacenters") => json_reply(&json!([wire::DATACENTER]), now),
        (Method::GET, "catalog/nodes") => json_reply(
            &json!([{"ID": "00000000-0000-0000-0000-00000000c0de", "Node": wire::NODE, "Address": "127.0.0.1",
                     "Datacenter": wire::DATACENTER, "TaggedAddresses": {"lan": "127.0.0.1", "wan": "127.0.0.1"},
                     "Meta": {}, "CreateIndex": 1, "ModifyIndex": 1}]),
            now,
        ),
        (Method::GET, "catalog/services") => catalog(ctx, id, now, "services", None).await,
        (Method::GET, "agent/services") => catalog(ctx, id, now, "agent_services", None).await,
        (Method::GET, r) if r.starts_with("catalog/service/") => {
            catalog(
                ctx,
                id,
                now,
                "service",
                Some(&r["catalog/service/".len()..]),
            )
            .await
        }
        (Method::GET, r) if r.starts_with("health/service/") => {
            catalog(ctx, id, now, "health", Some(&r["health/service/".len()..])).await
        }
        (Method::PUT, "agent/service/register") => {
            let def = match serde_json::from_slice::<Value>(&bytes)
                .map_err(anyhow::Error::from)
                .and_then(|b| wire::registration(&b))
            {
                Ok(d) => d,
                Err(e) => return text(400, &format!("Request decode failed: {e:#}")),
            };
            register(
                ctx,
                id,
                index,
                json!({"operation": "register", "service": def}),
            )
            .await
        }
        (Method::PUT, r) if r.starts_with("agent/service/deregister/") => {
            let sid = &r["agent/service/deregister/".len()..];
            register(
                ctx,
                id,
                index,
                json!({"operation": "deregister", "id": sid}),
            )
            .await
        }
        _ => text(
            404,
            "Not found: this agent serves kv, catalog, health, agent services and status",
        ),
    }
}

async fn kv(
    ctx: &SpawnContext,
    id: ConnectionId,
    index: &AtomicU64,
    method: &Method,
    key: &str,
    q: &HashMap<String, String>,
    body: &[u8],
) -> Reply {
    let now = index.load(Ordering::SeqCst);
    let flag = |k: &str| q.contains_key(k);
    match *method {
        Method::GET => {
            let recurse = flag("recurse");
            let keys_only = flag("keys");
            let event = Event::new(
                &actions::KV_READ_EVENT,
                json!({"key": key, "recurse": recurse, "keys_only": keys_only}),
            );
            let (name, data) = match ask(ctx, id, event, "kv_read").await {
                Ok(a) => a,
                Err(r) => return r,
            };
            if name == "consul_not_found" {
                outcome(ctx, id, "kv_read", "model_answer");
                let mut r = text(404, "");
                r.headers_mut()
                    .insert("X-Consul-Index", HeaderValue::from(now));
                return r;
            }
            if name != "consul_kv_entries" {
                outcome(ctx, id, "kv_read", "fail_closed_invalid_reply");
                return failure(None);
            }
            let entries: Vec<Value> = match data["entries"].as_array().map(|a| {
                a.iter()
                    .map(|e| wire::kv_entry(e, now))
                    .collect::<Result<Vec<_>>>()
            }) {
                Some(Ok(e)) => e,
                _ => {
                    outcome(ctx, id, "kv_read", "fail_closed_invalid_reply");
                    return failure(None);
                }
            };
            outcome(ctx, id, "kv_read", "model_answer");
            if entries.is_empty() {
                return text(404, "");
            }
            if keys_only {
                let keys: Vec<&Value> = entries.iter().map(|e| &e["Key"]).collect();
                return json_reply(&json!(keys), now);
            }
            if flag("raw") {
                let v = entries[0]["Value"]
                    .as_str()
                    .map(wire::unb64)
                    .transpose()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                return reply(200, "text/plain; charset=utf-8", v, Some(now));
            }
            let shown = if recurse {
                entries
            } else {
                entries.into_iter().take(1).collect()
            };
            json_reply(&Value::Array(shown), now)
        }
        Method::PUT | Method::DELETE => {
            let cas = q.get("cas").and_then(|c| c.parse::<u64>().ok());
            let (event, op) = if *method == Method::PUT {
                let (value, encoding) = wire::shown(body);
                let flags = q
                    .get("flags")
                    .and_then(|f| f.parse::<u64>().ok())
                    .unwrap_or(0);
                (
                    Event::new(
                        &actions::KV_WRITE_EVENT,
                        json!({"key": key, "value": value, "value_encoding": encoding, "flags": flags, "cas": cas}),
                    ),
                    "kv_write",
                )
            } else {
                (
                    Event::new(
                        &actions::KV_DELETE_EVENT,
                        json!({"key": key, "recurse": flag("recurse"), "cas": cas}),
                    ),
                    "kv_delete",
                )
            };
            let (name, _) = match ask(ctx, id, event, op).await {
                Ok(a) => a,
                Err(r) => return r,
            };
            let accepted = match name.as_str() {
                "consul_ok" => true,
                "consul_refuse" => false,
                _ => {
                    outcome(ctx, id, op, "fail_closed_invalid_reply");
                    return failure(None);
                }
            };
            outcome(
                ctx,
                id,
                op,
                if accepted {
                    "model_answer"
                } else {
                    "model_reject"
                },
            );
            let i = if accepted {
                index.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                now
            };
            json_reply(&json!(accepted), i)
        }
        _ => text(405, "Method not allowed"),
    }
}

async fn catalog(
    ctx: &SpawnContext,
    id: ConnectionId,
    now: u64,
    endpoint: &str,
    name: Option<&str>,
) -> Reply {
    let name = name.map(|n| {
        urlencoding::decode(n)
            .map(|c| c.into_owned())
            .unwrap_or_default()
    });
    let event = Event::new(
        &actions::CATALOG_EVENT,
        json!({"endpoint": endpoint, "name": name}),
    );
    let (action, data) = match ask(ctx, id, event, endpoint).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let body = match (endpoint, action.as_str()) {
        ("services", "consul_services") => {
            let mut out = Map::new();
            for (k, v) in data["services"].as_object().cloned().unwrap_or_default() {
                let tags: Vec<Value> = v
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(Value::is_string)
                    .collect();
                out.insert(k, Value::Array(tags));
            }
            Value::Object(out)
        }
        (_, "consul_instances") if endpoint != "services" => {
            let list: Result<Vec<wire::Instance>> = data["instances"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(wire::instance)
                .collect();
            let Ok(list) = list else {
                outcome(ctx, id, endpoint, "fail_closed_invalid_reply");
                return failure(None);
            };
            match endpoint {
                "service" => {
                    Value::Array(list.iter().map(|i| wire::catalog_entry(i, now)).collect())
                }
                "health" => Value::Array(list.iter().map(|i| wire::health_entry(i, now)).collect()),
                _ => Value::Object(
                    list.iter()
                        .map(|i| (i.id.clone(), wire::agent_service(i)))
                        .collect(),
                ),
            }
        }
        _ => {
            outcome(ctx, id, endpoint, "fail_closed_invalid_reply");
            return failure(None);
        }
    };
    outcome(ctx, id, endpoint, "model_answer");
    json_reply(&body, now)
}

async fn register(ctx: &SpawnContext, id: ConnectionId, index: &AtomicU64, data: Value) -> Reply {
    let op = data["operation"].as_str().unwrap_or("register").to_string();
    let (name, _) = match ask(ctx, id, Event::new(&actions::REGISTER_EVENT, data), &op).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    if name != "consul_ok" {
        outcome(ctx, id, &op, "fail_closed_invalid_reply");
        return failure(None);
    }
    outcome(ctx, id, &op, "model_answer");
    let i = index.fetch_add(1, Ordering::SeqCst) + 1;
    reply(200, "text/plain; charset=utf-8", Vec::new(), Some(i))
}
