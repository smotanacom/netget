//! etcd v3 server implementation with gRPC KV service
//!
//! Implements etcd v3 KV service where the LLM controls all key-value operations
//! through actions. Uses pre-compiled protobuf definitions from build.rs.

pub mod actions;

// Re-export protocol for external use
pub use actions::EtcdProtocol;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, trace};

#[cfg(feature = "etcd")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "etcd")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "etcd")]
use crate::protocol::Event;
#[cfg(feature = "etcd")]
use crate::server::etcd::actions::ETCD_RANGE_REQUEST_EVENT;
#[cfg(feature = "etcd")]
use crate::state::app_state::AppState;
#[cfg(feature = "etcd")]
use bytes::Bytes;
#[cfg(feature = "etcd")]
use http_body_util::{BodyExt, Full, Limited};
#[cfg(feature = "etcd")]
use hyper::{body::Incoming, header::HeaderValue, Request, Response, StatusCode};
#[cfg(feature = "etcd")]
use prost::Message;

// Include generated protobuf code
#[cfg(feature = "etcd")]
mod etcdserverpb {
    include!(concat!(env!("OUT_DIR"), "/etcdserverpb.rs"));
}
#[cfg(feature = "etcd")]
mod mvccpb {
    include!(concat!(env!("OUT_DIR"), "/mvccpb.rs"));
}

#[cfg(feature = "etcd")]
use crate::logging::emit::Log;
#[cfg(feature = "etcd")]
use etcdserverpb::{
    CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest,
    PutResponse, RangeRequest, RangeResponse, ResponseHeader, TxnRequest, TxnResponse,
};
#[cfg(feature = "etcd")]
use mvccpb::KeyValue;

/// Cluster identity plus the monotonic revision counter that every etcd response header
/// carries.
///
/// Deliberately **not** a key-value store: per the no-storage rule this protocol keeps no
/// keys, and the handler answers every Range/Put/Delete. A `kvs: HashMap<Vec<u8>, KeyValue>`
/// field used to sit here behind `#[allow(dead_code)]` and was never read or written; it is
/// gone rather than left as a half-built store.
#[cfg(feature = "etcd")]
struct EtcdMeta {
    /// Current revision counter
    revision: i64,
    /// Cluster ID
    cluster_id: u64,
    /// Member ID
    member_id: u64,
}

#[cfg(feature = "etcd")]
impl EtcdMeta {
    fn new(cluster_id: u64, member_id: u64) -> Self {
        Self {
            revision: 0,
            cluster_id,
            member_id,
        }
    }

    fn get_response_header(&self) -> ResponseHeader {
        ResponseHeader {
            cluster_id: self.cluster_id,
            member_id: self.member_id,
            revision: self.revision,
            raft_term: 1, // Simplified: always term 1
        }
    }

    fn increment_revision(&mut self) {
        self.revision += 1;
    }
}

/// Matches etcd's own `--max-request-bytes` default (1.5 MiB).
#[cfg(feature = "etcd")]
const MAX_REQUEST_BYTES: usize = 1_572_864;

// gRPC status codes used by this server (google.rpc.Code).
#[cfg(feature = "etcd")]
const GRPC_UNKNOWN: i32 = 2;
#[cfg(feature = "etcd")]
const GRPC_NOT_FOUND: i32 = 5;
#[cfg(feature = "etcd")]
const GRPC_INVALID_ARGUMENT: i32 = 3;
#[cfg(feature = "etcd")]
const GRPC_RESOURCE_EXHAUSTED: i32 = 8;
#[cfg(feature = "etcd")]
const GRPC_UNIMPLEMENTED: i32 = 12;
#[cfg(feature = "etcd")]
const GRPC_INTERNAL: i32 = 13;
#[cfg(feature = "etcd")]
const GRPC_UNAVAILABLE: i32 = 14;

