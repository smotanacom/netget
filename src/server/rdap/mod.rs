//! RDAP over HTTP/1.1: Rust parses and normalizes every RFC 9082 query and enforces the
//! RFC 9083 envelope on every answer; the handler supplies the registration data.
pub mod actions;
pub mod query;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value;
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_HEADERS: usize = 64;
const CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\nRetry-After: 5\r\n\r\n";

struct Shared {
    ctx: SpawnContext,
    base: String,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let base = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_string("base_path"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| query::DEFAULT_BASE_PATH.into());
    ensure!(
        base.starts_with('/') && base.len() <= 128 && !base.contains(['?', '#', ' ']),
        "base_path must be an absolute path without query"
    );
    let base = base.trim_end_matches('/').to_owned();
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("RDAP listening on http://{local}{base}/"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        base,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                CAP_REFUSAL,
                "RDAP",
                Some(&shared.ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
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
                    let service_shared = child.clone();
                    let service = service_fn(move |request| {
                        let shared = service_shared.clone();
                        async move { Ok::<_, Infallible>(handle(&shared, id, request).await) }
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
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("RDAP connection {id}: {e}"));
                    }
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

fn json_response(status: u16, body: &Value, head: bool) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let mut r = Response::new(Full::new(if head {
        Bytes::new()
    } else {
        Bytes::from(bytes)
    }));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(query::MEDIA_TYPE),
    );
    // RFC 7480 §5.6: RDAP is meant to be readable from browser scripts.
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static("*"),
    );
    r
}

fn error_response(status: u16, title: &str, head: bool) -> Response<Full<Bytes>> {
    json_response(status, &query::error_body(status, title, &[]), head)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("RDAP connection {id} query={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn decide(shared: &Shared, id: ConnectionId, event: Event) -> Result<Value> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RdapProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        bail!("RDAP handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == "rdap_response" => {
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("RDAP handler supplied more than one response");
                }
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    found
        .context("RDAP handler did not answer")
        .inspect_err(|_| outcome(ctx, id, event.id(), "model_silent"))
}

async fn handle(
    shared: &Shared,
    id: ConnectionId,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let ctx = &shared.ctx;
    let received = request.uri().to_string().len()
        + request
            .headers()
            .iter()
            .map(|(k, v)| k.as_str().len() + v.len() + 4)
            .sum::<usize>();
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(received as u64),
            None,
            Some(1),
            None,
        )
        .await;
    let head = request.method() == Method::HEAD;
    if request.method() != Method::GET && !head {
        let mut r = error_response(405, "Method Not Allowed", false);
        r.headers_mut()
            .insert(header::ALLOW, header::HeaderValue::from_static("GET, HEAD"));
        return r;
    }
    let path = request.uri().path().to_owned();
    let Some(relative) = path
        .strip_prefix(shared.base.as_str())
        .filter(|r| shared.base.is_empty() || r.is_empty() || r.starts_with('/'))
    else {
        return error_response(404, "Not Found", head);
    };
    let query = match query::parse(relative, request.uri().query()) {
        Ok(q) => q,
        // RFC 7480 §5.4: a malformed query is 400 and costs no handler call.
        Err(e) => {
            outcome(ctx, id, relative, "protocol_refusal");
            return json_response(
                400,
                &query::error_body(400, "Bad Request", &[e.to_string()]),
                head,
            );
        }
    };
    let mut data = query.to_event();
    data["method"] = Value::from(if head { "HEAD" } else { "GET" });
    let label = data["query_type"].as_str().unwrap_or("rdap").to_owned();
    let response = match decide(shared, id, Event::new(&actions::QUERY_EVENT, data)).await {
        Err(e) => {
            let (status, title) = if crate::utils::WireFailure::classify(&e).is_overloaded() {
                (503, "Service Unavailable")
            } else {
                (500, "Internal Server Error")
            };
            let mut r = error_response(status, title, head);
            if status == 503 {
                r.headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
            }
            r
        }
        Ok(v) => match query::answer(&query, &v) {
            Ok(query::Answer::Body { status, body }) => {
                outcome(
                    ctx,
                    id,
                    &label,
                    if status < 300 {
                        "model_answer"
                    } else {
                        "model_reject"
                    },
                );
                json_response(status, &body, head)
            }
            Ok(query::Answer::Redirect(url)) => {
                outcome(ctx, id, &label, "model_redirect");
                let mut r = Response::new(Full::new(Bytes::new()));
                *r.status_mut() = StatusCode::FOUND;
                if let Ok(v) = header::HeaderValue::from_str(&url) {
                    r.headers_mut().insert(header::LOCATION, v);
                }
                r.headers_mut().insert(
                    header::ACCESS_CONTROL_ALLOW_ORIGIN,
                    header::HeaderValue::from_static("*"),
                );
                r
            }
            Err(e) => {
                outcome(ctx, id, &label, "fail_closed_invalid_reply");
                Log::new(Some(&ctx.status_tx))
                    .warn(format!("RDAP connection {id}: handler answer refused: {e}"));
                error_response(500, "Internal Server Error", head)
            }
        },
    };
    let sent = hyper::body::Body::size_hint(response.body())
        .exact()
        .unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    response
}
