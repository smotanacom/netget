//! Generated unary OTLP services, sharing the HTTP receiver's semantic verdicts.
use super::{codec, RequestContext, MAX_BODY_BYTES};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response, Version};
use opentelemetry_proto::tonic::collector::{
    logs::v1 as logs, metrics::v1 as metrics, trace::v1 as traces,
};
use prost::Message;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tonic::{body::BoxBody, codec::CompressionEncoding, Code, Status};
use tower::Service;

pub(super) const MAX_EXPORTS: usize = 64;
const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);

// Hyper's HTTP/2 executor creates stream tasks. A connection owns and aborts every
// one, including a stream waiting on a manual handler, when its owner is removed.
#[derive(Clone, Default)]
pub(super) struct OwnedExecutor(Arc<Children>);
#[derive(Default)]
struct Children {
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    closed: AtomicBool,
}
pub(super) struct ConnectionTasks(pub OwnedExecutor);
impl Drop for ConnectionTasks {
    fn drop(&mut self) {
        let mut tasks = self.0 .0.tasks.lock().unwrap_or_else(|e| e.into_inner());
        self.0 .0.closed.store(true, Ordering::Relaxed);
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}
impl<F> hyper::rt::Executor<F> for OwnedExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let mut tasks = self.0.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if self.0.closed.load(Ordering::Relaxed) {
            return;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio::spawn(async move {
            let _ = future.await;
        }));
    }
}

fn deadline(req: &Request<Incoming>) -> Result<Duration, Status> {
    let Some(value) = req.headers().get("grpc-timeout") else {
        return Ok(EXPORT_TIMEOUT);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    if !(2..=9).contains(&value.len()) {
        return Err(Status::invalid_argument("invalid grpc-timeout"));
    }
    let (digits, unit) = value.split_at(value.len() - 1);
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Status::invalid_argument("invalid grpc-timeout"));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| Status::invalid_argument("invalid grpc-timeout"))?;
    let nanos = match unit {
        "H" => n.checked_mul(3_600_000_000_000),
        "M" => n.checked_mul(60_000_000_000),
        "S" => n.checked_mul(1_000_000_000),
        "m" => n.checked_mul(1_000_000),
        "u" => n.checked_mul(1000),
        "n" => Some(n),
        _ => None,
    }
    .ok_or_else(|| Status::invalid_argument("invalid grpc-timeout"))?;
    Ok(EXPORT_TIMEOUT.min(Duration::from_nanos(nanos)))
}
pub(super) async fn dispatch(req: Request<Incoming>, ctx: RequestContext) -> Response<BoxBody> {
    if req.version() != Version::HTTP_2 {
        return Status::invalid_argument("OTLP/gRPC requires HTTP/2").into_http();
    }
    let timeout = match deadline(&req) {
        Ok(value) => value,
        Err(status) => return status.into_http(),
    };
    let _permit = match ctx.exports.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return Status::unavailable("netget: receiver at capacity").into_http(),
    };
    let path = req.uri().path().to_owned();
    let compressed = req
        .headers()
        .get("grpc-encoding")
        .is_some_and(|v| v == "gzip");
    let receiver = Receiver { ctx, compressed };
    let call = async move {
        let req = match bounded_unary(req).await {
            Ok(req) => req,
            Err(status) => return Ok(status.into_http()),
        };
        match path.as_str() {
            "/opentelemetry.proto.collector.trace.v1.TraceService/Export" => {
                traces::trace_service_server::TraceServiceServer::new(receiver)
                    .accept_compressed(CompressionEncoding::Gzip)
                    .send_compressed(CompressionEncoding::Gzip)
                    .max_decoding_message_size(MAX_BODY_BYTES)
                    .max_encoding_message_size(MAX_BODY_BYTES)
                    .call(req)
                    .await
            }
            "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export" => {
                metrics::metrics_service_server::MetricsServiceServer::new(receiver)
                    .accept_compressed(CompressionEncoding::Gzip)
                    .send_compressed(CompressionEncoding::Gzip)
                    .max_decoding_message_size(MAX_BODY_BYTES)
                    .max_encoding_message_size(MAX_BODY_BYTES)
                    .call(req)
                    .await
            }
            "/opentelemetry.proto.collector.logs.v1.LogsService/Export" => {
                logs::logs_service_server::LogsServiceServer::new(receiver)
                    .accept_compressed(CompressionEncoding::Gzip)
                    .send_compressed(CompressionEncoding::Gzip)
                    .max_decoding_message_size(MAX_BODY_BYTES)
                    .max_encoding_message_size(MAX_BODY_BYTES)
                    .call(req)
                    .await
            }
            _ => Ok(Status::unimplemented("unknown OTLP service").into_http()),
        }
    };
    match tokio::time::timeout(timeout, call).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(never)) => match never {},
        Err(_) => Status::deadline_exceeded("netget: export deadline exceeded").into_http(),
    }
}