/// The gRPC status a failed handler call must be reported as.
///
/// `14 UNAVAILABLE` when the failure was the LLM backend being saturated, `13 INTERNAL`
/// otherwise. The distinction is not cosmetic: `UNAVAILABLE` is in gRPC's default set of
/// retryable codes and is what a `grpc-service-config` `retryableStatusCodes` list keys on,
/// while `INTERNAL` is explicitly *not* retryable. Reporting a transient rate-limiter refusal
/// as `INTERNAL` turns a backlog that would clear in seconds into a hard application error for
/// every caller — and for etcd in particular, into a failed lock acquisition that no client
/// will retry.
///
/// Neither code can be confused with success: both are non-zero, and `grpc_status_reply`
/// carries no message body at all, so there is no shape here that an empty-but-OK Range
/// response could be mistaken for.
///
/// `pub` so `tests/` can exercise the classification directly — the project forbids
/// `#[cfg(test)]` modules in `src/`, and driving the rate limiter to refusal through a spawned
/// binary needs bounds the E2E harness cannot set.
#[cfg(feature = "etcd")]
pub fn grpc_status_for_llm_failure(err: &anyhow::Error) -> i32 {
    if crate::llm::is_overload_error(err) {
        GRPC_UNAVAILABLE
    } else {
        GRPC_INTERNAL
    }
}

/// Build the client-visible failure for a handler call that could not be made, logging it on
/// both channels first. Never returns anything a client could read as an answer.
#[cfg(feature = "etcd")]
fn llm_failure(
    status_tx: &mpsc::UnboundedSender<String>,
    method: &str,
    e: anyhow::Error,
) -> GrpcFailure {
    let status = grpc_status_for_llm_failure(&e);
    let name = if status == GRPC_UNAVAILABLE {
        "UNAVAILABLE"
    } else {
        "INTERNAL"
    };
    Log::new(Some(status_tx)).warn(format!(
        "etcd {} could not be answered: the handler failed ({}). Replying grpc-status {} \
         ({}); no key-value data is being invented.",
        method, e, status, name
    ));
    GrpcFailure::new(
        status,
        crate::utils::WireFailure::classify(&e).prefixed_text(),
    )
}

/// A failure to be reported to the client as a gRPC status rather than as a transport error.
#[cfg(feature = "etcd")]
struct GrpcFailure {
    status: i32,
    message: String,
}

