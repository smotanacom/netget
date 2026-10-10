//! Zipkin collector and read API over HTTP/1.1. Rust owns HTTP, gzip, span validation and
//! every bound; the handler decides whether a report is accepted and answers every query.
//! NetGet stores no spans.
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
use crate::state::AccessLogOwner;
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming,
    header::{HeaderValue, ALLOW, CONTENT_ENCODING, CONTENT_TYPE, RETRY_AFTER},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Map, Value};
use std::{collections::BTreeSet, convert::Infallible, net::SocketAddr, time::Duration};

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_HEADERS: usize = 64;
const CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\nRetry-After: 5\r\n\r\n";

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Zipkin collector listening on {local}"));
    let server_id = ctx.server_id;
    let state = ctx.state.clone();
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                CAP_REFUSAL,
                "Zipkin",
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
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let request_ctx = child.clone();
                    let service = service_fn(move |request| {
                        let ctx = request_ctx.clone();
                        async move { Ok::<_, Infallible>(handle(request, peer, id, &ctx).await) }
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
                            .warn(format!("Zipkin connection {id} HTTP error: {e}"));
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

fn text(status: u16, body: &str) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    r
}

fn accepted() -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = StatusCode::ACCEPTED;
    r
}

fn json_body(value: &Value) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from(
        serde_json::to_vec(value).unwrap_or_default(),
    )));
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("Zipkin connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

fn failure(error: Option<&anyhow::Error>) -> Response<Full<Bytes>> {
    let message = match error {
        Some(e) => crate::utils::wire_failure::prefixed_wire_failure_text(e),
        None => crate::utils::WireFailure::Unavailable.prefixed_text(),
    };
    let mut r = text(503, message);
    r.headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("5"));
    r
}

/// The handler's answer: its actions, flattened, or the response that a failure earns.
async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: &Event,
    op: &str,
) -> std::result::Result<Vec<(String, Value)>, Response<Full<Bytes>>> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        event,
        &actions::ZipkinProtocol,
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
    let mut out = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("zipkin_") => {
                out.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    out.reverse();
    Ok(out)
}

fn rejected(data: &Value) -> Response<Full<Bytes>> {
    text(
        data["status"].as_u64().unwrap_or(500) as u16,
        data["message"].as_str().unwrap_or_default(),
    )
}