// Generated tonic unary services drain additional messages while reading
// trailers. OTLP permits one Export message. Bound its total wire body and
// reject a second frame before passing the single frame to tonic's decoder.
async fn bounded_unary(req: Request<Incoming>) -> Result<Request<Full<Bytes>>, Status> {
    let (parts, mut body) = req.into_parts();
    let mut buffer = Vec::new();
    let mut length = None;
    while let Some(frame) = body.frame().await {
        let frame =
            frame.map_err(|_| Status::invalid_argument("netget: unreadable export body"))?;
        if let Ok(data) = frame.into_data() {
            if buffer
                .len()
                .checked_add(data.len())
                .is_none_or(|n| n > MAX_BODY_BYTES + 5)
            {
                return Err(Status::resource_exhausted("netget: export body too large"));
            }
            buffer.extend_from_slice(&data);
            if length.is_none() && buffer.len() >= 5 {
                let declared = u32::from_be_bytes(buffer[1..5].try_into().unwrap()) as usize;
                if declared > MAX_BODY_BYTES {
                    return Err(Status::resource_exhausted(
                        "netget: export message too large",
                    ));
                }
                length = Some(declared + 5);
            }
            if length.is_some_and(|length| buffer.len() > length) {
                return Err(Status::invalid_argument(
                    "netget: unary Export requires one message",
                ));
            }
        }
    }
    if length != Some(buffer.len()) {
        return Err(Status::invalid_argument(
            "netget: incomplete export message",
        ));
    }
    Ok(Request::from_parts(parts, Full::new(Bytes::from(buffer))))
}

struct Receiver {
    ctx: RequestContext,
    compressed: bool,
}
impl Receiver {
    async fn export<T: Message, R: Message + Default>(
        &self,
        signal: codec::Signal,
        export: T,
    ) -> Result<tonic::Response<R>, Status> {
        let len = export.encoded_len();
        if len > MAX_BODY_BYTES {
            return Err(Status::resource_exhausted("netget: export too large"));
        }
        let body = export.encode_to_vec();
        drop(export);
        let summary = codec::summarize(signal, codec::Encoding::Protobuf, &body)
            .map_err(|_| Status::invalid_argument("netget: invalid OTLP export"))?;
        drop(body);
        self.ctx
            .app_state
            .update_connection_stats(
                self.ctx.server_id,
                self.ctx.connection_id,
                Some(len as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let reply = super::respond_to_export(
            self.ctx.clone(),
            signal,
            codec::Encoding::Protobuf,
            self.compressed,
            len,
            summary,
            "grpc",
        )
        .await;
        let (parts, body) = reply.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        if parts.status.is_success() {
            return R::decode(bytes)
                .map(tonic::Response::new)
                .map_err(|_| Status::internal("netget: response encoding failed"));
        }
        let failure = codec::RpcStatus::decode(bytes)
            .map_err(|_| Status::internal("netget: response encoding failed"))?;
        let code = Code::from_i32(failure.code);
        if let Some(secs) = parts
            .headers
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .and_then(|s| i64::try_from(s).ok())
        {
            let detail = RetryInfo {
                retry_delay: Some(prost_types::Duration {
                    seconds: secs,
                    nanos: 0,
                }),
            }
            .encode_to_vec();
            let rpc = DetailedStatus {
                code: failure.code,
                message: failure.message.clone(),
                details: vec![prost_types::Any {
                    type_url: "type.googleapis.com/google.rpc.RetryInfo".into(),
                    value: detail,
                }],
            };
            Err(Status::with_details(
                code,
                failure.message,
                Bytes::from(rpc.encode_to_vec()),
            ))
        } else {
            Err(Status::new(code, failure.message))
        }
    }
}
#[derive(Clone, PartialEq, Message)]
struct RetryInfo {
    #[prost(message, optional, tag = "1")]
    retry_delay: Option<prost_types::Duration>,
}
#[derive(Clone, PartialEq, Message)]
struct DetailedStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}
#[tonic::async_trait]
impl traces::trace_service_server::TraceService for Receiver {
    async fn export(
        &self,
        request: tonic::Request<traces::ExportTraceServiceRequest>,
    ) -> Result<tonic::Response<traces::ExportTraceServiceResponse>, Status> {
        self.export(codec::Signal::Traces, request.into_inner())
            .await
    }
}
#[tonic::async_trait]
impl metrics::metrics_service_server::MetricsService for Receiver {
    async fn export(
        &self,
        request: tonic::Request<metrics::ExportMetricsServiceRequest>,
    ) -> Result<tonic::Response<metrics::ExportMetricsServiceResponse>, Status> {
        self.export(codec::Signal::Metrics, request.into_inner())
            .await
    }
}
#[tonic::async_trait]
impl logs::logs_service_server::LogsService for Receiver {
    async fn export(
        &self,
        request: tonic::Request<logs::ExportLogsServiceRequest>,
    ) -> Result<tonic::Response<logs::ExportLogsServiceResponse>, Status> {
        self.export(codec::Signal::Logs, request.into_inner()).await
    }
}
