//! Anthropic Messages API. Rust owns HTTP, validation and Anthropic's error envelope, the
//! optional API key check, ids, usage estimates, the event stream, count_tokens and the models
//! list; the handler writes every assistant message.
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
use anyhow::{ensure, Result};
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
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const BODY_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_HEADERS: usize = 100;
/// Requests served on one keep-alive connection before it is closed.
pub const MAX_REQUESTS_PER_CONNECTION: usize = 1000;
const CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\nRetry-After: 5\r\n\r\n";

type Reply = Response<Full<Bytes>>;

struct Config {
    api_key: Option<String>,
    models: Vec<String>,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let api_key = params
        .map(|p| p.get_optional_string("api_key"))
        .transpose()?
        .flatten()
        .filter(|k| !k.is_empty());
    let models: Vec<String> = match params
        .map(|p| p.get_optional_array("models"))
        .transpose()?
        .flatten()
    {
        Some(list) => list
            .iter()
            .map(|m| m.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| anyhow::anyhow!("models must be an array of strings"))?,
        None => vec![wire::DEFAULT_MODEL.to_string()],
    };
    ensure!(
        !models.is_empty()
            && models.len() <= 100
            && models.iter().all(|m| !m.is_empty() && m.len() <= 256),
        "models must list 1..=100 non-empty ids"
    );
    let config = Arc::new(Config { api_key, models });
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Anthropic Messages API listening on {local}"));
    let server_id = ctx.server_id;
    let state = ctx.state.clone();
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                CAP_REFUSAL,
                "Anthropic",
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
            let config = config.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let request_ctx = child.clone();
                    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let service = service_fn(move |request| {
                        let ctx = request_ctx.clone();
                        let config = config.clone();
                        let served = served.clone();
                        async move {
                            let n = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            let mut r = handle(request, id, &ctx, &config).await;
                            if n >= MAX_REQUESTS_PER_CONNECTION {
                                r.headers_mut().insert(
                                    hyper::header::CONNECTION,
                                    HeaderValue::from_static("close"),
                                );
                            }
                            Ok::<_, Infallible>(r)
                        }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .keep_alive(true)
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(MAX_HEADERS)
                        .max_buf_size(MAX_HEADER_BYTES);
                    if let Err(e) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("Anthropic connection {id} HTTP error: {e}"));
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

fn reply(status: u16, content_type: &'static str, body: Vec<u8>, request_id: &str) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = r.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(v) = HeaderValue::from_str(request_id) {
        h.insert("request-id", v);
    }
    if content_type == "text/event-stream" {
        h.insert(
            hyper::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
    }
    r
}

fn json_reply(v: &Value, request_id: &str) -> Reply {
    reply(
        200,
        "application/json",
        serde_json::to_vec(v).unwrap_or_default(),
        request_id,
    )
}

fn error(error_type: &str, message: &str, request_id: &str) -> Reply {
    let status = wire::status_of(error_type).unwrap_or(500);
    let body = wire::error_body(error_type, message, request_id);
    reply(
        status,
        "application/json",
        serde_json::to_vec(&body).unwrap_or_default(),
        request_id,
    )
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, decision: &str) {
    let line = format!("Anthropic connection {id} operation=messages decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed") || decision == "model_silent" {
        log.error(line)
    } else {
        log.info(line)
    }
}

/// The handler failed or said nothing: overloaded (529) when the backend is busy, else an API
/// error (500). Never a fabricated message.
fn failure(e: Option<&anyhow::Error>, request_id: &str) -> Reply {
    let category = e
        .map(crate::utils::WireFailure::classify)
        .unwrap_or(crate::utils::WireFailure::Unavailable);
    let kind = if category.is_overloaded() {
        "overloaded_error"
    } else {
        "api_error"
    };
    error(kind, category.prefixed_text(), request_id)
}

/// Ask the handler for one answer to a messages request.
async fn ask(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    request_id: &str,
) -> std::result::Result<Value, Reply> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::AnthropicProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, "fail_closed_llm_error");
            return Err(failure(Some(&e), request_id));
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, "fail_closed_invalid_reply");
        return Err(failure(None, request_id));
    }
    let mut answers = Vec::new();
    let mut stack = result.protocol_results;
    while let Some(r) = stack.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("anthropic_") => {
                answers.push(data)
            }
            ActionResult::Multiple(items) => stack.extend(items),
            _ => {}
        }
    }
    match answers.len() {
        1 => {
            let a = answers.pop().unwrap_or_default();
            if a["type"] == "anthropic_error" {
                outcome(ctx, id, "model_reject");
                return Err(error(
                    a["error_type"].as_str().unwrap_or("api_error"),
                    a["message"].as_str().unwrap_or_default(),
                    request_id,
                ));
            }
            outcome(ctx, id, "model_answer");
            Ok(a)
        }
        0 => {
            outcome(ctx, id, "model_silent");
            Err(failure(None, request_id))
        }
        _ => {
            outcome(ctx, id, "fail_closed_invalid_reply");
            Err(failure(None, request_id))
        }
    }
}

