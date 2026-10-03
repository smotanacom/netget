//! Separate binary protobuf HTTP/1.1 binding; the HTTP connection owns every RPC worker.
pub mod actions;
pub mod wire;
use crate::server::grpc::{
    self,
    streaming::{ConnectionTasks, OwnedExecutor, Registry},
    WebCore,
};
use crate::{
    protocol::SpawnContext,
    state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
};
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    HeaderMap, Method, Request, Response, StatusCode,
};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tonic::{body::BoxBody, Status};
use tower::{Layer, Service};
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_CONNECTIONS: usize = 256;
const REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: 5\r\nConnection: close\r\n\r\n";

fn empty(status: StatusCode) -> Response<BoxBody> {
    let mut response = Response::new(tonic::body::empty_body());
    *response.status_mut() = status;
    if status != StatusCode::NO_CONTENT {
        response
            .headers_mut()
            .insert("connection", "close".parse().unwrap());
    }
    response
}
fn closing_status(status: Status) -> Response<BoxBody> {
    let mut response = status_in_body(status.into_http());
    response
        .headers_mut()
        .insert("connection", "close".parse().unwrap());
    response
}
/// Always put application status in a Web trailer envelope, including failures.
fn status_in_body(mut response: Response<BoxBody>) -> Response<BoxBody> {
    if response.headers().contains_key("grpc-status") {
        let mut trailers = HeaderMap::new();
        for key in ["grpc-status", "grpc-message", "grpc-status-details-bin"] {
            if let Some(value) = response.headers_mut().remove(key) {
                trailers.insert(key, value);
            }
        }
        *response.body_mut() =
            StreamBody::new(futures::stream::iter([Ok::<_, Status>(
                Frame::<Bytes>::trailers(trailers),
            )]))
            .boxed_unsync();
    }
    response
}
fn cors(response: &mut Response<BoxBody>, origin: Option<&str>) {
    if let Some(origin) = origin {
        response
            .headers_mut()
            .insert("access-control-allow-origin", origin.parse().unwrap());
        response
            .headers_mut()
            .insert("vary", "Origin".parse().unwrap());
        response.headers_mut().insert(
            "access-control-expose-headers",
            "grpc-status,grpc-message,grpc-status-details-bin"
                .parse()
                .unwrap(),
        );
    }
}
fn allowed_origin(
    request: &Request<Incoming>,
    configured: Option<&str>,
) -> Result<Option<String>, StatusCode> {
    let mut origins = request.headers().get_all("origin").iter();
    let Some(origin) = origins.next() else {
        return Ok(None);
    };
    let origin = origin.to_str().map_err(|_| StatusCode::FORBIDDEN)?;
    if origins.next().is_some() || configured != Some(origin) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Some(origin.into()))
}
async fn request(
    mut request: Request<Incoming>,
    core: WebCore,
    connection: crate::server::connection::ConnectionId,
    activity: Arc<crate::server::accept_bounded::ConnectionActivity>,
    registry: Registry,
    executor: OwnedExecutor,
    allow_origin: Option<String>,
) -> Response<BoxBody> {
    if !wire::bounded_headers(request.headers()) {
        return empty(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
    }
    let origin = match allowed_origin(&request, allow_origin.as_deref()) {
        Ok(origin) => origin,
        Err(code) => return empty(code),
    };
    if request.method() == Method::OPTIONS {
        let allowed = [
            "content-type",
            "x-grpc-web",
            "x-user-agent",
            "grpc-timeout",
            "grpc-encoding",
            "grpc-accept-encoding",
        ];
        let headers_ok = request
            .headers()
            .get("access-control-request-headers")
            .map(|value| {
                value.to_str().is_ok_and(|value| {
                    value.len() <= 1024
                        && value.split(',').count() <= 32
                        && value.split(',').all(|header| {
                            allowed.contains(&header.trim().to_ascii_lowercase().as_str())
                        })
                })
            })
            .unwrap_or(true);
        if origin.is_none()
            || request
                .headers()
                .get("access-control-request-method")
                .is_none_or(|value| value != "POST")
            || request
                .headers()
                .get_all("access-control-request-method")
                .iter()
                .count()
                != 1
            || request
                .headers()
                .get_all("access-control-request-headers")
                .iter()
                .count()
                > 1
            || !headers_ok
        {
            return empty(StatusCode::FORBIDDEN);
        }
        let mut response = empty(StatusCode::NO_CONTENT);
        cors(&mut response, origin.as_deref());
        response
            .headers_mut()
            .insert("access-control-allow-methods", "POST".parse().unwrap());
        response.headers_mut().insert(
            "access-control-allow-headers",
            allowed.join(",").parse().unwrap(),
        );
        return response;
    }
    if request.method() != Method::POST {
        return empty(StatusCode::METHOD_NOT_ALLOWED);
    }
    if !wire::binary_content_type(request.headers())
        || request.headers().get_all("accept").iter().any(|value| {
            value
                .to_str()
                .map_or(true, |value| value.contains("grpc-web-text"))
        })
        || request.headers().contains_key("content-encoding")
    {
        return empty(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if request.headers().get_all("grpc-encoding").iter().count() > 1 {
        return empty(StatusCode::BAD_REQUEST);
    }
    if request.uri().path().len() > 2048 || request.uri().query().is_some() {
        return empty(StatusCode::BAD_REQUEST);
    }
    // tonic-web chooses response encoding from Accept, so pin the selected binary subset.
    request
        .headers_mut()
        .insert("accept", "application/grpc-web+proto".parse().unwrap());
    let busy = activity.busy();
    let body = request.map(|body| {
        body.map_err(|_| Status::unavailable("request transport failed"))
            .boxed_unsync()
    });
    let mut busy = Some(busy);
    let mut service = tonic_web::GrpcWebLayer::new().layer(tower::service_fn(
        move |request: Request<BoxBody>| {
            let core = core.clone();
            let registry = registry.clone();
            let executor = executor.clone();
            // HTTP/1.1 admits one live response per connection; this invocation owns its busy guard.
            let busy = busy.take().expect("single request service invoked once");
            async move {
                let admission = match core.prepare(&request, executor) {
                    Ok(admission) => admission,
                    Err(status) => return Ok::<_, Infallible>(closing_status(status)),
                };
                let (parts, body) = request.into_parts();
                // The selected methods have exactly one request. Admit before buffering, retain
                // the same hard deadline, and read only framed bound+1 through EOF. This permits
                // deterministic limit+1 refusal without racing an HTTP/1 client's upload or
                // attempting an unlimited drain after an oversized prefix.
                let input = tokio::time::timeout_at(
                    admission.deadline,
                    Limited::new(wire::request_body(body), wire::MAX_MESSAGE_BYTES + 6).collect(),
                )
                .await;
                let bytes = match input {
                    Ok(Ok(body)) => body.to_bytes(),
                    Ok(Err(_)) => {
                        return Ok(admission.guard(
                            closing_status(Status::resource_exhausted(
                                "request exceeds framed bound+1 or is malformed",
                            )),
                            busy,
                        ))
                    }
                    Err(_) => {
                        return Ok(admission.guard(
                            closing_status(Status::deadline_exceeded(
                                "RPC request deadline exceeded",
                            )),
                            busy,
                        ))
                    }
                };
                if bytes.len() > wire::MAX_MESSAGE_BYTES + 5 {
                    return Ok(admission.guard(
                        status_in_body(
                            Status::resource_exhausted("request exceeds 4 MiB framed message")
                                .into_http(),
                        ),
                        busy,
                    ));
                }
                let request = Request::from_parts(
                    parts,
                    Full::new(bytes)
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                );
                let response = core
                    .request(request, connection, registry, &admission)
                    .await;
                Ok(admission.guard(status_in_body(response), busy))
            }
        },
    ));
    // The service is invoked exactly once. Using oneshot avoids a reusable closure that
    // would incorrectly attempt to clone the affine admission/activity guard.
    let mut response = match service.call(body).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    cors(&mut response, origin.as_deref());
    response
}

pub struct GrpcWebServer;
impl GrpcWebServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<std::net::SocketAddr> {
        let p = ctx.startup_params.as_ref();
        let schema = p
            .context("proto_schema required")?
            .get_string("proto_schema")?;
        let timeout = p
            .map(|p| p.get_optional_u64("rpc_timeout_secs"))
            .transpose()?
            .flatten()
            .unwrap_or(300);
        ensure!(
            (1..=3600).contains(&timeout),
            "rpc_timeout_secs must be 1..3600"
        );
        let allow_origin = p
            .map(|p| p.get_optional_string("allow_origin"))
            .transpose()?
            .flatten();
        if let Some(origin) = &allow_origin {
            ensure!(origin.len() <= 2048, "allow_origin exceeds 2048 bytes");
            let uri: hyper::Uri = origin.parse()?;
            ensure!(
                matches!(uri.scheme_str(), Some("http" | "https"))
                    && uri.host().is_some()
                    && uri.path() == "/"
                    && uri.query().is_none()
                    && !origin.contains('@')
                    && !origin.ends_with('/'),
                "allow_origin must be one exact HTTP(S) origin without a trailing slash"
            );
        }
        let pool = grpc::schema::load(&schema).await?;
        ensure!(
            pool.services().next().is_some(),
            "schema must define services"
        );
        let core = WebCore::new(
            &ctx,
            pool,
            Arc::new(actions::GrpcWebProtocol),
            Duration::from_secs(timeout),
        );
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(ctx.legacy_listen_addr())
                .await?;
        let address = listener.local_addr()?;
        let registrar = ctx.state.clone();
        let server_id = ctx.server_id;
        let task = tokio::spawn(async move {
            let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
            loop {
                let (stream, remote, permit) = match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    REFUSAL,
                    "gRPC-Web",
                    Some(&ctx.status_tx),
                )
                .await
                {
                    Ok(value) => value,
                    Err(error) => {
                        crate::console_error!(ctx.status_tx, "gRPC-Web accept failed: {error}");
                        break;
                    }
                };
                let id = crate::server::connection::ConnectionId::new(
                    ctx.state.get_next_unified_id().await,
                );
                let now = crate::utils::clock::Instant::now();
                ctx.state
                    .add_connection_to_server(
                        ctx.server_id,
                        ConnectionState {
                            id,
                            remote_addr: remote,
                            local_addr: address,
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
                let state = ctx.state.clone();
                let owner = state.clone();
                let core = core.clone();
                let origin = allow_origin.clone();
                let server = ctx.server_id;
                owner.spawn_server_task(server, async move {
                    let _permit = permit;
                    if matches!(tokio::time::timeout(FIRST_BYTE_TIMEOUT, stream.peek(&mut [0u8;1])).await, Ok(Ok(n)) if n > 0) {
                        let activity = Arc::new(crate::server::accept_bounded::ConnectionActivity::new());
                        let executor = OwnedExecutor::default(); let _children = ConnectionTasks(executor.clone());
                        let registry = Registry::default();
                        let commands = crate::server::peer_support::register_peer_channel(&state, server, id.as_u32()).await;
                        let _ = executor.spawn(core.clone().commands(commands, registry.clone(), id));
                        let execute = executor.clone(); let active = activity.clone();
                        let (done, ended) = tokio::sync::oneshot::channel();
                        // Tracking the HTTP/1 owner in the same executor lets the core's hard
                        // RPC timer cancel a body stalled behind a peer that never reads.
                        let _ = executor.spawn(async move {
                            let service = hyper::service::service_fn(move |request| {
                                let core = core.clone(); let activity = active.clone(); let registry = registry.clone();
                                let executor = execute.clone(); let origin = origin.clone();
                                async move { Ok::<_, Infallible>(self::request(request, core, id, activity, registry, executor, origin).await) }
                            });
                            let _ = hyper::server::conn::http1::Builder::new().max_headers(64).max_buf_size(32768)
                                .serve_connection(hyper_util::rt::TokioIo::new(stream), service).await;
                            let _ = done.send(());
                        });
                        tokio::select! { _ = ended => {}, _ = crate::server::accept_bounded::watch_idle(activity, IDLE_TIMEOUT) => {} }
                    }
                    state.remove_peer_handle(server, id.as_u32()).await;
                    state.remove_connection_from_server(server, id).await;
                }).await;
            }
        });
        registrar.register_server_task(server_id, task).await;
        Ok(address)
    }
}
