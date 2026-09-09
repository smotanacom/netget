//! Apache Spark monitoring REST API server.
//!
//! Serves the Spark monitoring endpoints a client / the History Server UI hits
//! (`/api/v1/applications`, `.../{id}/jobs`, `/stages`, `/executors`). The LLM roleplays the
//! application's control plane, inventing applications/jobs/stages/executors per request.
//!
//! Static vs LLM-driven:
//! - `GET /api/v1/version` — mechanical version banner, answered **statically** (no LLM).
//! - Unrecognised paths get a static 404 (plain text), no LLM call.
//! - `applications`, `jobs`, `stages`, `executors` — **LLM-driven**.
//!
//! Fail-closed: on an LLM error, or when the model produces no `spark_response`, the server
//! answers 503/500 with a JSON error object — never a success-shaped empty array (which a client
//! cannot distinguish from a genuinely empty application list).

pub mod actions;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;
use tracing::{error, trace};

use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::server::connection::ConnectionId;
use crate::server::spark::actions::SparkProtocol;
use crate::state::app_state::AppState;
use crate::{console_error, console_info};

/// Largest request body this server will buffer.
///
/// The monitoring API is read-only, so a body is never needed — but it was read with an
/// unbounded `req.into_body().collect()` and then used for nothing but a `trace!`, which
/// made an unauthenticated POST of any size a way to grow the process. Small on purpose.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Apache Spark monitoring REST server.
pub struct SparkServer;

impl SparkServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        spark_version: String,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        console_info!(status_tx, "Spark REST API listening on {}", local_addr);

        let protocol = Arc::new(SparkProtocol::new());
        let version = Arc::new(spark_version);

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        Log::new(Some(&status_tx)).info(format!(
                            "Spark connection {} from {}",
                            connection_id, remote_addr
                        ));

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: local_addr_conn,
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

                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let version_clone = version.clone();

                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let status_for_service = status_tx_clone.clone();
                            let app_state_for_service = app_state_clone.clone();

                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                let version_clone = version_clone.clone();
                                handle_spark_request(
                                    req,
                                    connection_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                    server_id,
                                    version_clone,
                                )
                            });

                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving Spark connection: {:?}", err);
                            }

                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("Spark connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "Failed to accept Spark connection: {}", e);
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
}

/// Handle one monitoring-API request, then record what crossed the wire.
///
/// The rail's byte counters and the connection-scoped task prompts read
/// `bytes_received`/`bytes_sent`, and this server left both at the zero it registered the
/// connection with, so every Spark peer showed no traffic at all however much it moved.
#[allow(clippy::too_many_arguments)]
async fn handle_spark_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SparkProtocol>,
    server_id: crate::state::ServerId,
    version: Arc<String>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let received = req.body().size_hint().lower();
    let response = handle_spark_request_inner(
        req,
        llm_client,
        app_state.clone(),
        status_tx.clone(),
        protocol,
        server_id,
        version,
    )
    .await;

    let sent = response
        .as_ref()
        .ok()
        .and_then(|resp| resp.body().size_hint().exact())
        .unwrap_or(0);
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(received),
            Some(sent),
            Some(1),
            Some(1),
        )
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());

    response
}

