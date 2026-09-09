//! Hadoop YARN ResourceManager REST API server.
//!
//! Serves the ResourceManager web-service endpoints a real client / `curl` hits
//! (`/ws/v1/cluster/info|metrics|apps|nodes`, an app by id). The LLM roleplays the
//! cluster control plane, inventing applications, nodes and metrics per request.
//!
//! Static vs LLM-driven:
//! - `GET /ws/v1/cluster/info` — the version banner is purely mechanical and answered
//!   **statically** here, with no LLM round-trip.
//! - Anything unrecognised (not under `/ws/v1/cluster`) gets a static 404 RemoteException,
//!   again with no LLM call (keeps scanner noise off the model).
//! - `metrics`, `apps` (list + submit), `nodes`, app-by-id — **LLM-driven**.
//!
//! Fail-closed: on an LLM error, or when the model produces no `yarn_response`, the server
//! answers 503/500 with a YARN RemoteException envelope — never a success-shaped empty
//! cluster (which a client cannot distinguish from a genuinely idle cluster).

pub mod actions;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
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
use crate::server::yarn::actions::YarnProtocol;
use crate::state::app_state::AppState;
use crate::{console_error, console_info};

const JSON_CT: &str = "application/json";

/// YARN ResourceManager REST server.
pub struct YarnServer;

impl YarnServer {
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        rm_version: String,
        cluster_id: String,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        console_info!(
            status_tx,
            "YARN ResourceManager listening on {}",
            local_addr
        );

        let protocol = Arc::new(YarnProtocol::new());
        let banner = Arc::new((rm_version, cluster_id));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        Log::new(Some(&status_tx)).info(format!(
                            "YARN connection {} from {}",
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
                        let banner_clone = banner.clone();

                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let status_for_service = status_tx_clone.clone();
                            let app_state_for_service = app_state_clone.clone();

                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                let banner_clone = banner_clone.clone();
                                handle_yarn_request(
                                    req,
                                    connection_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                    server_id,
                                    banner_clone,
                                )
                            });

                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving YARN connection: {:?}", err);
                            }

                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("YARN connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "Failed to accept YARN connection: {}", e);
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

/// Approximate the bytes this request cost on the wire.
///
/// hyper hands us a parsed `Request`, so the original head is gone; this
/// reconstructs its size from the parts that survive. It is an estimate, and
/// deliberately so — the alternative is a `↓` counter frozen at 0, which reads as
/// "this peer sent nothing" rather than "we did not measure".
fn approximate_request_bytes(req: &Request<Incoming>) -> u64 {
    use hyper::body::Body;
    let head: usize = req.method().as_str().len()
        + req.uri().to_string().len()
        + 12 // " HTTP/1.1\r\n" plus the blank line terminating the head
        + req
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len() + 4)
            .sum::<usize>();
    head as u64 + req.body().size_hint().lower()
}

/// Record one request/response exchange against the connection's counters, then
/// return the response unchanged.
///
/// `update_connection_stats` is what the dashboard rail's `↓ ↑` columns and the
/// connection-scoped task prompts read, and it is what keeps `last_activity`
/// moving. Without it a busy YARN server draws every peer as idle having sent
/// and received nothing.
#[allow(clippy::too_many_arguments)]
async fn handle_yarn_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<YarnProtocol>,
    server_id: crate::state::ServerId,
    banner: Arc<(String, String)>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let bytes_received = approximate_request_bytes(&req);
    let response = handle_yarn_request_inner(
        req,
        connection_id,
        llm_client,
        app_state.clone(),
        status_tx,
        protocol,
        server_id,
        banner,
    )
    .await;

    let bytes_sent = response
        .as_ref()
        .ok()
        .and_then(|resp| {
            use hyper::body::Body;
            resp.body().size_hint().exact()
        })
        .unwrap_or(0);
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(bytes_received),
            Some(bytes_sent),
            Some(1),
            Some(1),
        )
        .await;

    response
}