async fn handle(
    request: Request<Incoming>,
    peer: SocketAddr,
    id: ConnectionId,
    ctx: &SpawnContext,
) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(body, wire::MAX_BODY_BYTES).collect(),
    )
    .await
    {
        Err(_) => return text(408, "request body deadline exceeded"),
        Ok(Err(e)) => {
            return if e
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                text(413, "request body exceeds 1 MiB")
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
    let path = parts.uri.path();
    let Some(endpoint) = path.strip_prefix("/api/v2/") else {
        return text(404, "only the /api/v2 API is implemented");
    };
    if endpoint == "spans" && parts.method == Method::POST {
        return report(&parts.headers, &bytes, peer, id, ctx).await;
    }
    if parts.method != Method::GET {
        let mut r = text(405, "only GET, and POST to /api/v2/spans");
        r.headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET"));
        return r;
    }
    query(endpoint, parts.uri.query(), peer, id, ctx).await
}

async fn report(
    headers: &hyper::HeaderMap,
    bytes: &[u8],
    peer: SocketAddr,
    id: ConnectionId,
    ctx: &SpawnContext,
) -> Response<Full<Bytes>> {
    if headers.get_all(CONTENT_TYPE).iter().count() > 1
        || headers.get_all(CONTENT_ENCODING).iter().count() > 1
    {
        return text(400, "duplicate content type or encoding");
    }
    if let Some(ct) = headers.get(CONTENT_TYPE) {
        let mime = ct
            .to_str()
            .ok()
            .and_then(|s| s.split(';').next())
            .map(|s| s.trim().to_ascii_lowercase());
        if mime.as_deref() != Some("application/json") {
            return text(415, "only JSON v2 spans (application/json) are accepted");
        }
    }
    let encoding = match headers.get(CONTENT_ENCODING).map(|v| v.to_str()) {
        None => "identity",
        Some(Ok(e)) if e.eq_ignore_ascii_case("gzip") => "gzip",
        Some(Ok(e)) if e.eq_ignore_ascii_case("identity") => "identity",
        _ => return text(415, "only identity and gzip content encodings"),
    };
    let body = match wire::decode_body(bytes, encoding) {
        Ok(b) => b,
        Err(e) if e.to_string().contains("limit") => {
            return text(413, "decompressed body exceeds 1 MiB")
        }
        Err(_) => return text(400, "malformed gzip body"),
    };
    let spans = match serde_json::from_slice::<Value>(&body)
        .map_err(anyhow::Error::from)
        .and_then(|v| wire::spans(&v))
    {
        Ok(s) => s,
        Err(e) => return text(400, &format!("{e:#} reading List<Span> from json")),
    };
    if spans.is_empty() {
        return accepted();
    }
    let services: BTreeSet<&str> = spans
        .iter()
        .filter_map(|s| s["localEndpoint"]["serviceName"].as_str())
        .collect();
    let event = Event::new(
        &actions::SPANS_EVENT,
        json!({"spans": spans, "span_count": spans.len(), "services": services, "remote_addr": peer.to_string()}),
    );
    let op = "zipkin_spans";
    let answers = match ask(ctx, id, &event, op).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    match answers.as_slice() {
        [] => {
            outcome(ctx, id, op, "model_silent_accepted");
            accepted()
        }
        [(name, _)] if name == "zipkin_accept" => {
            outcome(ctx, id, op, "model_answer");
            accepted()
        }
        [(name, data)] if name == "zipkin_reject" => {
            outcome(ctx, id, op, "model_reject");
            rejected(data)
        }
        _ => {
            outcome(ctx, id, op, "fail_closed_invalid_reply");
            failure(None)
        }
    }
}

async fn query(
    endpoint: &str,
    raw_query: Option<&str>,
    peer: SocketAddr,
    id: ConnectionId,
    ctx: &SpawnContext,
) -> Response<Full<Bytes>> {
    let (name, trace_id) = match endpoint.split_once('/') {
        Some(("trace", tid)) => match wire::trace_id(&json!(tid)) {
            Ok(t) => ("trace", Some(t)),
            Err(e) => return text(400, &e.to_string()),
        },
        Some(_) => return text(404, "unknown /api/v2 endpoint"),
        None if endpoint == "trace" => return text(404, "trace id required"),
        None => (endpoint, None),
    };
    let Some((allowed, shape)) = wire::endpoint_shape(name) else {
        return text(404, "unknown /api/v2 endpoint");
    };
    let mut params = Map::new();
    for pair in raw_query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (Ok(k), Ok(v)) = (wire::unescape(k), wire::unescape(v)) else {
            return text(400, "malformed query string");
        };
        if !allowed.contains(&k.as_str()) {
            return text(400, &format!("unsupported query parameter {k}"));
        }
        if params.insert(k.clone(), json!(v)).is_some() {
            return text(400, &format!("duplicate query parameter {k}"));
        }
    }
    let mut data = json!({"endpoint": name, "query": params, "remote_addr": peer.to_string()});
    if let Some(t) = &trace_id {
        data["trace_id"] = json!(t);
    }
    let event = Event::new(&actions::QUERY_EVENT, data);
    let op = "zipkin_query";
    let answers = match ask(ctx, id, &event, op).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    match answers.as_slice() {
        [(n, data)] if n == "zipkin_query_result" => match wire::result(shape, &data["result"]) {
            Ok(result) => {
                outcome(ctx, id, op, "model_answer");
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Server(ctx.server_id.as_u32()),
                        "Zipkin",
                        Some(id.as_u32()),
                        "zipkin_query_answered",
                        json!({"endpoint": name, "items": result.as_array().map_or(0, Vec::len)}),
                        vec![],
                    )
                    .await;
                json_body(&result)
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx))
                    .error(format!("Zipkin {name} answer has the wrong shape: {e:#}"));
                outcome(ctx, id, op, "fail_closed_invalid_reply");
                failure(None)
            }
        },
        [(n, data)] if n == "zipkin_reject" => {
            outcome(ctx, id, op, "model_reject");
            rejected(data)
        }
        [] => {
            outcome(ctx, id, op, "model_silent");
            failure(None)
        }
        _ => {
            outcome(ctx, id, op, "fail_closed_invalid_reply");
            failure(None)
        }
    }
}
