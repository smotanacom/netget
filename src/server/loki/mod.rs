//! One-request-per-connection HTTP/1.1 push collector with owned tasks.
pub mod actions;
pub mod codec;
use crate::protocol::{Event, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::{
    server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
    AccessLogOwner,
};
use crate::{console_error, console_info};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming,
    header::{HeaderValue, ALLOW, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE, RETRY_AFTER},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::json;
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Notify;

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_HEADERS: usize = 64;
const CAP_REFUSAL:&[u8]=b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 19\r\nConnection: close\r\nRetry-After: 5\r\n\r\nconnection capacity";
#[derive(Clone)]
struct Config {
    token: Option<String>,
    llm_fallback: bool,
    require_tenant: bool,
}
pub struct LokiServer;
impl LokiServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
        let token = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_string("auth_token"))
            .transpose()?
            .flatten();
        if let Some(token) = &token {
            codec::validate_token(token)?;
        }
        let llm_fallback = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_bool("llm_fallback"))
            .transpose()?
            .flatten()
            .unwrap_or(codec::DEFAULT_LLM_FALLBACK);
        let require_tenant = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_bool("require_tenant"))
            .transpose()?
            .flatten()
            .unwrap_or(false);
        let config = Arc::new(Config {
            token,
            llm_fallback,
            require_tenant,
        });
        let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
        let local = listener.local_addr()?;
        console_info!(
            ctx.status_tx,
            "Loki push listening on {} (auth_required={},llm_fallback={})",
            local,
            config.token.is_some(),
            llm_fallback
        );
        let owner = ctx.state.clone();
        let server_id = ctx.server_id;
        owner.spawn_server_task(server_id,async move {
            let limiter=ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                let (stream,peer,permit)=match accept_bounded(&listener,&limiter,CAP_REFUSAL,"Loki",Some(&ctx.status_tx)).await {
                    Ok(value)=>value,
                    Err(error)=>{console_error!(ctx.status_tx,"Loki accept failed: {}",error);break;}
                };
                let id=ConnectionId::new(ctx.state.get_next_unified_id().await);
                let now=crate::utils::clock::Instant::now();
                ctx.state.add_connection_to_server(server_id,ConnectionState {id,remote_addr:peer,local_addr:local,bytes_sent:0,bytes_received:0,packets_sent:0,packets_received:0,last_activity:now,status:ConnectionStatus::Active,status_changed_at:now,protocol_info:ProtocolConnectionInfo::empty()}).await;
                let child_ctx=ctx.clone(); let config=config.clone();
                ctx.state.spawn_server_task(server_id,async move {
                    let _permit=permit;
                    let write_ready=Arc::new(Notify::new());
                    let service_ready=write_ready.clone();
                    let request_ctx=child_ctx.clone();
                    let service=service_fn(move |request| {
                        let ctx=request_ctx.clone(); let config=config.clone(); let ready=service_ready.clone();
                        async move {
                            let response=handle_request(request,peer,id,&ctx,&config).await;
                            ready.notify_one();
                            Ok::<_,Infallible>(response)
                        }
                    });
                    let mut builder=http1::Builder::new();
                    builder.keep_alive(false).timer(TokioTimer::new()).header_read_timeout(HEADER_TIMEOUT).max_headers(MAX_HEADERS).max_buf_size(MAX_HEADER_BYTES);
                    let connection=builder.serve_connection(TokioIo::new(stream),service);
                    tokio::pin!(connection);
                    tokio::select! {
                        result=&mut connection=>{if let Err(error)=result {console_error!(child_ctx.status_tx,"Loki HTTP framing/header error: {}",error);}}
                        _=async {write_ready.notified().await;tokio::time::sleep(WRITE_TIMEOUT).await;}=>{console_error!(child_ctx.status_tx,"Loki decision=fail_closed_response_write_timeout");}
                    }
                    child_ctx.state.remove_connection_from_server(server_id,id).await;
                    let _=child_ctx.status_tx.send("__UPDATE_UI__".into());
                }).await;
            }
        }).await;
        Ok(local)
    }
}
fn response(status: u16, message: &str, retry: Option<u16>) -> Response<Full<Bytes>> {
    let bytes = if status == 204 {
        Vec::new()
    } else {
        message.as_bytes().to_vec()
    };
    let mut r = Response::new(Full::new(Bytes::from(bytes)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if let Some(n) = retry {
        if let Ok(v) = HeaderValue::from_str(&n.to_string()) {
            r.headers_mut().insert(RETRY_AFTER, v);
        }
    }
    r
}
fn error(status: u16, message: &str) -> Response<Full<Bytes>> {
    response(status, message, None)
}
fn authorized(headers: &hyper::HeaderMap, expected: Option<&str>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    let values = headers.get_all(AUTHORIZATION).iter().collect::<Vec<_>>();
    if values.len() != 1 {
        return false;
    }
    let Some((scheme, token)) = values[0].to_str().ok().and_then(|v| v.split_once(' ')) else {
        return false;
    };
    if scheme != "Bearer" || codec::validate_token(token).is_err() {
        return false;
    }
    let mut difference = expected.len() ^ token.len();
    for at in 0..expected.len().max(token.len()) {
        difference |= usize::from(
            expected.as_bytes().get(at).copied().unwrap_or(0)
                ^ token.as_bytes().get(at).copied().unwrap_or(0),
        );
    }
    difference == 0
}
async fn handle_request(
    request: Request<Incoming>,
    peer: SocketAddr,
    id: ConnectionId,
    ctx: &SpawnContext,
    config: &Config,
) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    if parts.headers.len() > MAX_HEADERS
        || parts
            .headers
            .iter()
            .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
            .sum::<usize>()
            > MAX_HEADER_BYTES
    {
        return error(413, "Request header byte/count limit");
    }
    let bytes = match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(body, codec::MAX_BODY_BYTES).collect(),
    )
    .await
    {
        Err(_) => return error(408, "Request body deadline exceeded"),
        Ok(Err(error)) => {
            return if error
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                self::error(413, "Request body limit exceeded")
            } else {
                self::error(400, "Incomplete HTTP body")
            };
        }
        Ok(Ok(body)) => body.to_bytes(),
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
    if parts.uri.path() != "/loki/api/v1/push" {
        return error(404, "Only the push endpoint is implemented");
    }
    if parts.method != Method::POST {
        let mut r = error(405, "Only POST is supported");
        r.headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return r;
    }
    if parts.uri.query().is_some() {
        return error(400, "Push query parameters unsupported");
    }
    if !authorized(&parts.headers, config.token.as_deref()) {
        return error(401, "Unauthorized bearer credential");
    }
    let tenants = parts
        .headers
        .get_all("X-Scope-OrgID")
        .iter()
        .collect::<Vec<_>>();
    if tenants.len() > 1 {
        return error(400, "Duplicate tenant header");
    }
    let tenant = match tenants.first() {
        Some(v) => match v.to_str() {
            Ok(t) if codec::validate_tenant(t).is_ok() => Some(t.to_owned()),
            _ => return error(400, "Invalid single tenant ID"),
        },
        None => None,
    };
    if config.require_tenant && tenant.is_none() {
        return error(401, "Tenant ID required");
    }
    if parts.headers.get_all(CONTENT_ENCODING).iter().count() > 1
        || parts.headers.get_all(CONTENT_TYPE).iter().count() > 1
    {
        return error(400, "Duplicate encoding/content type headers");
    }
    let content_type = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let content_encoding = match parts
        .headers
        .get(CONTENT_ENCODING)
        .map(|v| v.to_str())
        .transpose()
    {
        Ok(Some(v)) => v,
        Ok(None) => "identity",
        Err(_) => return error(415, "Invalid content encoding"),
    };
    let encoding = match (content_type, content_encoding) {
        ("application/json", "identity") => codec::Encoding::Json,
        ("application/json", "gzip") => codec::Encoding::GzipJson,
        ("application/x-protobuf", "identity" | "snappy") => codec::Encoding::SnappyProtobuf,
        _ => return error(415, "Expected JSON, gzip JSON or Snappy protobuf"),
    };
    let streams = match codec::decode_batch(&bytes, encoding) {
        Ok(v) => v,
        Err(e) => {
            return error(
                if e.to_string().contains("limit") {
                    413
                } else {
                    400
                },
                "Malformed/oversized Loki push",
            )
        }
    };
    let event = Event::new(
        &actions::LOKI_PUSH_EVENT,
        json!({"tenant_id":tenant.as_deref().unwrap_or("fake"),"tenant_provided":tenant.is_some(),"encoding":encoding,"streams":streams,"authenticated":true,"auth_required":config.token.is_some(),"source_addr":peer.to_string()}),
    );
    let configured = ctx
        .state
        .get_event_handler_config(ctx.server_id)
        .await
        .is_some_and(|c| c.find_handler("loki_push").is_some());
    let decision = if configured || config.llm_fallback {
        match crate::llm::action_helper::call_llm(
            &ctx.llm_client,
            &ctx.state,
            ctx.server_id,
            Some(id),
            &event,
            &actions::LokiProtocol::new(),
        )
        .await
        {
            Ok(result) if result.failures.is_empty() => {
                let decisions = result
                    .protocol_results
                    .iter()
                    .filter_map(|r| match r {
                        crate::llm::ActionResult::Custom { name, data }
                            if name == "loki_push_decision" =>
                        {
                            Some(data.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if decisions.len() > 1 {
                    return failed(ctx, id, event.data, "fail_closed_multiple_decisions").await;
                }
                match decisions
                    .first()
                    .map(|d| serde_json::from_value::<actions::Decision>(d.clone()))
                    .transpose()
                {
                    Ok(Some(d)) => d,
                    Ok(None) => actions::Decision::Accept,
                    Err(_) => {
                        return failed(ctx, id, event.data, "fail_closed_invalid_decision").await
                    }
                }
            }
            Ok(_) => return failed(ctx, id, event.data, "fail_closed_handler_action_error").await,
            Err(_) => return failed(ctx, id, event.data, "fail_closed_handler_error").await,
        }
    } else {
        ctx.state
            .record_access_log(
                AccessLogOwner::Server(ctx.server_id.as_u32()),
                "Loki",
                Some(id.as_u32()),
                "loki_push",
                event.data.clone(),
                vec![json!({"type":"accept_loki_entries"})],
            )
            .await;
        actions::Decision::Accept
    };
    match decision {
        actions::Decision::Accept => response(204, "", None),
        actions::Decision::Reject {
            status,
            message,
            retry_after_seconds,
        } => response(status, &message, retry_after_seconds),
    }
}
async fn failed(
    ctx: &SpawnContext,
    id: ConnectionId,
    data: serde_json::Value,
    decision: &str,
) -> Response<Full<Bytes>> {
    console_error!(ctx.status_tx, "Loki decision={}", decision);
    ctx.state
        .record_access_log(
            AccessLogOwner::Server(ctx.server_id.as_u32()),
            "Loki",
            Some(id.as_u32()),
            "loki_handler_failed",
            data,
            vec![json!({"decision":decision})],
        )
        .await;
    error(503, "Push handler failed; no acceptance confirmed")
}