#[allow(clippy::too_many_arguments)]
async fn handle_yarn_request_inner(
    req: Request<Incoming>,
    _connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<YarnProtocol>,
    server_id: crate::state::ServerId,
    banner: Arc<(String, String)>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    // Bounded. `POST /ws/v1/cluster/apps` carries a real body, `Incoming` has no
    // default limit, and this body is buffered whole *and* interpolated into the LLM
    // prompt below — so without a cap an unauthenticated submit of arbitrary length
    // allocates twice over. `Limited` errors as soon as the cap is passed rather than
    // after buffering the whole thing.
    //
    // Falling back to an empty body would be worse than refusing: the model would be
    // asked to act on a submission it never saw, and would answer as if it had.
    let body_bytes = match http_body_util::Limited::new(
        req.into_body(),
        crate::server::http_common::handler::MAX_REQUEST_BODY_BYTES,
    )
    .collect()
    .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            console_error!(
                status_tx,
                "YARN {} {} decision=refused_body_too_large (limit {} bytes) -> 413: {}",
                method,
                path,
                crate::server::http_common::handler::MAX_REQUEST_BODY_BYTES,
                e
            );
            return Ok(build_yarn_response(
                413,
                yarn_remote_exception(
                    413,
                    "WebApplicationException",
                    &format!(
                        "request body exceeds {} bytes",
                        crate::server::http_common::handler::MAX_REQUEST_BODY_BYTES
                    ),
                ),
                None,
            ));
        }
    };
    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    let (operation, app_id, node_id) = detect_yarn_operation(&method, &path);
    Log::new(Some(&status_tx)).debug(format!("YARN {} {} op={}", method, path, operation));
    trace!("YARN request body: {}", body_str);

    // Mechanical endpoints answered without an LLM call.
    if operation == "info" {
        return Ok(build_cluster_info(&banner));
    }
    if operation == "unknown" {
        return Ok(build_yarn_response(
            404,
            yarn_remote_exception(404, "NotFoundException", &format!("unknown path: {path}")),
            None,
        ));
    }

    let event = crate::protocol::Event::new(
        &actions::YARN_REQUEST_EVENT,
        serde_json::json!({
            "method": method,
            "path": path,
            "operation": operation,
            "app_id": app_id,
            "node_id": node_id,
            "request_body": body_str,
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
                    if name == "yarn_response" {
                        let status =
                            data.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                        let body = data
                            .get("body")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let location = data
                            .get("location")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        Log::new(Some(&status_tx)).debug(format!("YARN -> {}", status));
                        trace!("YARN response body: {}", body);
                        return Ok(build_yarn_response_str(status, body, location));
                    }
                }
            }
            // Fail-closed: the model answered but produced no YARN response. Do NOT fall
            // through to a success-shaped empty cluster — that is indistinguishable from a
            // real idle cluster and is the fail-open trap.
            Log::new(Some(&status_tx))
                .error("YARN: LLM returned no yarn_response action; answering 500");
            Ok(build_yarn_response(
                500,
                yarn_remote_exception(
                    500,
                    "WebApplicationException",
                    "netget: model produced no YARN response",
                ),
                None,
            ))
        }
        Err(e) => {
            // Overload is transient/retryable -> 503; anything else -> 500. Both carry the
            // RemoteException envelope every YARN client parses.
            let overloaded = crate::llm::is_overload_error(&e);
            let (status, exception) = if overloaded {
                (503u16, "ServiceUnavailableException")
            } else {
                (500u16, "WebApplicationException")
            };
            error!("LLM error for YARN request (status {}): {}", status, e);
            console_error!(status_tx, "YARN answering {} on LLM failure: {}", status, e);
            let reason = crate::utils::WireFailure::classify(&e).prefixed_text();
            Ok(build_yarn_response(
                status,
                yarn_remote_exception(status, exception, reason),
                None,
            ))
        }
    }
}

