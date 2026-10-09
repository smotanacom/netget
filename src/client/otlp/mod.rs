//! Reusable OTLP connections with owned exports, response handlers and socket shutdown.
pub mod actions;
pub mod wire;
use crate::server::otlp::codec::{self, Encoding, Signal};
use crate::{
    client::{command_support, llm_budget::call_llm_for_client},
    llm::actions::client_trait::{Client, ClientActionResult},
    protocol::{ConnectContext, Event},
    state::{
        client_handles::{ClientCommand, ClientSendOutcome},
        AccessLogOwner, ClientStatus,
    },
};
use actions::{OtlpClientProtocol, CONNECTED, ERROR, RESULT};
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{client::conn::http1, Request};
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::{
    logs::v1 as logs, metrics::v1 as metrics, trace::v1 as traces,
};
use prost::Message;
use serde_json::{json, Value};
use std::{
    net::{Shutdown, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tonic::{
    codec::CompressionEncoding,
    transport::{Channel, ClientTlsConfig, Endpoint},
};
pub const DEFAULT_TRANSPORT: &str = "grpc";
pub const DEFAULT_TLS: bool = true;
pub const DEFAULT_GZIP: bool = false;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_OPERATIONS: usize = 16;
const MAX_DEPTH: u8 = 4;
const CONNECT_DEPTH: u8 = u8::MAX;
type Handler = BoxFuture<'static, (u8, Result<Vec<Value>>)>;
type Operation = BoxFuture<'static, (u8, Event)>;
trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Io for T {}
struct SocketGuard(std::net::TcpStream);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}
#[derive(Clone)]
enum Transport {
    Grpc(Channel),
    Http(Arc<tokio::sync::Mutex<http1::SendRequest<Full<Bytes>>>>),
}
#[derive(Clone)]
struct Session {
    transport: Transport,
    host: String,
    gzip: bool,
    deadline: Duration,
}
fn seconds(
    params: Option<&crate::protocol::StartupParams>,
    key: &str,
    default: Duration,
    max: u64,
) -> Result<Duration> {
    let n = params
        .map(|p| p.get_optional_u64(key))
        .transpose()?
        .flatten()
        .unwrap_or(default.as_secs());
    ensure!(n > 0 && n <= max, "{key} must be 1..{max}");
    Ok(Duration::from_secs(n))
}
pub struct OtlpClient;
impl OtlpClient {
    pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
        let p = ctx.startup_params.as_ref();
        let connect = seconds(p, "connect_timeout_secs", CONNECT_TIMEOUT, 60)?;
        let deadline = seconds(p, "export_timeout_secs", EXPORT_TIMEOUT, 300)?;
        let idle = seconds(p, "idle_timeout_secs", IDLE_TIMEOUT, 3600)?;
        let transport = p
            .map(|p| p.get_optional_string("transport"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| DEFAULT_TRANSPORT.into());
        ensure!(
            matches!(transport.as_str(), "grpc" | "http"),
            "transport must be grpc or http"
        );
        let tls = p
            .map(|p| p.get_optional_bool("tls"))
            .transpose()?
            .flatten()
            .unwrap_or(DEFAULT_TLS);
        let gzip = p
            .map(|p| p.get_optional_bool("gzip"))
            .transpose()?
            .flatten()
            .unwrap_or(DEFAULT_GZIP);
        let ca = p
            .map(|p| p.get_optional_string("ca_cert_path"))
            .transpose()?
            .flatten();
        let name = p
            .map(|p| p.get_optional_string("server_name"))
            .transpose()?
            .flatten();
        ensure!(
            tls || (ca.is_none() && name.is_none()),
            "TLS trust/name parameters require tls=true"
        );
        let uri: hyper::Uri = format!(
            "{}://{}",
            if tls { "https" } else { "http" },
            ctx.remote_addr
        )
        .parse()?;
        let authority = uri.authority().context("remote_addr must be host:port")?;
        ensure!(
            authority.port_u16().is_some()
                && authority.port_u16() != Some(0)
                && !authority.as_str().contains('@')
                && uri.path() == "/",
            "remote_addr must be host:port"
        );
        let name = name.unwrap_or_else(|| uri.host().unwrap().trim_matches(['[', ']']).to_owned());
        let pem = if let Some(path) = ca {
            Some(
                tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                    use std::io::Read;
                    ensure!(
                        std::fs::metadata(&path)?.is_file(),
                        "CA path must be a regular file"
                    );
                    let file = std::fs::File::open(path)?;
                    ensure!(file.metadata()?.is_file(), "CA path must be a regular file");
                    let mut data = Vec::new();
                    file.take(1048577).read_to_end(&mut data)?;
                    ensure!(
                        !data.is_empty() && data.len() <= 1048576,
                        "CA file must be 1..1048576 bytes"
                    );
                    Ok(data)
                })
                .await??,
            )
        } else {
            None
        };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (session, socket, local, remote, driver) = tokio::time::timeout(connect, async {
            let stream = tokio::net::TcpStream::connect(&ctx.remote_addr).await?;
            let local = stream.local_addr()?;
            let remote = stream.peer_addr()?;
            let raw = stream.into_std()?;
            let socket = SocketGuard(raw.try_clone()?);
            let stream = tokio::net::TcpStream::from_std(raw)?;
            let (transport, driver): (Transport, BoxFuture<'static, Result<(), hyper::Error>>) =
                if transport == "grpc" {
                    let mut endpoint = Endpoint::from(uri.clone())
                        .connect_timeout(connect)
                        .concurrency_limit(MAX_OPERATIONS)
                        .buffer_size(MAX_OPERATIONS)
                        .initial_stream_window_size(65536)
                        .initial_connection_window_size(1048576)
                        .http2_max_header_list_size(32768);
                    if tls {
                        let mut config = ClientTlsConfig::new()
                            .with_webpki_roots()
                            .domain_name(name.clone());
                        if let Some(pem) = pem.clone() {
                            config =
                                config.ca_certificate(tonic::transport::Certificate::from_pem(pem));
                        }
                        endpoint = endpoint.tls_config(config)?;
                    }
                    // Reconnection cannot create an unowned socket. The sole connector
                    // consumes the socket whose shutdown guard belongs to this client.
                    let mut stream = Some(stream);
                    let connector = tower::service_fn(move |_: hyper::Uri| {
                        let stream = stream.take();
                        async move {
                            stream.map(TokioIo::new).ok_or_else(|| {
                                std::io::Error::new(
                                    std::io::ErrorKind::NotConnected,
                                    "OTLP automatic reconnect is disabled",
                                )
                            })
                        }
                    });
                    (
                        Transport::Grpc(endpoint.connect_with_connector(connector).await?),
                        futures::future::pending().boxed(),
                    )
                } else {
                    let io: Box<dyn Io> = if tls {
                        let mut roots = rustls::RootCertStore::empty();
                        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                        if let Some(pem) = pem.clone() {
                            let certs = rustls_pemfile::certs(&mut pem.as_slice())
                                .collect::<std::result::Result<Vec<_>, _>>()?;
                            ensure!(!certs.is_empty(), "CA file contains no certificates");
                            for cert in certs {
                                roots.add(cert)?;
                            }
                        }
                        let mut config = rustls::ClientConfig::builder()
                            .with_root_certificates(roots)
                            .with_no_client_auth();
                        config.alpn_protocols = vec![b"http/1.1".to_vec()];
                        Box::new(
                            tokio_rustls::TlsConnector::from(Arc::new(config))
                                .connect(
                                    rustls::pki_types::ServerName::try_from(name.clone())?,
                                    stream,
                                )
                                .await?,
                        )
                    } else {
                        Box::new(stream)
                    };
                    let (sender, driver) = http1::Builder::new()
                        .max_headers(128)
                        .max_buf_size(32768)
                        .handshake(TokioIo::new(io))
                        .await?;
                    (
                        Transport::Http(Arc::new(tokio::sync::Mutex::new(sender))),
                        driver.boxed(),
                    )
                };
            Ok::<_, anyhow::Error>((
                Session {
                    transport,
                    host: ctx.remote_addr.clone(),
                    gzip,
                    deadline,
                },
                socket,
                local,
                remote,
                driver,
            ))
        })
        .await
        .context("OTLP connect deadline exceeded")??;
        let now = crate::utils::clock::Instant::now();
        ctx.state
            .with_client_mut(ctx.client_id, |client| {
                client.connection = Some(crate::state::ClientConnectionState {
                    id: ctx.client_id,
                    remote_addr: ctx.remote_addr.clone(),
                    connected_addr: Some(remote),
                    local_addr: Some(local),
                    bytes_sent: 0,
                    bytes_received: 0,
                    packets_sent: 0,
                    packets_received: 0,
                    last_activity: now,
                    status: ClientStatus::Connected,
                    status_changed_at: now,
                    protocol_info: crate::state::server::ProtocolConnectionInfo::new(
                        json!({"transport":transport,"tls_verified":tls}),
                    ),
                });
            })
            .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Connected)
            .await;
        let commands = command_support::register_command_channel(&ctx.state, ctx.client_id).await;
        let event = Event::new(
            &CONNECTED,
            json!({"remote_addr":ctx.remote_addr,"transport":transport,"tls_verified":tls,"server_name":if tls {Some(name)} else {None}}),
        );
        let state = ctx.state.clone();
        let id = ctx.client_id;
        state
            .spawn_client_task(
                id,
                Self::run(ctx, session, socket, commands, event, driver, idle),
            )
            .await;
        Ok(local)
    }
    #[allow(clippy::too_many_arguments)]
    async fn run(
        ctx: ConnectContext,
        session: Session,
        _socket: SocketGuard,
        mut commands: tokio::sync::mpsc::Receiver<ClientCommand>,
        event: Event,
        mut driver: BoxFuture<'static, Result<(), hyper::Error>>,
        idle: Duration,
    ) {
        let mut operations: FuturesUnordered<Operation> = FuturesUnordered::new();
        let mut handlers: FuturesUnordered<Handler> = FuturesUnordered::new();
        handlers.push(handler(ctx.clone(), event, CONNECT_DEPTH));
        let mut activity = tokio::time::Instant::now();
        loop {
            tokio::select! {
                _ = &mut driver => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    activity = tokio::time::Instant::now();
                    if enqueue(
                        &ctx, &session, &mut operations, handlers.len(),
                        command.action.clone(), Some(command), 0,
                    ).await {
                        break;
                    }
                }
                Some((depth, event)) = operations.next(), if !operations.is_empty() => {
                    activity = tokio::time::Instant::now();
                    handlers.push(handler(ctx.clone(), event, depth));
                }
                Some((depth, result)) = handlers.next(), if !handlers.is_empty() => {
                    activity = tokio::time::Instant::now();
                    match result {
                        Ok(actions) if actions.len() <= MAX_OPERATIONS => {
                            let mut stop = false;
                            for action in actions {
                                if depth != CONNECT_DEPTH && depth >= MAX_DEPTH
                                    && action["type"] != "disconnect"
                                {
                                    continue;
                                }
                                let next = if depth == CONNECT_DEPTH { 0 } else { depth + 1 };
                                if enqueue(
                                    &ctx, &session, &mut operations, handlers.len(), action, None, next,
                                ).await {
                                    stop = true;
                                    break;
                                }
                            }
                            if stop { break; }
                        }
                        Ok(_) => crate::logging::emit::Log::new(Some(&ctx.status_tx))
                            .warn("OTLP handler exceeded 16 actions"),
                        Err(e) => crate::logging::emit::Log::new(Some(&ctx.status_tx))
                            .warn(format!("OTLP handler failed: {e}")),
                    }
                }
                _ = tokio::time::sleep_until(activity + idle),
                    if operations.is_empty() && handlers.is_empty() => break,
            }
        }
        drop(operations);
        drop(handlers);
        drop(session);
        drop(driver);
        drop(_socket);
        ctx.state.remove_client_handle(ctx.client_id).await;
        ctx.state
            .with_client_mut(ctx.client_id, |client| {
                if let Some(c) = &mut client.connection {
                    c.status = ClientStatus::Disconnected;
                    c.status_changed_at = crate::utils::clock::Instant::now();
                }
            })
            .await;
        ctx.state
            .update_client_status(ctx.client_id, ClientStatus::Disconnected)
            .await;
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
}
fn handler(ctx: ConnectContext, event: Event, depth: u8) -> Handler {
    async move {
        let result = async {
            let instruction = ctx
                .state
                .get_instruction_for_client(ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = ctx
                .state
                .get_memory_for_client(ctx.client_id)
                .await
                .unwrap_or_default();
            let result = call_llm_for_client(
                &ctx.llm_client,
                &ctx.state,
                ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &OtlpClientProtocol,
                &ctx.status_tx,
            )
            .await?;
            if let Some(memory) = result.memory_updates {
                ctx.state.set_memory_for_client(ctx.client_id, memory).await;
            }
            Ok(result.actions)
        }
        .await;
        (depth, result)
    }
    .boxed()
}
async fn record(ctx: &ConnectContext, action: &Value, outcome: &Result<ClientSendOutcome>) {
    ctx.state
        .record_access_log(
            AccessLogOwner::Client(ctx.client_id.as_u32()),
            "OTLP",
            None,
            "injected_action",
            action.clone(),
            vec![match outcome {
                Ok(v) => serde_json::to_value(v).unwrap_or(Value::Null),
                Err(e) => json!({"error":e.to_string()}),
            }],
        )
        .await;
}
async fn enqueue(
    ctx: &ConnectContext,
    session: &Session,
    operations: &mut FuturesUnordered<Operation>,
    handlers: usize,
    action: Value,
    command: Option<ClientCommand>,
    depth: u8,
) -> bool {
    let outcome = match OtlpClientProtocol.execute_action(action.clone()) {
        Err(e) => Ok(ClientSendOutcome::Rejected {
            error: e.to_string(),
        }),
        Ok(ClientActionResult::Disconnect) => Ok(ClientSendOutcome::Disconnected),
        Ok(ClientActionResult::WaitForMore | ClientActionResult::NoAction) => {
            Ok(ClientSendOutcome::Executed {
                detail: "wait_for_more".into(),
            })
        }
        Ok(ClientActionResult::Custom { name, .. }) if name == "otlp_export" => {
            if operations.len() + handlers >= MAX_OPERATIONS {
                Err(anyhow::anyhow!("OTLP client busy: 16 exports or handlers"))
            } else {
                let ctx = ctx.clone();
                let session = session.clone();
                operations.push(
                    async move {
                        let result = tokio::time::timeout(session.deadline, export(&session, &action))
                            .await
                            .context("OTLP export deadline exceeded")
                            .and_then(|r| r);
                        let acknowledged = result.is_ok();
                        let (outcome, event) = match result {
                            Ok(data) => (
                                Ok(ClientSendOutcome::Executed {
                                    detail: format!(
                                        "OTLP {}: {}",
                                        data["signal"].as_str().unwrap_or(""),
                                        data["result"].as_str().unwrap_or(""),
                                    ),
                                }),
                                Event::new(&RESULT, data),
                            ),
                            Err(e) => (
                                Err(anyhow::anyhow!(e.to_string())),
                                Event::new(
                                    &ERROR,
                                    json!({"action_type":action["type"],"error":diagnostic(&e.to_string())}),
                                ),
                            ),
                        };
                        ctx.state
                            .with_client_mut(ctx.client_id, |client| {
                                if let Some(c) = &mut client.connection {
                                    c.last_activity = crate::utils::clock::Instant::now();
                                    if acknowledged {
                                        c.packets_sent += 1;
                                        c.packets_received += 1;
                                    }
                                }
                            })
                            .await;
                        record(&ctx, &action, &outcome).await;
                        if let Some(command) = command {
                            command_support::reply(command, outcome);
                        }
                        (depth, event)
                    }
                    .boxed(),
                );
                return false;
            }
        }
        Ok(_) => Ok(ClientSendOutcome::Rejected {
            error: "unsupported OTLP action".into(),
        }),
    };
    let stop = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
    record(ctx, &action, &outcome).await;
    if let Some(command) = command {
        command_support::reply(command, outcome);
    }
    stop
}
fn partial(signal: Signal, body: &[u8]) -> Result<(i64, String)> {
    Ok(match signal {
        Signal::Traces => traces::ExportTraceServiceResponse::decode(body)?
            .partial_success
            .map(|v| (v.rejected_spans, v.error_message)),
        Signal::Metrics => metrics::ExportMetricsServiceResponse::decode(body)?
            .partial_success
            .map(|v| (v.rejected_data_points, v.error_message)),
        Signal::Logs => logs::ExportLogsServiceResponse::decode(body)?
            .partial_success
            .map(|v| (v.rejected_log_records, v.error_message)),
    }
    .unwrap_or_default())
}
async fn export(session: &Session, action: &Value) -> Result<Value> {
    let export = wire::build(action)?;
    let signal = export.signal();
    let body = export.encode();
    let summary =
        codec::summarize(signal, Encoding::Protobuf, &body).map_err(anyhow::Error::msg)?;
    let mut data = json!({"signal":signal.as_str(),"items":summary.item_count,"service_name":action["service_name"],"retryable":false,"retry_after_secs":null,"rejected":0,"message":""});
    let reply = match &session.transport {
        Transport::Grpc(channel) => {
            data["transport"] = json!("grpc");
            macro_rules! call {
                ($client:ty,$message:expr) => {{
                    let mut client = <$client>::new(channel.clone())
                        .accept_compressed(CompressionEncoding::Gzip)
                        .max_decoding_message_size(codec::MAX_BODY_BYTES)
                        .max_encoding_message_size(wire::MAX_EXPORT_BYTES);
                    if session.gzip {
                        client = client.send_compressed(CompressionEncoding::Gzip);
                    }
                    let mut request = tonic::Request::new($message);
                    request.set_timeout(session.deadline);
                    client
                        .export(request)
                        .await
                        .map(|r| r.into_inner().encode_to_vec())
                }};
            }
            let result = match export {
                wire::Export::Traces(v) => {
                    call!(traces::trace_service_client::TraceServiceClient<Channel>, v)
                }
                wire::Export::Metrics(v) => call!(
                    metrics::metrics_service_client::MetricsServiceClient<Channel>,
                    v
                ),
                wire::Export::Logs(v) => {
                    call!(logs::logs_service_client::LogsServiceClient<Channel>, v)
                }
            };
            match result {
                Ok(body) => {
                    data["grpc_code"] = json!(0);
                    body
                }
                Err(status) => {
                    if std::error::Error::source(&status).is_some() {
                        return Err(anyhow::anyhow!(status));
                    }
                    data["grpc_code"] = json!(status.code() as i32);
                    data["result"] = json!("rejected");
                    data["message"] = json!(diagnostic(status.message()));
                    let retry = retry_info(status.details());
                    data["retry_after_secs"] = json!(retry);
                    data["retryable"] = json!(
                        matches!(
                            status.code(),
                            tonic::Code::Cancelled
                                | tonic::Code::Unavailable
                                | tonic::Code::DeadlineExceeded
                                | tonic::Code::Aborted
                                | tonic::Code::OutOfRange
                                | tonic::Code::DataLoss
                        ) || (status.code() == tonic::Code::ResourceExhausted && retry.is_some())
                    );
                    return Ok(data);
                }
            }
        }
        Transport::Http(sender) => {
            data["transport"] = json!("http");
            let mut sender = sender.lock().await;
            sender.ready().await?;
            let body = if session.gzip {
                use std::io::Write;
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&body)?;
                encoder.finish()?
            } else {
                body
            };
            let mut request = Request::builder()
                .method("POST")
                .uri(format!("/v1/{}", signal.as_str()))
                .header("host", &session.host)
                .header("content-type", "application/x-protobuf")
                .body(Full::new(Bytes::from(body)))?;
            if session.gzip {
                request.headers_mut().insert(
                    "content-encoding",
                    hyper::header::HeaderValue::from_static("gzip"),
                );
            }
            let reply = sender.send_request(request).await?;
            let (parts, body) = reply.into_parts();
            let body = http_body_util::Limited::new(body, codec::MAX_BODY_BYTES)
                .collect()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?
                .to_bytes();
            let body = if parts
                .headers
                .get("content-encoding")
                .is_some_and(|v| v == "gzip")
            {
                codec::gunzip_bounded(&body, codec::MAX_BODY_BYTES)
                    .map_err(|e| anyhow::anyhow!("invalid bounded gzip response: {e:?}"))?
            } else {
                body.to_vec()
            };
            data["http_status"] = json!(parts.status.as_u16());
            if parts.status.as_u16() != 200 {
                data["result"] = json!("rejected");
                data["retryable"] = json!(codec::retryable(parts.status.as_u16()));
                data["retry_after_secs"] = json!(parts
                    .headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok()));
                let message = codec::RpcStatus::decode(body.as_slice())
                    .map(|v| v.message)
                    .unwrap_or_else(|_| format!("HTTP {}", parts.status.as_u16()));
                data["message"] = json!(diagnostic(&message));
                return Ok(data);
            }
            ensure!(
                parts
                    .headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .and_then(Encoding::from_content_type)
                    == Some(Encoding::Protobuf),
                "OTLP response must be protobuf"
            );
            body
        }
    };
    let (rejected, message) = partial(signal, &reply)?;
    ensure!(
        rejected >= 0 && rejected as usize <= summary.item_count,
        "invalid partial-success count"
    );
    data["result"] = json!(if rejected > 0 || !message.is_empty() {
        "partial_success"
    } else {
        "accepted"
    });
    data["rejected"] = json!(rejected);
    data["message"] = json!(diagnostic(&message));
    Ok(data)
}
#[derive(Clone, PartialEq, Message)]
struct DetailedStatus {
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}
#[derive(Clone, PartialEq, Message)]
struct RetryInfo {
    #[prost(message, optional, tag = "1")]
    retry_delay: Option<prost_types::Duration>,
}
fn retry_info(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > 32768 {
        return None;
    }
    let status = DetailedStatus::decode(bytes).ok()?;
    for detail in status.details {
        if detail.type_url == "type.googleapis.com/google.rpc.RetryInfo" {
            let delay = RetryInfo::decode(detail.value.as_slice())
                .ok()?
                .retry_delay?;
            if delay.seconds >= 0 && (0..1_000_000_000).contains(&delay.nanos) {
                return (delay.seconds as u64).checked_add(u64::from(delay.nanos > 0));
            }
        }
    }
    None
}

fn diagnostic(text: &str) -> String {
    if text.len() > 512 {
        crate::utils::truncate_for_llm(text, 512 - crate::utils::truncate::TRUNCATION_MARKER.len())
    } else {
        text.to_owned()
    }
}