#[cfg(feature = "etcd")]
impl GrpcFailure {
    fn new(status: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// Map the `code` string of an `etcd_error` action onto a gRPC status code. etcd itself
    /// reports key-space failures this way, so a handler returning `KEY_NOT_FOUND` produces
    /// `5 NOT_FOUND` rather than a generic internal error.
    fn from_action_code(code: &str, message: String) -> Self {
        let status = match code.to_ascii_uppercase().as_str() {
            "KEY_NOT_FOUND" | "NOT_FOUND" => GRPC_NOT_FOUND,
            "INVALID_ARGUMENT" | "BAD_REQUEST" => GRPC_INVALID_ARGUMENT,
            "UNIMPLEMENTED" | "NOT_IMPLEMENTED" => GRPC_UNIMPLEMENTED,
            "RESOURCE_EXHAUSTED" => GRPC_RESOURCE_EXHAUSTED,
            "INTERNAL" => GRPC_INTERNAL,
            _ => GRPC_UNKNOWN,
        };
        Self::new(status, message)
    }
}

#[cfg(feature = "etcd")]
impl From<anyhow::Error> for GrpcFailure {
    fn from(e: anyhow::Error) -> Self {
        // Classify here too, not only at the explicit call sites: `?` on an anyhow error is
        // reachable from several places in this module and an overload must never be reported
        // as a non-retryable INTERNAL just because it took the implicit path.
        //
        // `grpc-message` is client-visible, so it carries the category and not the error -
        // see `crate::utils::wire_failure`.
        let status = grpc_status_for_llm_failure(&e);
        Self::new(status, crate::utils::WireFailure::classify(&e).text())
    }
}

#[cfg(feature = "etcd")]
impl From<prost::DecodeError> for GrpcFailure {
    fn from(e: prost::DecodeError) -> Self {
        // A body this server cannot decode is the client's malformed input, not a server bug -
        // but the decoder's own message is ours, so it is logged rather than sent.
        debug!("etcd request message did not decode: {e}");
        Self::new(GRPC_INVALID_ARGUMENT, "could not decode request message")
    }
}

#[cfg(feature = "etcd")]
impl From<prost::EncodeError> for GrpcFailure {
    fn from(e: prost::EncodeError) -> Self {
        debug!("etcd response encoding failed: {e}");
        Self::new(GRPC_INTERNAL, "could not encode response")
    }
}

/// Scan a handler's action results for an `etcd_error` and turn it into a gRPC status.
///
/// `etcd_error` is offered to the model on every etcd event. Before this, no handler looked
/// for it, so a model correctly reporting "key not found" had its answer dropped and the
/// client saw an empty success instead.
#[cfg(feature = "etcd")]
fn take_etcd_error(
    results: &[crate::llm::actions::protocol_trait::ActionResult],
) -> Option<GrpcFailure> {
    for result in results {
        if let crate::llm::actions::protocol_trait::ActionResult::Custom { name, data } = result {
            if name == "etcd_error" {
                let code = data
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("UNKNOWN");
                let message = data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("etcdserver: request failed")
                    .to_string();
                return Some(GrpcFailure::from_action_code(code, message));
            }
        }
    }
    None
}

/// Reduce a status message to the visible-ASCII subset `grpc-message` can carry.
///
/// `HeaderValue` accepts only visible ASCII, and these messages come from LLM output and
/// `anyhow` chains — netget's own LLM errors begin with a literal `✗`, and multi-line `anyhow`
/// context chains are routine. Without this the reply fell back to a static `"internal error"`
/// and the caller lost the reason entirely, which is how the fail-closed status came back
/// carrying nothing an operator could act on.
///
/// Substitution rather than rejection: a mangled character is a far better outcome than a
/// discarded explanation. Capped at 512 characters because a status message is a diagnostic,
/// not a payload, and every character here is ASCII by then so the cut cannot split a
/// codepoint.
#[cfg(feature = "etcd")]
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

/// Build a body-less gRPC reply carrying a status code.
///
/// `HeaderValue::from_str` can fail: `message` originates in LLM output and `anyhow` chains,
/// either of which may contain non-ASCII or control characters that are illegal in a header
/// value. It is put through [`header_safe`] first so the caller keeps the explanation, and the
/// fallback below stays as a last resort rather than the common case — it used to be the common
/// case, because netget's own LLM error strings start with `✗`.
#[cfg(feature = "etcd")]
fn grpc_status_reply(status: i32, message: &str) -> Response<Full<Bytes>> {
    let message = &header_safe(message);
    let mut res = Response::new(Full::new(Bytes::new()));
    *res.status_mut() = StatusCode::OK;
    let headers = res.headers_mut();
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/grpc+proto"),
    );
    headers.insert(
        "grpc-status",
        HeaderValue::from_str(&status.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("13")),
    );
    headers.insert(
        "grpc-message",
        HeaderValue::from_str(message)
            .unwrap_or_else(|_| HeaderValue::from_static("internal error")),
    );
    res
}

/// etcd v3 server
pub struct EtcdServer;

#[cfg(feature = "etcd")]
impl EtcdServer {
    /// Spawn etcd server with LLM-controlled KV operations
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract cluster configuration
        let cluster_name = startup_params
            .as_ref()
            .map(|p| p.get_optional_string("cluster_name"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| "netget-cluster".to_string());

        let cluster_id = 0x6574636400000001u64; // "etcd" + 1
        let member_id = 0x6d656d6265720001u64; // "member" + 1 (shortened to fit u64)

        Log::new(Some(&status_tx)).info(format!(
            "etcd server starting on {} (cluster: {})",
            listen_addr, cluster_name
        ));

        // Create in-memory meta
        let meta = Arc::new(Mutex::new(EtcdMeta::new(cluster_id, member_id)));
        let protocol = Arc::new(EtcdProtocol::new());

        // Bind to address
        let listener = tokio::net::TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("etcd server listening on {}", local_addr));

