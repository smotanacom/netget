//! gRPC server implementation with dynamic schema support
//!
//! Implements a gRPC server where the LLM provides protobuf schema definitions
//! and controls RPC request/response handling through JSON.

pub mod actions;

use anyhow::{bail, Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

#[cfg(feature = "grpc")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "grpc")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "grpc")]
use crate::llm::ActionResult;
#[cfg(feature = "grpc")]
use crate::logging::emit::Log;
#[cfg(feature = "grpc")]
use crate::protocol::Event;
#[cfg(feature = "grpc")]
use crate::server::grpc::actions::GRPC_UNARY_REQUEST_EVENT;
#[cfg(feature = "grpc")]
use crate::server::GrpcProtocol;
#[cfg(feature = "grpc")]
use crate::state::app_state::AppState;
use crate::{console_error, console_info};
#[cfg(feature = "grpc")]
use bytes::Bytes;
#[cfg(feature = "grpc")]
use http_body_util::{BodyExt, Full, Limited};
#[cfg(feature = "grpc")]
use hyper::{body::Incoming, header::HeaderValue, Request, Response, StatusCode};
#[cfg(feature = "grpc")]
use prost::Message;
#[cfg(feature = "grpc")]
use prost_reflect::{DescriptorPool, DynamicMessage, ReflectMessage};
#[cfg(feature = "grpc")]
use prost_types::FileDescriptorSet;
#[cfg(feature = "grpc")]
use serde_json::json;

/// Largest request body buffered, before gRPC framing. Matches gRPC's own default
/// `maxReceiveMessageLength` of 4 MiB.
#[cfg(feature = "grpc")]
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// gRPC status codes (`google.rpc.Code`) this server produces.
///
/// `grpc_error`'s documented `code` string maps onto these. It previously did not map onto
/// anything: the code was parsed, logged, and then folded into a `bail!` message, so every
/// error reached the client as `13 INTERNAL` with the real code smuggled into the text of
/// `grpc-message`.
#[cfg(feature = "grpc")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum GrpcStatus {
    Ok = 0,
    Cancelled = 1,
    Unknown = 2,
    InvalidArgument = 3,
    DeadlineExceeded = 4,
    NotFound = 5,
    AlreadyExists = 6,
    PermissionDenied = 7,
    ResourceExhausted = 8,
    FailedPrecondition = 9,
    Aborted = 10,
    OutOfRange = 11,
    Unimplemented = 12,
    Internal = 13,
    Unavailable = 14,
    DataLoss = 15,
    Unauthenticated = 16,
}

#[cfg(feature = "grpc")]
impl GrpcStatus {
    /// Parse the `code` field of a `grpc_error` action. Accepts the canonical spec spellings
    /// (`NOT_FOUND`), lowercase, and the bare integer. Anything unrecognized is `UNKNOWN`
    /// rather than a silent `INTERNAL`, so a typo is visible as a typo.
    fn parse(code: &str) -> Self {
        if let Ok(n) = code.trim().parse::<i32>() {
            return Self::from_i32(n);
        }
        match code.trim().to_ascii_uppercase().replace('-', "_").as_str() {
            "OK" => Self::Ok,
            "CANCELLED" | "CANCELED" => Self::Cancelled,
            "UNKNOWN" => Self::Unknown,
            "INVALID_ARGUMENT" => Self::InvalidArgument,
            "DEADLINE_EXCEEDED" => Self::DeadlineExceeded,
            "NOT_FOUND" => Self::NotFound,
            "ALREADY_EXISTS" => Self::AlreadyExists,
            "PERMISSION_DENIED" => Self::PermissionDenied,
            "RESOURCE_EXHAUSTED" => Self::ResourceExhausted,
            "FAILED_PRECONDITION" => Self::FailedPrecondition,
            "ABORTED" => Self::Aborted,
            "OUT_OF_RANGE" => Self::OutOfRange,
            "UNIMPLEMENTED" | "NOT_IMPLEMENTED" => Self::Unimplemented,
            "INTERNAL" => Self::Internal,
            "UNAVAILABLE" => Self::Unavailable,
            "DATA_LOSS" => Self::DataLoss,
            "UNAUTHENTICATED" => Self::Unauthenticated,
            _ => Self::Unknown,
        }
    }

    fn from_i32(n: i32) -> Self {
        match n {
            0 => Self::Ok,
            1 => Self::Cancelled,
            2 => Self::Unknown,
            3 => Self::InvalidArgument,
            4 => Self::DeadlineExceeded,
            5 => Self::NotFound,
            6 => Self::AlreadyExists,
            7 => Self::PermissionDenied,
            8 => Self::ResourceExhausted,
            9 => Self::FailedPrecondition,
            10 => Self::Aborted,
            11 => Self::OutOfRange,
            12 => Self::Unimplemented,
            14 => Self::Unavailable,
            15 => Self::DataLoss,
            16 => Self::Unauthenticated,
            _ => Self::Internal,
        }
    }
}

