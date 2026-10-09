//! Native binary protobuf Connect over owned HTTP/1.1 connections.
pub mod actions;
pub mod wire;
use crate::server::grpc::{
    self,
    streaming::{ConnectionTasks, OwnedExecutor, Registry},
    HttpCore,
};
use crate::{
    protocol::SpawnContext,
    state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
};
use anyhow::{ensure, Context, Result};
use http_body_util::BodyExt;
use hyper::{body::Incoming, Method, Request, Response, StatusCode};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tonic::{body::BoxBody, Status};
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_CONNECTIONS: usize = 256;
pub const DEFAULT_RPC_TIMEOUT_SECS: u64 = 300;
const REFUSAL:&[u8]=b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: 5\r\nConnection: close\r\n\r\n";
fn refusal(code: StatusCode, status: Status) -> Response<BoxBody> {
    let mut response = wire::failure(status, wire::Shape::Unary, true);
    *response.status_mut() = code;
    response
}
async fn request(
    request: Request<Incoming>,
    core: HttpCore,
    connection: crate::server::connection::ConnectionId,
    activity: Arc<crate::server::accept_bounded::ConnectionActivity>,
    registry: Registry,
    executor: OwnedExecutor,
) -> Response<BoxBody> {
    if !wire::bounded_headers(request.headers()) {
        return refusal(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            Status::resource_exhausted("HTTP headers exceed bound"),
        );
    }
    if request.headers().contains_key("origin") {
        return refusal(
            StatusCode::FORBIDDEN,
            Status::permission_denied("browser origin outside native Connect scope"),
        );
    }
    if request.method() != Method::POST {
        return refusal(
            StatusCode::METHOD_NOT_ALLOWED,
            Status::unimplemented("Connect POST only; GET excluded"),
        );
    }
    if request.uri().path().len() > 2048 || request.uri().query().is_some() {
        return refusal(
            StatusCode::BAD_REQUEST,
            Status::invalid_argument("RPC path exceeds bound or has query"),
        );
    }
    let shape = match wire::shape(request.headers()) {
        Ok(shape) => shape,
        Err(status) => return refusal(StatusCode::UNSUPPORTED_MEDIA_TYPE, status),
    };
    let request_gzip = match wire::encoding(
        request.headers(),
        if shape == wire::Shape::Unary {
            "content-encoding"
        } else {
            "connect-content-encoding"
        },
    ) {
        Ok(value) => value,
        Err(status) => return wire::failure(status, shape, true),
    };
    let accept_gzip = match wire::accepts_gzip(
        request.headers(),
        if shape == wire::Shape::Unary {
            "accept-encoding"
        } else {
            "connect-accept-encoding"
        },
        request_gzip,
    ) {
        Ok(value) => value,
        Err(status) => return wire::failure(status, shape, true),
    };
    let timeout = match wire::timeout_ms(request.headers()) {
        Ok(value) => value,
        Err(status) => return wire::failure(status, shape, true),
    };
    if wire::unique(request.headers(), "connect-protocol-version")
        .ok()
        .flatten()
        != Some("1")
    {
        return refusal(
            StatusCode::BAD_REQUEST,
            Status::invalid_argument("unique connect-protocol-version: 1 required"),
        );
    }
    if request
        .headers()
        .keys()
        .any(|key| key.as_str().starts_with("grpc-") || key.as_str().ends_with("-bin"))
        || (shape == wire::Shape::Stream && request.headers().contains_key("content-encoding"))
        || (shape == wire::Shape::Unary
            && (request.headers().contains_key("connect-content-encoding")
                || request.headers().contains_key("connect-accept-encoding")))
    {
        return wire::failure(
            Status::invalid_argument("reserved or unsupported transport headers"),
            shape,
            true,
        );
    }
    let metadata = match wire::RpcMetadata::new(request.headers()) {
        Ok(value) => value,
        Err(status) => return wire::failure(status, shape, true),
    };
    let busy = activity.busy();
    let (mut parts, body) = request.into_parts();
    for name in [
        "content-type",
        "content-encoding",
        "accept-encoding",
        "connect-content-encoding",
        "connect-accept-encoding",
        "connect-protocol-version",
        "connect-timeout-ms",
    ] {
        parts.headers.remove(name);
    }
    parts.headers.insert(
        "content-type",
        http::HeaderValue::from_static("application/grpc"),
    );
    if request_gzip && shape == wire::Shape::Stream {
        parts
            .headers
            .insert("grpc-encoding", http::HeaderValue::from_static("gzip"));
    }
    if accept_gzip {
        parts.headers.insert(
            "grpc-accept-encoding",
            http::HeaderValue::from_static("gzip"),
        );
    }
    if let Some(timeout) = timeout {
        parts.headers.insert(
            "grpc-timeout",
            http::HeaderValue::from_str(&format!("{}m", timeout.min(3_600_000))).unwrap(),
        );
    }
    parts.extensions.insert(metadata.clone());
    let request = Request::from_parts(
        parts,
        body.map_err(|_| Status::unavailable("request transport failed"))
            .boxed_unsync(),
    );
    let admission = match core.prepare(&request, executor) {
        Ok(admission) => admission,
        Err(status) => return wire::failure(status, shape, true),
    };
    if admission.method.is_server_streaming() != (shape == wire::Shape::Stream) {
        return admission.guard(
            wire::failure(
                Status::invalid_argument("content type does not match method shape"),
                shape,
                true,
            ),
            busy,
        );
    }
    let maximum = wire::MAX_MESSAGE_BYTES + if shape == wire::Shape::Stream { 5 } else { 0 };
    let (parts, body) = request.into_parts();
    let bytes =
        match tokio::time::timeout_at(admission.deadline, wire::collect(body, maximum)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(status)) => return admission.guard(wire::failure(status, shape, true), busy),
            Err(_) => {
                return admission.guard(
                    wire::failure(
                        Status::deadline_exceeded("RPC upload deadline exceeded"),
                        shape,
                        true,
                    ),
                    busy,
                )
            }
        };
    let bytes = match if shape == wire::Shape::Unary {
        wire::unary_request(&bytes, request_gzip)
    } else {
        wire::streaming_request(&bytes, request_gzip)
    } {
        Ok(bytes) => bytes,
        Err(status) => return admission.guard(wire::failure(status, shape, false), busy),
    };
    let response = core
        .request(
            Request::from_parts(parts, wire::body(bytes)),
            connection,
            registry,
            &admission,
        )
        .await;
    let response = match tokio::time::timeout_at(
        admission.deadline,
        wire::server_response(response, shape, metadata),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(status)) => wire::failure(status, shape, true),
        Err(_) => wire::failure(
            Status::deadline_exceeded("RPC response deadline exceeded"),
            shape,
            true,
        ),
    };
    admission.guard(response, busy)
}
pub struct ConnectRpcServer;
impl ConnectRpcServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<std::net::SocketAddr> {
        let p = ctx.startup_params.as_ref();
        let schema = p
            .context("proto_schema required")?
            .get_string("proto_schema")?;
        let timeout = p
            .map(|p| p.get_optional_u64("rpc_timeout_secs"))
            .transpose()?
            .flatten()
            .unwrap_or(DEFAULT_RPC_TIMEOUT_SECS);
        ensure!(
            (1..=3600).contains(&timeout),
            "rpc_timeout_secs must be 1..3600"
        );
        let pool = grpc::schema::load(&schema).await?;
        ensure!(
            pool.services().next().is_some(),
            "schema must define services"
        );
        let core = HttpCore::new(
            &ctx,
            pool,
            Arc::new(actions::ConnectRpcProtocol),
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
                    "ConnectRPC",
                    Some(&ctx.status_tx),
                )
                .await
                {
                    Ok(value) => value,
                    Err(error) => {
                        crate::console_error!(ctx.status_tx, "ConnectRPC accept failed: {error}");
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
                            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({"transport":"http1","encoding":"proto","tls_verified":false})),
                        },
                    )
                    .await;
                let state = ctx.state.clone();
                let owner = state.clone();
                let core = core.clone();
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
                                let executor = execute.clone();
                                async move { Ok::<_, Infallible>(self::request(request, core, id, activity, registry, executor).await) }
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