        // Spawn server task
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        Log::new(Some(&status_tx))
                            .debug(format!("etcd connection from {}", peer_addr));

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let meta_clone = meta.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                stream,
                                peer_addr,
                                local_addr,
                                llm_clone,
                                state_clone,
                                status_clone,
                                server_id,
                                meta_clone,
                                protocol_clone,
                            )
                            .await
                            {
                                error!("etcd connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        // A persistent accept error (EMFILE, listener torn down) recurs
                        // immediately, so continuing spins a hot loop that floods the
                        // unbounded status channel. Stop the listener instead.
                        Log::new(Some(&status_tx))
                            .error(format!("etcd accept failed, listener stopped: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    async fn handle_connection(
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
        local_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> Result<()> {
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;

        let io = TokioIo::new(stream);

        let service = service_fn(move |req: Request<Incoming>| {
            let llm = llm_client.clone();
            let state = app_state.clone();
            let status = status_tx.clone();
            let meta_ref = meta.clone();
            let proto = protocol.clone();

            async move {
                Self::handle_grpc_request(
                    req, peer_addr, local_addr, llm, state, status, server_id, meta_ref, proto,
                )
                .await
            }
        });

        hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(io, service)
            .await?;

        Ok(())
    }

    async fn handle_grpc_request(
        req: Request<Incoming>,
        _peer_addr: SocketAddr,
        _local_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> Result<Response<Full<Bytes>>> {
        // Never return Err from here. hyper turns a service error into an abrupt HTTP/2
        // stream reset with no gRPC status, which a client reports as a transport failure
        // with no explanation; worse, an error out of service_fn tears down the whole
        // multiplexed connection, killing every other in-flight RPC on it. Every failure
        // below becomes a well-formed gRPC status reply instead.
        Ok(
            match Self::route_grpc_request(
                req, llm_client, app_state, status_tx, server_id, meta, protocol,
            )
            .await
            {
                Ok(response) => response,
                Err(GrpcFailure { status, message }) => grpc_status_reply(status, &message),
            },
        )
    }

    async fn route_grpc_request(
        req: Request<Incoming>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> std::result::Result<Response<Full<Bytes>>, GrpcFailure> {
        // Store owned copies before consuming req
        let path = req.uri().path().to_string();
        let method = req.method().as_str().to_string();

        Log::new(Some(&status_tx)).debug(format!("etcd gRPC {} {}", method, path));

        // Cap the body before buffering it. HTTP/2 flow control bounds the window, not the
        // total, so an unbounded `collect()` lets one client grow the process without limit.
        // The limit matches etcd's own --max-request-bytes default of 1.5 MiB.
        let body = Limited::new(req.into_body(), MAX_REQUEST_BYTES);
        let whole_body = body.collect().await.map_err(|e| {
            GrpcFailure::new(
                GRPC_RESOURCE_EXHAUSTED,
                format!(
                    "request body rejected (limit {} bytes): {}",
                    MAX_REQUEST_BYTES, e
                ),
            )
        })?;
        let whole_body = whole_body.to_bytes();

        // Parse gRPC frame: 1 byte compression flag + 4 byte big-endian length + message.
        if whole_body.len() < 5 {
            return Err(GrpcFailure::new(
                GRPC_INTERNAL,
                format!("gRPC frame too short: {} bytes", whole_body.len()),
            ));
        }

        if whole_body[0] != 0 {
            // The message is compressed; nothing here can decompress it, and silently
            // handing the compressed bytes to prost would decode as garbage.
            return Err(GrpcFailure::new(
                GRPC_UNIMPLEMENTED,
                "compressed gRPC messages are not supported".to_string(),
            ));
        }

        // Honour the declared length rather than taking "everything after byte 5": the tail
        // may hold a second frame (which this unary server does not handle) and a short
        // declared length would otherwise feed trailing bytes into the protobuf decoder.
        let declared =
            u32::from_be_bytes([whole_body[1], whole_body[2], whole_body[3], whole_body[4]])
                as usize;
        let available = whole_body.len() - 5;
        if declared > available {
            return Err(GrpcFailure::new(
                GRPC_INTERNAL,
                format!(
                    "gRPC frame declares {} bytes but only {} follow the header",
                    declared, available
                ),
            ));
        }
        let msg_bytes = &whole_body[5..5 + declared];

        // Route to appropriate handler based on path
        let response_bytes = match path.as_str() {
            "/etcdserverpb.KV/Range" => {
                Self::handle_range(
                    msg_bytes, llm_client, app_state, status_tx, server_id, meta, protocol,
                )
                .await?
            }
            "/etcdserverpb.KV/Put" => {
                Self::handle_put(
                    msg_bytes, llm_client, app_state, status_tx, server_id, meta, protocol,
                )
                .await?
            }
            "/etcdserverpb.KV/DeleteRange" => {
                Self::handle_delete_range(
                    msg_bytes, llm_client, app_state, status_tx, server_id, meta, protocol,
                )
                .await?
            }
            "/etcdserverpb.KV/Txn" => {
                Self::handle_txn(
                    msg_bytes, llm_client, app_state, status_tx, server_id, meta, protocol,
                )
                .await?
            }
            "/etcdserverpb.KV/Compact" => {
                Self::handle_compact(
                    msg_bytes, llm_client, app_state, status_tx, server_id, meta, protocol,
                )
                .await?
            }
            // Only the KV service is implemented. Answering with UNIMPLEMENTED lets the
            // client report a real gRPC error and keeps the connection usable; the previous
            // bail! reset the whole HTTP/2 connection, taking concurrent RPCs with it.
            other => {
                return Err(GrpcFailure::new(
                    GRPC_UNIMPLEMENTED,
                    format!(
                        "etcd server implements only etcdserverpb.KV; no method {}",
                        other
                    ),
                ));
            }
        };

        // Build gRPC response with framing
        let mut response_with_frame = Vec::with_capacity(5 + response_bytes.len());
        response_with_frame.push(0); // Not compressed
        response_with_frame.extend_from_slice(&(response_bytes.len() as u32).to_be_bytes());
        response_with_frame.extend_from_slice(&response_bytes);

        let mut res = Response::new(Full::new(Bytes::from(response_with_frame)));
        *res.status_mut() = StatusCode::OK;
        let headers = res.headers_mut();
        headers.insert(
            "content-type",
            HeaderValue::from_static("application/grpc+proto"),
        );
        headers.insert("grpc-status", HeaderValue::from_static("0"));
        headers.insert("grpc-message", HeaderValue::from_static(""));

        Ok(res)
    }

    async fn handle_range(
        msg_bytes: &[u8],
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> std::result::Result<Vec<u8>, GrpcFailure> {
        let request = RangeRequest::decode(msg_bytes)?;

        let key_str = String::from_utf8_lossy(&request.key);
        Log::new(Some(&status_tx)).debug(format!("etcd Range request: key={}", key_str));

        trace!("etcd Range request: {:?}", request);

        // Create event for LLM
        let event = Event::new(
            &ETCD_RANGE_REQUEST_EVENT,
            serde_json::json!({
                "key": key_str,
                "range_end": if request.range_end.is_empty() { None } else { Some(String::from_utf8_lossy(&request.range_end).to_string()) },
                "limit": request.limit,
            }),
        );

        // Call LLM for decision
        let execution_result = call_llm(
            &llm_client,
            &app_state,
            server_id,
            None,
            &event,
            protocol.as_ref(),
        )
        .await
        .map_err(|e| llm_failure(&status_tx, "Range", e))?;

        if let Some(failure) = take_etcd_error(&execution_result.protocol_results) {
            return Err(failure);
        }

        // Process LLM action results to build response
        let meta_lock = meta.lock().await;
        let mut kvs = vec![];
        let mut more = false;
        let mut count = 0;
        let mut answered = false;

        for protocol_result in &execution_result.protocol_results {
            if let crate::llm::actions::protocol_trait::ActionResult::Custom { name, data } =
                protocol_result
            {
                if name == "etcd_range_response" {
                    answered = true;
                    // Parse LLM response
                    if let Some(kvs_array) = data.get("kvs").and_then(|v| v.as_array()) {
                        for kv_json in kvs_array {
                            let key = kv_json.get("key").and_then(|v| v.as_str()).unwrap_or("");
                            let value = kv_json.get("value").and_then(|v| v.as_str()).unwrap_or("");
                            let create_revision = kv_json
                                .get("create_revision")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let mod_revision = kv_json
                                .get("mod_revision")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            let version =
                                kv_json.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
                            let lease = kv_json.get("lease").and_then(|v| v.as_i64()).unwrap_or(0);

                            kvs.push(KeyValue {
                                key: key.as_bytes().to_vec(),
                                create_revision,
                                mod_revision,
                                version,
                                lease,
                                value: value.as_bytes().to_vec(),
                            });
                        }
                    }
                    more = data.get("more").and_then(|v| v.as_bool()).unwrap_or(false);
                    count = data
                        .get("count")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(kvs.len() as i64);
                }
            }
        }

        // The same fail-closed rule `handle_put` already applies, for the same reason.
        // `kvs: [], count: 0` is not the absence of an answer - in etcd it *is* an answer, and
        // a definite one: the key does not exist. A model that declined to answer, a static
        // handler with an empty action list, or a reply whose actions were all unrecognised
        // used to reach the client as "no such key", which is indistinguishable from a real
        // lookup that found nothing. A handler that genuinely means "nothing here" says so
        // with `etcd_range_response` carrying an empty `kvs`, and that still works.
        //
        // INTERNAL rather than UNAVAILABLE: nothing is saturated, so inviting a retry would
        // be wrong.
        if !answered {
            drop(meta_lock);
            Log::new(Some(&status_tx)).warn(
                "etcd Range could not be answered: the handler produced no etcd_range_response \
                 (decision=fail_closed_no_action). Replying grpc-status 13 (INTERNAL); \
                 \"key not found\" is not being invented."
                    .to_string(),
            );
            return Err(GrpcFailure::new(
                GRPC_INTERNAL,
                crate::utils::WireFailure::Unavailable.prefixed_text(),
            ));
        }

        let response = RangeResponse {
            header: Some(meta_lock.get_response_header()),
            kvs,
            more,
            count,
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(buf)
    }

    async fn handle_put(
        msg_bytes: &[u8],
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> std::result::Result<Vec<u8>, GrpcFailure> {
        let request = PutRequest::decode(msg_bytes)?;

        let key_str = String::from_utf8_lossy(&request.key);
        let value_str = String::from_utf8_lossy(&request.value);
        Log::new(Some(&status_tx)).debug(format!(
            "etcd Put request: key={}, value={}",
            key_str, value_str
        ));

        // Call LLM with put request event
        use crate::protocol::Event;
        use crate::server::etcd::actions::ETCD_PUT_REQUEST_EVENT;

        let event = Event::new(
            &ETCD_PUT_REQUEST_EVENT,
            serde_json::json!({
                "key": key_str,
                "value": value_str,
                "lease": request.lease,
            }),
        );

        Log::new(Some(&status_tx)).debug("etcd calling LLM for Put request");

        let execution_result = crate::llm::action_helper::call_llm(
            &llm_client,
            &app_state,
            server_id,
            None,
            &event,
            protocol.as_ref(),
        )
        .await
        .map_err(|e| llm_failure(&status_tx, "Put", e))?;

        // Display messages from LLM
        for message in &execution_result.messages {
            Log::new(Some(&status_tx)).info(format!("{}", message));
        }

        Log::new(Some(&status_tx)).debug(format!(
            "etcd got {} protocol results",
            execution_result.protocol_results.len()
        ));

        if let Some(failure) = take_etcd_error(&execution_result.protocol_results) {
            return Err(failure);
        }

        // Process LLM action results to build response
        let mut meta_lock = meta.lock().await;
        let mut revision: i64 = 0;
        let mut answered = false;

        for protocol_result in &execution_result.protocol_results {
            if let crate::llm::actions::protocol_trait::ActionResult::Custom { name, data } =
                protocol_result
            {
                if name == "etcd_put_response" {
                    answered = true;
                    // LLM provided put response
                    revision = data
                        .get("revision")
                        .and_then(|v| v.as_i64())
                        .unwrap_or_else(|| {
                            meta_lock.increment_revision();
                            meta_lock.revision
                        });

                    // Update meta revision if LLM provided one
                    if revision > meta_lock.revision {
                        meta_lock.revision = revision;
                    }
                }
            }
        }

        // A Put with no `etcd_put_response` used to fall through here, bump the revision and
        // return a normal PutResponse — so a model that declined the write, a static handler
        // with an empty action list, or an answer whose actions were all unrecognised was
        // reported to the client as a committed write, complete with a fresh revision number
        // that nothing had written. A client cannot tell that from a real commit.
        //
        // Refuse with the same gRPC failure shape the backend-error path uses. INTERNAL, not
        // UNAVAILABLE: nothing is saturated, so inviting a retry would be wrong.
        if !answered {
            drop(meta_lock);
            Log::new(Some(&status_tx)).warn(
                "etcd Put could not be answered: the handler produced no etcd_put_response \
                 (decision=fail_closed_no_action). Replying grpc-status 13 (INTERNAL); no \
                 revision is being invented."
                    .to_string(),
            );
            return Err(GrpcFailure::new(
                GRPC_INTERNAL,
                crate::utils::WireFailure::Unavailable.prefixed_text(),
            ));
        }

        let response = PutResponse {
            header: Some(meta_lock.get_response_header()),
            prev_kv: None,
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(buf)
    }

    async fn handle_delete_range(
        msg_bytes: &[u8],
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> std::result::Result<Vec<u8>, GrpcFailure> {
        let request = DeleteRangeRequest::decode(msg_bytes)?;

        let key_str = String::from_utf8_lossy(&request.key);
        Log::new(Some(&status_tx)).debug(format!("etcd DeleteRange request: key={}", key_str));

        // Call LLM with delete request event
        use crate::protocol::Event;
        use crate::server::etcd::actions::ETCD_DELETE_REQUEST_EVENT;

        let event = Event::new(
            &ETCD_DELETE_REQUEST_EVENT,
            serde_json::json!({
                "key": key_str,
                "range_end": if request.range_end.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(String::from_utf8_lossy(&request.range_end))
                },
            }),
        );

        Log::new(Some(&status_tx)).debug("etcd calling LLM for Delete request");

        let execution_result = crate::llm::action_helper::call_llm(
            &llm_client,
            &app_state,
            server_id,
            None,
            &event,
            protocol.as_ref(),
        )
        .await
        .map_err(|e| llm_failure(&status_tx, "DeleteRange", e))?;

        // Display messages from LLM
        for message in &execution_result.messages {
            Log::new(Some(&status_tx)).info(format!("{}", message));
        }

        Log::new(Some(&status_tx)).debug(format!(
            "etcd got {} protocol results",
            execution_result.protocol_results.len()
        ));

        if let Some(failure) = take_etcd_error(&execution_result.protocol_results) {
            return Err(failure);
        }

        // Process LLM action results to build response
        let mut meta_lock = meta.lock().await;
        let mut deleted: i64 = 0;
        let mut answered = false;

        for protocol_result in &execution_result.protocol_results {
            if let crate::llm::actions::protocol_trait::ActionResult::Custom { name, data } =
                protocol_result
            {
                if name == "etcd_delete_range_response" {
                    answered = true;
                    // LLM provided delete response
                    deleted = data.get("deleted").and_then(|v| v.as_i64()).unwrap_or(0);
                }
            }
        }

        // Same rule as Put and Range. An unanswered DeleteRange used to bump the revision and
        // return `deleted: 0` under a fresh revision number - a client reads that as a delete
        // that ran and matched nothing, which is a successful, committed operation. Refuse
        // instead, and do not move the revision counter for something that did not happen.
        if !answered {
            drop(meta_lock);
            Log::new(Some(&status_tx)).warn(
                "etcd DeleteRange could not be answered: the handler produced no \
                 etcd_delete_range_response (decision=fail_closed_no_action). Replying \
                 grpc-status 13 (INTERNAL); no deletion is being reported.\
                 "
                .to_string(),
            );
            return Err(GrpcFailure::new(
                GRPC_INTERNAL,
                crate::utils::WireFailure::Unavailable.prefixed_text(),
            ));
        }

        meta_lock.increment_revision();

        let response = DeleteRangeResponse {
            header: Some(meta_lock.get_response_header()),
            deleted,
            prev_kvs: vec![],
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(buf)
    }

    /// Handle a Txn.
    ///
    /// The comparison predicates are described to the handler and the handler decides whether
    /// they hold; the *nested* operations inside the success/failure branches are still not
    /// executed, so `responses` is always empty. This used to hardcode `succeeded: false`
    /// with no LLM call at all, which made every etcd distributed-lock acquisition fail
    /// unconditionally - and `etcd_txn_request` was advertised as an event that never fired.
    async fn handle_txn(
        msg_bytes: &[u8],
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        protocol: Arc<EtcdProtocol>,
    ) -> std::result::Result<Vec<u8>, GrpcFailure> {
        use crate::server::etcd::actions::ETCD_TXN_REQUEST_EVENT;

        let request = TxnRequest::decode(msg_bytes)?;

        Log::new(Some(&status_tx)).debug(format!(
            "etcd Txn request: {} compare(s), {} success op(s), {} failure op(s)",
            request.compare.len(),
            request.success.len(),
            request.failure.len()
        ));

        // Describe the predicates as structured JSON. Protobuf enums are rendered as their
        // spec names rather than raw numbers so a handler can reason about them.
        let compares: Vec<serde_json::Value> = request
            .compare
            .iter()
            .map(|c| {
                serde_json::json!({
                    "key": String::from_utf8_lossy(&c.key),
                    "range_end": if c.range_end.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(String::from_utf8_lossy(&c.range_end))
                    },
                    "result": match c.result {
                        0 => "EQUAL",
                        1 => "GREATER",
                        2 => "LESS",
                        3 => "NOT_EQUAL",
                        _ => "UNKNOWN",
                    },
                    "target": match c.target {
                        0 => "VERSION",
                        1 => "CREATE",
                        2 => "MOD",
                        3 => "VALUE",
                        4 => "LEASE",
                        _ => "UNKNOWN",
                    },
                })
            })
            .collect();

        let event = Event::new(
            &ETCD_TXN_REQUEST_EVENT,
            serde_json::json!({
                "compare_count": request.compare.len(),
                "success_count": request.success.len(),
                "failure_count": request.failure.len(),
                "compares": compares,
            }),
        );

        let execution_result = call_llm(
            &llm_client,
            &app_state,
            server_id,
            None,
            &event,
            protocol.as_ref(),
        )
        .await
        .map_err(|e| llm_failure(&status_tx, "Txn", e))?;

        for message in &execution_result.messages {
            Log::new(Some(&status_tx)).info(format!("{}", message));
        }

        if let Some(failure) = take_etcd_error(&execution_result.protocol_results) {
            return Err(failure);
        }

        let mut succeeded = false;
        for protocol_result in &execution_result.protocol_results {
            if let crate::llm::actions::protocol_trait::ActionResult::Custom { name, data } =
                protocol_result
            {
                if name == "etcd_txn_response" {
                    succeeded = data
                        .get("succeeded")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                }
            }
        }

        let mut meta_lock = meta.lock().await;
        meta_lock.increment_revision();

        let response = TxnResponse {
            header: Some(meta_lock.get_response_header()),
            succeeded,
            responses: vec![],
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(buf)
    }

    async fn handle_compact(
        msg_bytes: &[u8],
        _llm_client: OllamaClient,
        _app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        _server_id: crate::state::ServerId,
        meta: Arc<Mutex<EtcdMeta>>,
        _protocol: Arc<EtcdProtocol>,
    ) -> std::result::Result<Vec<u8>, GrpcFailure> {
        let request = CompactionRequest::decode(msg_bytes)?;

        Log::new(Some(&status_tx)).debug(format!(
            "etcd Compact request: revision={}",
            request.revision
        ));

        let meta_lock = meta.lock().await;

        let response = CompactionResponse {
            header: Some(meta_lock.get_response_header()),
        };

        let mut buf = Vec::new();
        response.encode(&mut buf)?;
        Ok(buf)
    }
}