/// The gRPC status a failed handler call must be reported as.
///
/// `UNAVAILABLE` (14) when the failure was the LLM backend being saturated, `INTERNAL` (13)
/// otherwise. The distinction is what gRPC client retry policies key on: `UNAVAILABLE` is in
/// the default retryable set and is the code a `grpc-service-config` `retryableStatusCodes`
/// list names, while `INTERNAL` is explicitly *not* retryable. Reporting a transient
/// rate-limiter refusal as `INTERNAL` converts a backlog that clears in seconds into a hard
/// application error for every caller.
///
/// Both are non-zero and both travel through `grpc_error_response`, which sends an empty body,
/// so neither can be mistaken for the `grpc-status: 0` + encoded message a success carries.
///
/// `pub` so `tests/` can exercise the classification directly — the project forbids
/// `#[cfg(test)]` modules in `src/`, and driving the rate limiter to refusal through a spawned
/// binary needs bounds the E2E harness cannot set.
#[cfg(feature = "grpc")]
pub fn grpc_status_for_llm_failure(err: &anyhow::Error) -> GrpcStatus {
    if crate::llm::is_overload_error(err) {
        GrpcStatus::Unavailable
    } else {
        GrpcStatus::Internal
    }
}

/// Reduce a status message to the visible-ASCII subset `grpc-message` can carry.
///
/// `HeaderValue` accepts only visible ASCII, and these messages come from LLM output and
/// `anyhow` chains — netget's own LLM errors begin with a literal `✗`, and multi-line `anyhow`
/// context chains are routine. Substitution rather than rejection: a mangled character is a far
/// better outcome than a discarded explanation, which is what the static fallback in
/// `grpc_error_response` amounts to. Capped at 512 characters because a status message is a
/// diagnostic, not a payload, and everything is ASCII by then so the cut cannot split a
/// codepoint.
#[cfg(feature = "grpc")]
fn header_safe(message: &str) -> String {
    let mut out = String::with_capacity(message.len().min(512));
    let mut last_was_space = false;
    for c in message.chars() {
        let c = if (' '..='~').contains(&c) { c } else { ' ' };
        if c == ' ' {
            if last_was_space {
                continue;
            }
            last_was_space = true;
        } else {
            last_was_space = false;
        }
        if out.chars().count() >= 512 {
            break;
        }
        out.push(c);
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        "internal error".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A handler failure carrying the gRPC status it should be reported as.
#[cfg(feature = "grpc")]
struct GrpcFailure {
    status: GrpcStatus,
    message: String,
}

#[cfg(feature = "grpc")]
impl GrpcFailure {
    fn new(status: GrpcStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

#[cfg(feature = "grpc")]
impl From<anyhow::Error> for GrpcFailure {
    fn from(e: anyhow::Error) -> Self {
        // Classify here too, not only at the explicit call site: `?` on an anyhow error is
        // reachable from several places in this module and an overload must never be reported
        // as a non-retryable INTERNAL just because it took the implicit path.
        //
        // `grpc-message` is a client-visible header, so it carries the category and not the
        // error - see `crate::utils::wire_failure`. The error itself is logged at the call
        // sites that have a status channel.
        let status = grpc_status_for_llm_failure(&e);
        Self::new(status, crate::utils::WireFailure::classify(&e).text())
    }
}

#[cfg(feature = "grpc")]
impl From<serde_json::Error> for GrpcFailure {
    fn from(e: serde_json::Error) -> Self {
        // Logged in full; the peer gets the category only.
        debug!("gRPC JSON conversion failed: {e}");
        Self::new(GrpcStatus::Internal, "request could not be processed")
    }
}

#[cfg(feature = "grpc")]
impl From<prost::EncodeError> for GrpcFailure {
    fn from(e: prost::EncodeError) -> Self {
        debug!("gRPC response encoding failed: {e}");
        Self::new(GrpcStatus::Internal, "could not encode response")
    }
}

/// gRPC server with dynamic schema support
pub struct GrpcServer;

#[cfg(feature = "grpc")]
impl GrpcServer {
    /// Spawn gRPC server with LLM-provided schema and actions
    ///
    /// The LLM provides a protobuf schema definition (as a string) via startup_params.
    /// The server parses requests into JSON, sends to LLM, and encodes JSON responses back to protobuf.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract proto schema from startup params
        let proto_schema = startup_params
            .as_ref()
            .map(|p| p.get_string("proto_schema"))
            .transpose()?
            .context(
                "Missing 'proto_schema' in startup_params. LLM must provide protobuf definition.",
            )?;

        trace!("Proto schema:\n{}", proto_schema);
        Log::new(Some(&status_tx)).debug("Compiling protobuf schema for gRPC server");

        // Compile proto schema to FileDescriptorSet
        let file_descriptor_set = Self::compile_proto_schema(&proto_schema)
            .context("Failed to compile protobuf schema")?;

        // Build descriptor pool for dynamic message handling
        let mut fd_bytes = Vec::new();
        file_descriptor_set.encode(&mut fd_bytes)?;
        let descriptor_pool = DescriptorPool::decode(fd_bytes.as_slice())
            .context("Failed to create descriptor pool from FileDescriptorSet")?;

        let services = descriptor_pool.services().collect::<Vec<_>>();

        if services.is_empty() {
            bail!("No services found in protobuf schema. Schema must define at least one service.");
        }

        info!("gRPC server starting with {} service(s)", services.len());
        let log = Log::new(Some(&status_tx));
        for service in &services {
            log.info(format!(
                "gRPC service: {} ({} methods)",
                service.full_name(),
                service.methods().count()
            ));
        }

        // Create gRPC server with dynamic handler
        let protocol = Arc::new(GrpcProtocol::new());
        let descriptor_pool_arc = Arc::new(descriptor_pool.clone());

        // Server reflection is NOT served.
        //
        // This used to build a `tonic_reflection` service into a variable named
        // `_reflection_service`, drop it at the end of scope, and log "gRPC reflection
        // enabled". The router below has no route for
        // `/grpc.reflection.v1.ServerReflection/ServerReflectionInfo`, so reflection requests
        // fell through to "unknown service". `grpcurl` with no -proto/-protoset starts with
        // exactly that call, which is why it could never introspect this server. Reflection is
        // also server-streaming, which this unary-only server cannot do at all.
        //
        // The `enable_reflection` startup parameter has been removed with it; it only ever
        // changed a log line.
        warn!(
            "gRPC server reflection is not implemented; clients must be given the schema \
             out of band (grpcurl -proto / -protoset)"
        );
        Log::new(Some(&status_tx))
            .warn("gRPC reflection is not served; pass the schema to clients out of band");

        // Create dynamic gRPC service
        let dynamic_service = DynamicGrpcService {
            llm_client: llm_client.clone(),
            app_state: app_state.clone(),
            status_tx: status_tx.clone(),
            server_id,
            descriptor_pool: descriptor_pool_arc,
            protocol,
        };

        // Start HTTP/2 server for gRPC
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        console_info!(status_tx, "gRPC server listening on {}", actual_addr);

        // Spawn server loop
        let service = Arc::new(dynamic_service);
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id = crate::server::connection::ConnectionId::new(
                            service.app_state.get_next_unified_id().await,
                        );
                        debug!("gRPC connection {} from {}", connection_id, remote_addr);
                        Log::new(Some(&status_tx))
                            .debug(format!("gRPC connection from {}", remote_addr));

                        // Add connection to server state
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: actual_addr,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let service_clone = service.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();

                        // Spawn connection handler
                        tokio::spawn(async move {
                            let io = hyper_util::rt::TokioIo::new(stream);

                            // Create service function for this connection
                            let grpc_service = hyper::service::service_fn(move |req| {
                                let service = service_clone.clone();
                                let conn_id = connection_id;
                                async move { service.handle_grpc_request(req, conn_id).await }
                            });

                            // Serve HTTP/2 connection
                            if let Err(e) = hyper::server::conn::http2::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .serve_connection(io, grpc_service)
                            .await
                            {
                                Log::new(Some(&status_tx_clone))
                                    .debug(format!("gRPC connection error: {}", e));
                            }

                            // Clean up connection
                            app_state_clone
                                .remove_connection_from_server(server_id, connection_id)
                                .await;
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "Failed to accept gRPC connection: {}", e);
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(actual_addr)
    }

    /// Parse protobuf schema into FileDescriptorSet
    ///
    /// Supports multiple input formats:
    /// 1. Base64-encoded FileDescriptorSet (recommended - no protoc needed)
    /// 2. .proto file path (requires protoc in PATH)
    /// 3. .proto text content (requires protoc in PATH)
    fn compile_proto_schema(proto_schema: &str) -> Result<FileDescriptorSet> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        // Try base64 decode first (FileDescriptorSet encoded as base64)
        if let Ok(decoded) = STANDARD.decode(proto_schema.trim()) {
            match FileDescriptorSet::decode(decoded.as_slice()) {
                Ok(fds) => {
                    debug!(
                        "Loaded FileDescriptorSet from base64 ({} bytes)",
                        decoded.len()
                    );
                    return Ok(fds);
                }
                Err(e) => {
                    // Base64 decoded successfully but FileDescriptorSet decode failed
                    // This is likely the correct format but corrupted data
                    bail!(
                        "Successfully decoded base64 but failed to parse FileDescriptorSet: {}. \
                           The base64 string may be corrupted or not a valid FileDescriptorSet.",
                        e
                    );
                }
            }
        }

        // Check if it's a file path
        if proto_schema.ends_with(".proto") || proto_schema.ends_with(".pb") {
            return Self::load_proto_from_file(proto_schema);
        }

        // Assume it's .proto text and compile with protoc
        Self::compile_proto_text(proto_schema)
    }

    /// Load FileDescriptorSet from a .proto or .pb file
    fn load_proto_from_file(path: &str) -> Result<FileDescriptorSet> {
        use std::path::Path;
        let path = Path::new(path);

        if !path.exists() {
            bail!("Proto file not found: {}", path.display());
        }

        // If it's a .pb file (pre-compiled descriptor), load directly
        if path.extension().and_then(|e| e.to_str()) == Some("pb") {
            let bytes = std::fs::read(path)?;
            let fds = FileDescriptorSet::decode(bytes.as_slice())
                .context("Failed to decode .pb file as FileDescriptorSet")?;
            debug!(
                "Loaded FileDescriptorSet from {} ({} files)",
                path.display(),
                fds.file.len()
            );
            return Ok(fds);
        }

        // Otherwise, compile the .proto file with protoc
        Self::compile_proto_file(path)
    }

    /// Compile .proto file using protoc
    fn compile_proto_file(path: &std::path::Path) -> Result<FileDescriptorSet> {
        use std::process::Command;

        // Unique per invocation. A fixed name meant two gRPC servers starting concurrently
        // wrote and read back the same descriptor file and could load each other's schema.
        let output_path = std::env::temp_dir().join(format!(
            "netget_grpc_descriptor_{}.pb",
            uuid::Uuid::new_v4()
        ));

        // Get the directory containing the proto file for proto_path
        let proto_dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let filename = path.file_name().context("Invalid proto file path")?;

        // Run protoc to generate FileDescriptorSet
        let output = Command::new("protoc")
            .arg("--include_imports")
            .arg("--include_source_info")
            .arg(format!("--descriptor_set_out={}", output_path.display()))
            .arg(format!("--proto_path={}", proto_dir.display()))
            .arg(filename)
            .output()
            .context("Failed to execute protoc. Is protoc installed and in PATH?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("protoc failed: {}", stderr);
        }

        // Load the generated descriptor set
        let bytes = std::fs::read(&output_path)?;
        let _ = std::fs::remove_file(&output_path);
        let fds = FileDescriptorSet::decode(bytes.as_slice())
            .context("Failed to decode protoc output")?;

        debug!(
            "Compiled {} with protoc ({} files)",
            path.display(),
            fds.file.len()
        );
        Ok(fds)
    }

    /// Compile .proto text using protoc
    fn compile_proto_text(proto_text: &str) -> Result<FileDescriptorSet> {
        use std::io::Write;

        // Write to temporary file with unique name to avoid conflicts when running multiple servers in parallel
        let temp_dir = std::env::temp_dir();
        let unique_id = uuid::Uuid::new_v4();
        let proto_file = temp_dir.join(format!("netget_grpc_{}.proto", unique_id));

        {
            let mut file = std::fs::File::create(&proto_file)?;
            file.write_all(proto_text.as_bytes())?;
        }

        // Compile with protoc
        let result = Self::compile_proto_file(&proto_file);

        // Clean up temp file
        let _ = std::fs::remove_file(&proto_file);

        result
    }
}

/// Dynamic gRPC service that handles requests using LLM
#[cfg(feature = "grpc")]
struct DynamicGrpcService {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    descriptor_pool: Arc<DescriptorPool>,
    protocol: Arc<GrpcProtocol>,
}

#[cfg(feature = "grpc")]
impl DynamicGrpcService {
    /// Handle a gRPC HTTP/2 request
    async fn handle_grpc_request(
        &self,
        req: Request<Incoming>,
        connection_id: crate::server::connection::ConnectionId,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        // Extract service and method from path (format: /package.Service/Method)
        let path = req.uri().path();
        let (service_name, method_name) = match Self::parse_grpc_path(path) {
            Ok((svc, method)) => (svc, method),
            Err(e) => {
                debug!("Invalid gRPC path: {} - {}", path, e);
                return Ok(Self::grpc_error_response(
                    GrpcStatus::Unimplemented,
                    "path must be /package.Service/Method",
                ));
            }
        };

        Log::new(Some(&self.status_tx))
            .debug(format!("gRPC request: {}/{}", service_name, method_name));

        // Validate content-type
        if let Some(content_type) = req.headers().get("content-type") {
            if !content_type
                .to_str()
                .unwrap_or("")
                .starts_with("application/grpc")
            {
                return Ok(Self::grpc_error_response(
                    GrpcStatus::Internal,
                    "expected content-type application/grpc",
                ));
            }
        }

        // Read request body, capped. HTTP/2 flow control bounds the window, not the total, so
        // an unbounded collect() lets one client grow the process without limit.
        let body_bytes = match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
            .collect()
            .await
        {
            Ok(collected) => collected.to_bytes(),
            Err(e) => {
                debug!("Failed to read gRPC request body: {}", e);
                return Ok(Self::grpc_error_response(
                    GrpcStatus::ResourceExhausted,
                    &format!("request body rejected (limit {} bytes)", MAX_REQUEST_BYTES),
                ));
            }
        };

        // Decode gRPC frame (5-byte header: compression flag + 4-byte length + payload)
        let request_payload = match Self::decode_grpc_frame(&body_bytes) {
            Ok(payload) => payload,
            Err(e) => {
                debug!("Failed to decode gRPC frame: {}", e);
                // A set compression flag is UNIMPLEMENTED per the spec, not a generic error:
                // it tells the client to retry without compression.
                let status = if body_bytes.first() == Some(&1) {
                    GrpcStatus::Unimplemented
                } else {
                    GrpcStatus::Internal
                };
                return Ok(Self::grpc_error_response(status, "malformed gRPC frame"));
            }
        };

        trace!("gRPC request payload: {} bytes", request_payload.len());

        // Handle the unary request
        let response_payload = match self
            .handle_unary(&service_name, &method_name, request_payload, connection_id)
            .await
        {
            Ok(payload) => payload,
            Err(failure) => {
                // Was `debug!` on the file log but `[ERROR]` on the TUI - exactly the
                // level drift the Log facade exists to prevent. Reclassified to ERROR on
                // both sinks: this is a genuine failed gRPC call reaching the client.
                Log::new(Some(&self.status_tx))
                    .error(format!("gRPC handler error: {}", failure.message));
                return Ok(Self::grpc_error_response(failure.status, &failure.message));
            }
        };

        // Encode response with gRPC framing
        let response_frame = Self::encode_grpc_frame(&response_payload);

        // Build the HTTP/2 response. Header values are compile-time constants here, so unlike
        // the error path there is nothing that can fail to parse.
        let mut response = Response::new(Full::new(Bytes::from(response_frame)));
        *response.status_mut() = StatusCode::OK;
        let headers = response.headers_mut();
        headers.insert("content-type", HeaderValue::from_static("application/grpc"));
        headers.insert("grpc-status", HeaderValue::from_static("0"));

        Log::new(Some(&self.status_tx))
            .debug(format!("gRPC response: {} bytes", response_payload.len()));

        Ok(response)
    }

    /// Parse gRPC path into service and method names
    fn parse_grpc_path(path: &str) -> Result<(String, String)> {
        // Format: /package.Service/Method
        if !path.starts_with('/') {
            bail!("Path must start with /");
        }

        let parts: Vec<&str> = path[1..].split('/').collect();
        if parts.len() != 2 {
            bail!("Path must be /Service/Method");
        }

        Ok((parts[0].to_string(), parts[1].to_string()))
    }

    /// Decode gRPC frame from bytes
    /// Frame format: 1 byte compression flag + 4 bytes length (big-endian) + payload
    fn decode_grpc_frame(frame: &[u8]) -> Result<Vec<u8>> {
        if frame.len() < 5 {
            bail!("Frame too short (need at least 5 bytes)");
        }

        let compressed = frame[0];
        if compressed != 0 {
            bail!("Compression not supported");
        }

        let length = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;

        // Compare against the bytes remaining rather than computing `5 + length`. On a 32-bit
        // target that addition wraps: a length of 0xFFFFFFFF gives `5 + length == 4`, the
        // guard passes, and `frame[5..4]` panics with start > end.
        let available = frame.len() - 5;
        if length > available {
            bail!(
                "frame declares {} bytes but only {} follow the header",
                length,
                available
            );
        }

        Ok(frame[5..5 + length].to_vec())
    }

    /// Encode payload into gRPC frame
    fn encode_grpc_frame(payload: &[u8]) -> Vec<u8> {
        let length = payload.len() as u32;
        let mut frame = Vec::with_capacity(5 + payload.len());

        // Compression flag (0 = not compressed)
        frame.push(0);

        // Length (4 bytes, big-endian)
        frame.extend_from_slice(&length.to_be_bytes());

        // Payload
        frame.extend_from_slice(payload);

        frame
    }

    /// Create a gRPC error reply.
    ///
    /// Two things this must get right, both of which it used to get wrong:
    ///
    /// * **HTTP 200.** gRPC carries application failures in `grpc-status`, not in the HTTP
    ///   status line. A non-200 makes a conformant client discard `grpc-message` and
    ///   synthesize `UNAVAILABLE`/`UNKNOWN`, so the code and text chosen here never reach the
    ///   caller. The old signature took a `StatusCode` and returned 500/404/400/415.
    /// * **No `unwrap()` on the message.** `message` comes from LLM output and `anyhow`
    ///   chains. `HeaderValue` accepts only visible ASCII, so a single non-ASCII character or
    ///   newline in a model's error text made `Builder::body` return `Err` and panicked the
    ///   connection task — skipping the connection cleanup that runs after `serve_connection`
    ///   and leaving the entry permanently `Active`.
    ///
    /// The message is now put through [`header_safe`] first, so the caller keeps the
    /// explanation instead of the static fallback. That fallback had become the *common* case
    /// rather than the last resort: netget's own LLM error strings begin with a literal `✗`, so
    /// every backend failure reached the client with its reason replaced by "internal error".
    fn grpc_error_response(status: GrpcStatus, message: &str) -> Response<Full<Bytes>> {
        let message = &header_safe(message);
        let mut res = Response::new(Full::new(Bytes::new()));
        *res.status_mut() = StatusCode::OK;
        let headers = res.headers_mut();
        headers.insert("content-type", HeaderValue::from_static("application/grpc"));
        headers.insert(
            "grpc-status",
            HeaderValue::from_str(&(status as i32).to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("13")),
        );
        headers.insert(
            "grpc-message",
            HeaderValue::from_str(message)
                .unwrap_or_else(|_| HeaderValue::from_static("internal error")),
        );
        res
    }

    /// Handle a gRPC unary request
    async fn handle_unary(
        &self,
        service_name: &str,
        method_name: &str,
        request_bytes: Vec<u8>,
        connection_id: crate::server::connection::ConnectionId,
    ) -> std::result::Result<Vec<u8>, GrpcFailure> {
        // Find service and method descriptors. An unknown service or method is UNIMPLEMENTED,
        // which is what a client expects and what `grpcurl` prints usefully; it used to be
        // reported as INTERNAL over HTTP 500.
        let service_desc = self
            .descriptor_pool
            .services()
            .find(|s| s.full_name() == service_name)
            .ok_or_else(|| {
                GrpcFailure::new(
                    GrpcStatus::Unimplemented,
                    format!("unknown service {}", service_name),
                )
            })?;

        let method_desc = service_desc
            .methods()
            .find(|m| m.name() == method_name)
            .ok_or_else(|| {
                GrpcFailure::new(
                    GrpcStatus::Unimplemented,
                    format!("unknown method {}/{}", service_name, method_name),
                )
            })?;

        let input_desc = method_desc.input();
        let output_desc = method_desc.output();

        Log::new(Some(&self.status_tx))
            .debug(format!("gRPC unary call: {}/{}", service_name, method_name));

        // Decode request using dynamic message. A body this server cannot decode is the
        // client's malformed input, not a server fault.
        let request_msg = DynamicMessage::decode(input_desc.clone(), request_bytes.as_slice())
            .map_err(|e| {
                debug!("gRPC request message did not decode: {e}");
                GrpcFailure::new(
                    GrpcStatus::InvalidArgument,
                    "could not decode request message",
                )
            })?;

        // Convert DynamicMessage to JSON using prost-reflect's JSON serialization
        let request_json = Self::dynamic_message_to_json(&request_msg)?;

        Log::new(Some(&self.status_tx)).trace(format!(
            "gRPC request JSON: {}",
            serde_json::to_string_pretty(&request_json)?
        ));

        // Build response schema description for LLM
        let response_schema = Self::build_message_schema(&output_desc);

        // Create event for LLM
        let event = Event::new(
            &GRPC_UNARY_REQUEST_EVENT,
            json!({
                "service": service_name,
                "method": method_name,
                "request": request_json,
                "expected_response_schema": response_schema,
            }),
        );

        // Call LLM.
        //
        // A failure here is answered, never swallowed: the caller gets a non-zero grpc-status
        // with an empty body, which no client can confuse with the encoded response message a
        // success carries. `UNAVAILABLE` when the backend was merely saturated so a retry
        // policy can act on it, `INTERNAL` otherwise.
        let execution_result = call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        .map_err(|e| {
            let status = grpc_status_for_llm_failure(&e);
            console_error!(
                self.status_tx,
                "gRPC {}/{} could not be answered: the handler failed ({}). Replying \
                 grpc-status {} ({:?}); no response message is being invented.",
                service_name,
                method_name,
                e,
                status as i32,
                status
            );
            GrpcFailure::new(
                status,
                crate::utils::WireFailure::classify(&e).prefixed_text(),
            )
        })?;

        // Process action results
        for protocol_result in execution_result.protocol_results {
            match protocol_result {
                ActionResult::Custom { name, data } if name == "grpc_unary_response" => {
                    // Extract response message from LLM
                    let response_json = data
                        .get("message")
                        .context("Missing 'message' in grpc_unary_response")?;

                    // Convert JSON to DynamicMessage
                    let response_msg = Self::json_to_dynamic_message(response_json, &output_desc)
                        .map_err(|e| {
                        debug!("gRPC handler response did not fit the schema: {e}");
                        GrpcFailure::new(
                            GrpcStatus::Internal,
                            "handler's response does not fit the schema",
                        )
                    })?;

                    // Encode to protobuf bytes
                    let mut response_bytes = Vec::new();
                    response_msg
                        .encode(&mut response_bytes)
                        .map_err(anyhow::Error::from)?;

                    Log::new(Some(&self.status_tx))
                        .debug(format!("gRPC response: {} bytes", response_bytes.len()));

                    return Ok(response_bytes);
                }
                ActionResult::Custom { name, data } if name == "grpc_error" => {
                    // The documented `code` now reaches the wire. It used to be parsed,
                    // logged and then folded into a bail! message, so every error was sent
                    // as 13 INTERNAL over HTTP 500 with the real code as text inside
                    // grpc-message.
                    let code = data
                        .get("code")
                        .and_then(|c| c.as_str())
                        .unwrap_or("INTERNAL");
                    let message = data
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Internal error")
                        .to_string();

                    // `GrpcStatus::parse` maps "OK" (and "0") to GrpcStatus::Ok, so a model
                    // answering the *error* action with code "OK" produced grpc-status 0 — a
                    // successful RPC with an empty body, carrying the error text in
                    // grpc-message where no client looks. An action whose entire purpose is to
                    // fail cannot be allowed to report success; that is the same confusion
                    // between a refusal and an approval the root CLAUDE.md warns about, just
                    // arriving via contradictory model output rather than via silence.
                    let parsed = GrpcStatus::parse(code);
                    let status = if parsed == GrpcStatus::Ok {
                        warn!(
                            "gRPC grpc_error asked for status OK; sending UNKNOWN instead \
                             (decision=model_reject code_coerced): {}",
                            message
                        );
                        GrpcStatus::Unknown
                    } else {
                        parsed
                    };
                    debug!("gRPC error: {} ({:?}) - {}", code, status, message);
                    return Err(GrpcFailure::new(status, message));
                }
                _ => {
                    // Ignore other action results
                }
            }
        }

        // The handler ran but produced no usable action — neither `grpc_unary_response` nor
        // `grpc_error`. This is the fail-open trap: returning an empty message with
        // `grpc-status: 0` here is indistinguishable, to the caller, from a handler that
        // *deliberately* returned an empty response (`grpc_unary_response` with `{}`). A model
        // that never answered would then look like a successful empty reply.
        //
        // Answer it as a failure instead. `INTERNAL` (13), not `UNAVAILABLE`: the backend was
        // reachable and did respond — the response was simply unusable — so this is a genuine
        // application error, not a transient condition a retry policy should hammer. The
        // deliberate-empty case stays structurally distinct: it travels through
        // `grpc_unary_response` above and reaches the wire as `grpc-status: 0`.
        console_error!(
            self.status_tx,
            "gRPC {}/{} could not be answered: the handler returned no usable action \
             (no grpc_unary_response, no grpc_error). Replying grpc-status 13 (INTERNAL); no \
             empty response is being invented.",
            service_name,
            method_name
        );
        Err(GrpcFailure::new(
            GrpcStatus::Internal,
            format!(
                "netget gRPC: handler produced no usable response for {}/{}",
                service_name, method_name
            ),
        ))
    }

    /// Convert DynamicMessage to JSON
    fn dynamic_message_to_json(msg: &DynamicMessage) -> Result<serde_json::Value> {
        // For now, create a basic JSON representation by iterating fields
        // TODO: Use proper protobuf JSON serialization when available
        let desc = msg.descriptor();
        let mut map = serde_json::Map::new();

        for field in desc.fields() {
            // get_field returns Cow<Value>, check if field has a value first
            if msg.has_field(&field) {
                let value = msg.get_field(&field);
                // Convert protobuf Value to JSON
                let json_value = Self::proto_value_to_json(&value)?;
                map.insert(field.name().to_string(), json_value);
            }
        }

        Ok(serde_json::Value::Object(map))
    }

    /// Convert protobuf Value to JSON
    fn proto_value_to_json(value: &prost_reflect::Value) -> Result<serde_json::Value> {
        use prost_reflect::Value;

        Ok(match value {
            Value::Bool(b) => json!(*b),
            Value::I32(i) => json!(*i),
            Value::I64(i) => json!(*i),
            Value::U32(u) => json!(*u),
            Value::U64(u) => json!(*u),
            Value::F32(f) => json!(*f),
            Value::F64(f) => json!(*f),
            Value::String(s) => json!(s),
            Value::Bytes(b) => {
                use base64::engine::general_purpose::STANDARD;
                use base64::Engine;
                json!(STANDARD.encode(b))
            }
            Value::EnumNumber(e) => json!(*e),
            Value::Message(m) => Self::dynamic_message_to_json(m)?,
            Value::List(l) => {
                let items: Result<Vec<_>> =
                    l.iter().map(|v| Self::proto_value_to_json(v)).collect();
                json!(items?)
            }
            Value::Map(m) => {
                let mut map = serde_json::Map::new();
                for (k, v) in m.iter() {
                    let key = Self::map_key_to_string(k)?;
                    let value = Self::proto_value_to_json(v)?;
                    map.insert(key, value);
                }
                json!(map)
            }
        })
    }

    /// Convert map key to string
    fn map_key_to_string(key: &prost_reflect::MapKey) -> Result<String> {
        use prost_reflect::MapKey;

        Ok(match key {
            MapKey::Bool(b) => b.to_string(),
            MapKey::I32(i) => i.to_string(),
            MapKey::I64(i) => i.to_string(),
            MapKey::U32(u) => u.to_string(),
            MapKey::U64(u) => u.to_string(),
            MapKey::String(s) => s.clone(),
        })
    }

    /// Convert JSON to DynamicMessage
    fn json_to_dynamic_message(
        json: &serde_json::Value,
        message_desc: &prost_reflect::MessageDescriptor,
    ) -> Result<DynamicMessage> {
        // Create a new dynamic message
        let mut msg = DynamicMessage::new(message_desc.clone());

        // Populate fields from JSON
        if let Some(obj) = json.as_object() {
            for (field_name, value) in obj {
                match message_desc.get_field_by_name(field_name) {
                    Some(field) => {
                        let proto_value = Self::json_to_field_value(value, &field)?;
                        msg.set_field(&field, proto_value);
                    }
                    // Silently dropping an unknown key made a hallucinated field name look
                    // like success: the response encoded without it and the client saw a
                    // default value with no indication anything was wrong.
                    None => {
                        warn!(
                            "handler returned field '{}' which is not in message {}; ignoring",
                            field_name,
                            message_desc.full_name()
                        );
                    }
                }
            }
        }

        Ok(msg)
    }

    /// Convert a JSON value to the protobuf value for a field, honoring its cardinality.
    ///
    /// `json_to_proto_value` looks only at `field.kind()`, which is the type of a single
    /// element. For a `repeated string` that is `Kind::String`, so a handler returning
    /// `{"tags": ["a", "b"]}` used to fail with `Expected string` and the whole RPC came back
    /// as an error — repeated and map fields could not be produced by a handler at all,
    /// despite `build_message_schema` telling it the cardinality was `repeated`.
    fn json_to_field_value(
        json: &serde_json::Value,
        field: &prost_reflect::FieldDescriptor,
    ) -> Result<prost_reflect::Value> {
        use prost_reflect::{MapKey, Value};

        // Maps are checked first: a protobuf map field is also "repeated" (of its synthetic
        // entry message), so testing is_list() first would take the wrong branch.
        if field.is_map() {
            let entry = match field.kind() {
                prost_reflect::Kind::Message(m) => m,
                _ => bail!("map field {} has no entry message", field.name()),
            };
            let key_field = entry.get_field(1).context("map entry has no key field")?;
            let value_field = entry.get_field(2).context("map entry has no value field")?;

            let obj = json.as_object().with_context(|| {
                format!("field {} is a map; expected a JSON object", field.name())
            })?;

            let mut map = std::collections::HashMap::new();
            for (k, v) in obj {
                let key = match key_field.kind() {
                    prost_reflect::Kind::String => MapKey::String(k.clone()),
                    prost_reflect::Kind::Bool => MapKey::Bool(
                        k.parse()
                            .with_context(|| format!("map key '{}' is not a boolean", k))?,
                    ),
                    prost_reflect::Kind::Int32
                    | prost_reflect::Kind::Sint32
                    | prost_reflect::Kind::Sfixed32 => MapKey::I32(
                        k.parse()
                            .with_context(|| format!("map key '{}' is not an int32", k))?,
                    ),
                    prost_reflect::Kind::Int64
                    | prost_reflect::Kind::Sint64
                    | prost_reflect::Kind::Sfixed64 => MapKey::I64(
                        k.parse()
                            .with_context(|| format!("map key '{}' is not an int64", k))?,
                    ),
                    prost_reflect::Kind::Uint32 | prost_reflect::Kind::Fixed32 => MapKey::U32(
                        k.parse()
                            .with_context(|| format!("map key '{}' is not a uint32", k))?,
                    ),
                    prost_reflect::Kind::Uint64 | prost_reflect::Kind::Fixed64 => MapKey::U64(
                        k.parse()
                            .with_context(|| format!("map key '{}' is not a uint64", k))?,
                    ),
                    other => bail!("unsupported protobuf map key type: {:?}", other),
                };
                map.insert(key, Self::json_to_proto_value(v, &value_field)?);
            }
            return Ok(Value::Map(map));
        }

        if field.is_list() {
            let arr = json.as_array().with_context(|| {
                format!("field {} is repeated; expected a JSON array", field.name())
            })?;
            let mut list = Vec::with_capacity(arr.len());
            for item in arr {
                list.push(Self::json_to_proto_value(item, field)?);
            }
            return Ok(Value::List(list));
        }

        Self::json_to_proto_value(json, field)
    }

    /// Convert a single JSON value to a protobuf Value of the field's element type.
    fn json_to_proto_value(
        json: &serde_json::Value,
        field: &prost_reflect::FieldDescriptor,
    ) -> Result<prost_reflect::Value> {
        use prost_reflect::{Kind, Value};

        Ok(match field.kind() {
            Kind::Bool => Value::Bool(json.as_bool().context("Expected boolean")?),
            // Range is checked rather than truncated with `as`: silently wrapping an
            // out-of-range value puts a different number on the wire than the handler asked
            // for, and the client has no way to tell.
            Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
                let n = json.as_i64().context("Expected integer")?;
                Value::I32(
                    i32::try_from(n).with_context(|| format!("{} does not fit in an int32", n))?,
                )
            }
            Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => {
                Value::I64(json.as_i64().context("Expected integer")?)
            }
            Kind::Uint32 | Kind::Fixed32 => {
                let n = json.as_u64().context("Expected unsigned integer")?;
                Value::U32(
                    u32::try_from(n).with_context(|| format!("{} does not fit in a uint32", n))?,
                )
            }
            Kind::Uint64 | Kind::Fixed64 => {
                Value::U64(json.as_u64().context("Expected unsigned integer")?)
            }
            Kind::Float => Value::F32(json.as_f64().context("Expected number")? as f32),
            Kind::Double => Value::F64(json.as_f64().context("Expected number")?),
            Kind::String => Value::String(json.as_str().context("Expected string")?.to_string()),
            Kind::Bytes => {
                use base64::engine::general_purpose::STANDARD;
                use base64::Engine;
                let s = json.as_str().context("Expected base64 string")?;
                let bytes = STANDARD.decode(s).context("Invalid base64")?;
                Value::Bytes(bytes.into())
            }
            Kind::Message(msg_desc) => {
                let msg = Self::json_to_dynamic_message(json, &msg_desc)?;
                Value::Message(msg)
            }
            Kind::Enum(enum_desc) => {
                if let Some(n) = json.as_i64() {
                    // Validate the number against the enum, matching what the string branch
                    // already did. `n as i32` accepted any integer and wrapped out-of-range
                    // ones into a valid-looking but wrong variant.
                    let n = i32::try_from(n)
                        .with_context(|| format!("{} is not a valid enum number", n))?;
                    if enum_desc.get_value(n).is_none() {
                        bail!("{} is not a value of enum {}", n, enum_desc.full_name());
                    }
                    Value::EnumNumber(n)
                } else if let Some(s) = json.as_str() {
                    // Try to find enum value by name
                    if let Some(val) = enum_desc.get_value_by_name(s) {
                        Value::EnumNumber(val.number())
                    } else {
                        bail!("Unknown enum value: {}", s);
                    }
                } else {
                    bail!("Expected enum number or string");
                }
            }
        })
    }

    /// Build a JSON schema description of a message type
    fn build_message_schema(message_desc: &prost_reflect::MessageDescriptor) -> serde_json::Value {
        let mut fields = serde_json::Map::new();

        for field in message_desc.fields() {
            let field_type = match field.kind() {
                prost_reflect::Kind::Double => "number (double)",
                prost_reflect::Kind::Float => "number (float)",
                prost_reflect::Kind::Int32
                | prost_reflect::Kind::Sint32
                | prost_reflect::Kind::Sfixed32 => "int32",
                prost_reflect::Kind::Int64
                | prost_reflect::Kind::Sint64
                | prost_reflect::Kind::Sfixed64 => "int64",
                prost_reflect::Kind::Uint32 | prost_reflect::Kind::Fixed32 => "uint32",
                prost_reflect::Kind::Uint64 | prost_reflect::Kind::Fixed64 => "uint64",
                prost_reflect::Kind::Bool => "boolean",
                prost_reflect::Kind::String => "string",
                prost_reflect::Kind::Bytes => "bytes (base64)",
                prost_reflect::Kind::Message(_) => "object",
                prost_reflect::Kind::Enum(_) => "enum (string)",
            };

            let cardinality = match field.cardinality() {
                prost_reflect::Cardinality::Optional => "optional",
                prost_reflect::Cardinality::Required => "required",
                prost_reflect::Cardinality::Repeated => "repeated",
            };

            fields.insert(
                field.name().to_string(),
                json!({
                    "type": field_type,
                    "cardinality": cardinality,
                }),
            );
        }

        json!({
            "type": "object",
            "fields": fields,
        })
    }
}