fn key_of(parts: &hyper::http::request::Parts) -> Option<String> {
    if let Some(k) = parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return Some(k.to_string());
    }
    parts
        .headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Constant-time comparison, so the key cannot be found byte by byte from response timing.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

async fn handle(
    request: Request<Incoming>,
    id: ConnectionId,
    ctx: &SpawnContext,
    config: &Config,
) -> Reply {
    let request_id = wire::random_id("req_");
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(body, wire::MAX_BODY).collect(),
    )
    .await
    {
        Err(_) => {
            return error(
                "invalid_request_error",
                "request body deadline exceeded",
                &request_id,
            )
        }
        Ok(Err(e)) => {
            return if e
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                error(
                    "request_too_large",
                    "Request exceeds the maximum allowed number of bytes.",
                    &request_id,
                )
            } else {
                error("invalid_request_error", "incomplete HTTP body", &request_id)
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
    let key = key_of(&parts);
    if let Some(expected) = &config.api_key {
        if !key.as_deref().is_some_and(|k| same(k, expected)) {
            outcome(ctx, id, "auth_reject");
            return error("authentication_error", "invalid x-api-key", &request_id);
        }
    }
    let path = parts.uri.path();
    let parsed = || serde_json::from_slice::<Value>(&bytes);
    let unparsable = |e: serde_json::Error| {
        error(
            "invalid_request_error",
            &format!("There was an issue with your request body: {e}"),
            &request_id,
        )
    };
    let r = match (&parts.method, path) {
        (&Method::POST, "/v1/messages") => {
            let body = match parsed() {
                Ok(b) => b,
                Err(e) => return unparsable(e),
            };
            messages(ctx, id, &body, key.is_some(), &request_id).await
        }
        (&Method::POST, "/v1/messages/count_tokens") => {
            let body = match parsed() {
                Ok(b) => b,
                Err(e) => return unparsable(e),
            };
            match wire::request_summary(&body, true) {
                Ok(_) => json_reply(
                    &json!({"input_tokens": wire::estimate_tokens(&wire::request_text(&body))}),
                    &request_id,
                ),
                Err(e) => error("invalid_request_error", &e.to_string(), &request_id),
            }
        }
        (&Method::GET, "/v1/models") => {
            let data: Vec<Value> = config
                .models
                .iter()
                .map(|m| wire::model_object(m))
                .collect();
            json_reply(
                &json!({"data": data, "has_more": false, "first_id": config.models.first(), "last_id": config.models.last()}),
                &request_id,
            )
        }
        (&Method::GET, p) if p.starts_with("/v1/models/") => {
            let wanted = &p["/v1/models/".len()..];
            match config.models.iter().find(|m| *m == wanted) {
                Some(m) => json_reply(&wire::model_object(m), &request_id),
                None => error("not_found_error", &format!("model: {wanted}"), &request_id),
            }
        }
        _ => error("not_found_error", "Not found", &request_id),
    };
    let sent = hyper::body::Body::size_hint(r.body()).exact().unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    r
}

async fn messages(
    ctx: &SpawnContext,
    id: ConnectionId,
    body: &Value,
    has_key: bool,
    request_id: &str,
) -> Reply {
    let mut summary = match wire::request_summary(body, false) {
        Ok(s) => s,
        Err(e) => return error("invalid_request_error", &e.to_string(), request_id),
    };
    summary["api_key_present"] = json!(has_key);
    let stream = summary["stream"].as_bool().unwrap_or(false);
    let model = summary["model"].as_str().unwrap_or_default().to_string();
    let answer = match ask(
        ctx,
        id,
        Event::new(&actions::MESSAGE_EVENT, summary),
        request_id,
    )
    .await
    {
        Ok(a) => a,
        Err(r) => return r,
    };
    // execute_action checked both; a failure here is a handler answer that changed meaning.
    let content = match wire::reply_content(&answer) {
        Ok(c) => c,
        Err(_) => return failure(None, request_id),
    };
    let stop = match wire::stop_reason(&answer, &content) {
        Ok(s) => s,
        Err(_) => return failure(None, request_id),
    };
    let output_text: String = content
        .iter()
        .map(|b| match b["type"].as_str() {
            Some("tool_use") => b["input"].to_string(),
            _ => b["text"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    let input_tokens = answer["input_tokens"]
        .as_u64()
        .unwrap_or_else(|| wire::estimate_tokens(&wire::request_text(body)));
    let output_tokens = answer["output_tokens"]
        .as_u64()
        .unwrap_or_else(|| wire::estimate_tokens(&output_text).max(1));
    let message = wire::message(
        &wire::random_id("msg_"),
        &model,
        content,
        &stop,
        answer["stop_sequence"].as_str(),
        input_tokens,
        output_tokens,
    );
    if stream {
        reply(
            200,
            "text/event-stream",
            wire::event_stream(&message).into_bytes(),
            request_id,
        )
    } else {
        json_reply(&message, request_id)
    }
}