#[allow(clippy::too_many_arguments)]
async fn handle_spark_request_inner(
    req: Request<Incoming>,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SparkProtocol>,
    server_id: crate::state::ServerId,
    version: Arc<String>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // The monitoring API takes no request body; this is read only so an unexpected one
    // shows up in the trace log. An over-cap body is dropped rather than refused, because
    // nothing downstream reads it and every endpoint here is a GET.
    let body_bytes = match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!(
                "Spark request body ignored (over {} bytes): {}",
                MAX_REQUEST_BYTES, e
            ));
            Bytes::new()
        }
    };
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    let (operation, app_id) = detect_spark_operation(&method, &path);
    Log::new(Some(&status_tx)).debug(format!("Spark {} {} op={}", method, path, operation));
    trace!("Spark request body: {}", body_str);

    if operation == "version" {
        let body = serde_json::json!({ "spark": version.as_str() }).to_string();
        return Ok(build_spark_response(200, body, "application/json"));
    }
    if operation == "unknown" {
        return Ok(build_spark_response(
            404,
            format!("no such endpoint: {path}"),
            "text/plain",
        ));
    }

    let event = crate::protocol::Event::new(
        &actions::SPARK_REQUEST_EVENT,
        serde_json::json!({
            "method": method,
            "path": path,
            "operation": operation,
            "app_id": app_id,
        }),
    );

    let llm_result = crate::llm::action_helper::call_llm(
        &llm_client,
        &app_state,
        server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await;

    match llm_result {
        Ok(execution_result) => {
            for result in execution_result.protocol_results {
                if let ActionResult::Custom { name, data } = result {
                    if name == "spark_response" {
                        let status =
                            data.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                        let body = data
                            .get("body")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let ct = data
                            .get("content_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("application/json");
                        Log::new(Some(&status_tx)).debug(format!("Spark -> {}", status));
                        trace!("Spark response body: {}", body);
                        return Ok(build_spark_response(status, body, ct));
                    }
                }
            }
            // Fail-closed: the model answered but produced no Spark response. A bare `[]` with
            // 200 is a valid "no applications/jobs" result and a client cannot tell it from a
            // backend that never ran — so answer 500 instead of that empty array.
            Log::new(Some(&status_tx))
                .error("Spark: LLM returned no spark_response action; answering 500");
            Ok(build_spark_error(
                500,
                "netget: model produced no Spark response",
            ))
        }
        Err(e) => {
            let overloaded = crate::llm::is_overload_error(&e);
            let status = if overloaded { 503u16 } else { 500u16 };
            error!("LLM error for Spark request (status {}): {}", status, e);
            console_error!(
                status_tx,
                "Spark answering {} on LLM failure: {}",
                status,
                e
            );
            let reason = crate::utils::WireFailure::classify(&e).prefixed_text();
            Ok(build_spark_error(status, reason))
        }
    }
}

/// Fail-closed JSON error body (distinct from any success array, which is a bare `[...]`).
fn build_spark_error(status: u16, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "error": message, "status": status }).to_string();
    build_spark_response(status, body, "application/json")
}

/// Build a Spark response. `status` originates in model output; `StatusCode::from_u16` rejects
/// out-of-range values where the previous `.unwrap()` shape would panic inside the hyper task.
fn build_spark_response(status: u16, body: String, content_type: &str) -> Response<Full<Bytes>> {
    let status = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("Invalid Spark status code {}, sending 500 instead", status);
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });
    Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("[]"))))
}

/// Map a Spark monitoring-API request to an operation name + optional app id.
fn detect_spark_operation(method: &str, path: &str) -> (String, Option<String>) {
    let trimmed = path.trim_start_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();

    match (method, parts.as_slice()) {
        ("GET", ["api", "v1", "version"]) => ("version".to_string(), None),
        ("GET", ["api", "v1", "applications"]) => ("applications".to_string(), None),
        ("GET", ["api", "v1", "applications", id]) => {
            ("application".to_string(), Some(id.to_string()))
        }
        ("GET", ["api", "v1", "applications", id, "jobs"]) => {
            ("jobs".to_string(), Some(id.to_string()))
        }
        ("GET", ["api", "v1", "applications", id, "stages"]) => {
            ("stages".to_string(), Some(id.to_string()))
        }
        ("GET", ["api", "v1", "applications", id, "executors"]) => {
            ("executors".to_string(), Some(id.to_string()))
        }
        // A per-application sub-resource we don't model explicitly still reaches the LLM as a
        // generic per-application request so the model can answer or 404 it.
        ("GET", ["api", "v1", "applications", id, _rest @ ..]) => {
            ("application".to_string(), Some(id.to_string()))
        }
        _ => ("unknown".to_string(), None),
    }
}