/// Answer `GET /ws/v1/cluster/info` statically (mechanical version banner, no LLM).
fn build_cluster_info(banner: &(String, String)) -> Response<Full<Bytes>> {
    let (version, cluster_id) = banner;
    let started_on: u64 = cluster_id.parse().unwrap_or(1476912658570);
    let body = serde_json::json!({
        "clusterInfo": {
            "id": started_on,
            "startedOn": started_on,
            "state": "STARTED",
            "haState": "ACTIVE",
            "rmStateStoreName":
                "org.apache.hadoop.yarn.server.resourcemanager.recovery.NullRMStateStore",
            "resourceManagerVersion": version,
            "resourceManagerBuildVersion": format!("{version} from netget"),
            "resourceManagerVersionBuiltOn": "2025-01-01T00:00Z",
            "hadoopVersion": version,
            "hadoopBuildVersion": format!("{version} from netget"),
            "hadoopVersionBuiltOn": "2025-01-01T00:00Z",
            "haZooKeeperConnectionState": "ResourceManager HA is not enabled."
        }
    });
    build_yarn_response(200, body, None)
}

fn yarn_remote_exception(_status: u16, exception: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "RemoteException": {
            "exception": exception,
            "message": message,
            "javaClassName": format!("org.apache.hadoop.yarn.webapp.{exception}"),
        }
    })
}

/// Build a YARN JSON response from a serde value.
fn build_yarn_response(
    status: u16,
    body: serde_json::Value,
    location: Option<String>,
) -> Response<Full<Bytes>> {
    build_yarn_response_str(
        status,
        serde_json::to_string(&body).unwrap_or_default(),
        location,
    )
}

/// Build a YARN response from an already-serialized body string.
///
/// `status` originates in model output; `StatusCode::from_u16` rejects out-of-range values
/// and the previous `.unwrap()` shape would panic inside the hyper task. An empty body (202
/// Accepted on submit) is sent without a Content-Type.
fn build_yarn_response_str(
    status: u16,
    body: String,
    location: Option<String>,
) -> Response<Full<Bytes>> {
    let status = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("Invalid YARN status code {}, sending 500 instead", status);
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });

    let mut builder = Response::builder().status(status);
    if !body.is_empty() {
        builder = builder.header("Content-Type", JSON_CT);
    }
    if let Some(loc) = location {
        // Header values hyper rejects (e.g. CR/LF injection) are dropped rather than panicking.
        if let Ok(v) = hyper::header::HeaderValue::from_str(&loc) {
            builder = builder.header(hyper::header::LOCATION, v);
        }
    }
    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("{}"))))
}

/// Map a YARN RM request to an operation name + optional app/node id.
fn detect_yarn_operation(method: &str, path: &str) -> (String, Option<String>, Option<String>) {
    let trimmed = path.trim_start_matches('/');
    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();

    // Everything lives under ws/v1/cluster
    match (method, parts.as_slice()) {
        ("GET", ["ws", "v1", "cluster"]) => ("info".to_string(), None, None),
        ("GET", ["ws", "v1", "cluster", "info"]) => ("info".to_string(), None, None),
        ("GET", ["ws", "v1", "cluster", "metrics"]) => ("metrics".to_string(), None, None),
        ("GET", ["ws", "v1", "cluster", "apps"]) => ("apps".to_string(), None, None),
        ("POST", ["ws", "v1", "cluster", "apps", "new-application"]) => {
            ("new_application".to_string(), None, None)
        }
        ("POST", ["ws", "v1", "cluster", "apps"]) => ("submit".to_string(), None, None),
        ("GET", ["ws", "v1", "cluster", "apps", app_id]) => {
            ("app".to_string(), Some(app_id.to_string()), None)
        }
        ("GET", ["ws", "v1", "cluster", "nodes"]) => ("nodes".to_string(), None, None),
        ("GET", ["ws", "v1", "cluster", "nodes", node_id]) => {
            ("node".to_string(), None, Some(node_id.to_string()))
        }
        _ => ("unknown".to_string(), None, None),
    }
}
