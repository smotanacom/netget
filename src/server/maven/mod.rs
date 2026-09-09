//! Maven repository server implementation using hyper
pub mod actions;

use std::collections::HashMap;
use std::convert::Infallible;
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

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::MavenProtocol;
use crate::state::app_state::AppState;
use actions::MAVEN_ARTIFACT_REQUEST_EVENT;

/// Maven repository server that delegates artifact requests to LLM
pub struct MavenServer;

impl MavenServer {
    /// Spawn the Maven repository server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "Maven repository server listening on {}",
            local_addr
        ));

        let protocol = Arc::new(MavenProtocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        Log::new(Some(&status_tx)).info(format!(
                            "Maven connection {} from {}",
                            connection_id, remote_addr
                        ));

                        // Add connection to ServerInstance
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

                        // Spawn a task to handle this connection
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);

                            // Clone for service closure
                            let status_for_service = status_tx_clone.clone();
                            let app_state_for_service = app_state_clone.clone();

                            // Create a service that handles Maven requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_maven_request_with_llm(
                                    req,
                                    connection_id,
                                    server_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                )
                            });

                            // Serve HTTP/1 on this connection
                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                // Non-fatal: this connection ends; the server continues.
                                Log::new(Some(&status_tx_clone))
                                    .warn(format!("Error serving Maven connection: {:?}", err));
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("Maven connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept Maven connection: {}", e));
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

/// How much of a request body is read before the request is refused with 413.
///
/// Deliberately a local constant rather than a reference to
/// `http_common::handler::MAX_REQUEST_BODY_BYTES`, which carries the same value:
/// `http_common` is gated on `#[cfg(any(feature = "http", ...))]` and `maven = []`
/// pulls none of those, so borrowing it broke `cargo check --no-default-features
/// --features maven` — exactly what CI's `single-feature` job exists to catch.
/// A per-protocol constant is also what the decentralisation rule asks for.
///
/// `Incoming` has no default limit, so without a cap the buffer is whatever the peer
/// chooses to send.
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Parse Maven artifact path into components
///
/// Maven paths follow the pattern:
/// /{groupId}/{artifactId}/{version}/{artifactId}-{version}[-{classifier}].{extension}
///
/// Examples:
/// - /com/example/mylib/1.0.0/mylib-1.0.0.jar
/// - /com/example/mylib/1.0.0/mylib-1.0.0.pom
/// - /com/example/mylib/1.0.0/mylib-1.0.0-sources.jar
/// - /com/example/mylib/maven-metadata.xml
#[derive(Debug)]
struct MavenArtifact {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    classifier: Option<String>,
    extension: String,
    is_metadata: bool,
    is_checksum: bool,
    checksum_type: Option<String>, // sha1, md5, sha256, sha512
}

impl MavenArtifact {
    fn parse(uri: &str) -> Option<Self> {
        let path = uri.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        if parts.is_empty() {
            return None;
        }

        // Check if this is a maven-metadata.xml request
        if parts.last() == Some(&"maven-metadata.xml")
            || parts.last() == Some(&"maven-metadata.xml.sha1")
            || parts.last() == Some(&"maven-metadata.xml.md5")
        {
            let is_checksum = parts.last()?.contains(".sha1") || parts.last()?.contains(".md5");
            let checksum_type = if parts.last()?.ends_with(".sha1") {
                Some("sha1".to_string())
            } else if parts.last()?.ends_with(".md5") {
                Some("md5".to_string())
            } else {
                None
            };

            // Group ID is everything except last 2 elements (artifact_id and filename)
            if parts.len() < 2 {
                return None;
            }
            let group_id = parts[..parts.len() - 2].join(".");
            let artifact_id = parts[parts.len() - 2].to_string();

            return Some(Self {
                group_id,
                artifact_id,
                version: None,
                classifier: None,
                extension: "xml".to_string(),
                is_metadata: true,
                is_checksum,
                checksum_type,
            });
        }

        // Regular artifact request - need at least 4 parts: group/artifact/version/file
        if parts.len() < 4 {
            return None;
        }

        let filename = parts.last()?;
        let version = parts[parts.len() - 2];
        let artifact_id = parts[parts.len() - 3];
        let group_parts = &parts[..parts.len() - 3];
        let group_id = group_parts.join(".");

        // Parse filename: {artifactId}-{version}[-{classifier}].{extension}[.checksum]
        // First check for checksums
        let (filename, is_checksum, checksum_type) = if filename.ends_with(".sha1") {
            (
                filename.trim_end_matches(".sha1"),
                true,
                Some("sha1".to_string()),
            )
        } else if filename.ends_with(".md5") {
            (
                filename.trim_end_matches(".md5"),
                true,
                Some("md5".to_string()),
            )
        } else if filename.ends_with(".sha256") {
            (
                filename.trim_end_matches(".sha256"),
                true,
                Some("sha256".to_string()),
            )
        } else if filename.ends_with(".sha512") {
            (
                filename.trim_end_matches(".sha512"),
                true,
                Some("sha512".to_string()),
            )
        } else {
            (*filename, false, None)
        };

        // Get extension
        let extension = filename.split('.').last()?.to_string();

        // Parse base filename (without extension)
        let base = filename.trim_end_matches(&format!(".{}", extension));

        // Expected format: {artifactId}-{version}[-{classifier}]
        let expected_prefix = format!("{}-{}", artifact_id, version);

        if !base.starts_with(&expected_prefix) {
            return None;
        }

        // Extract classifier if present
        let classifier = if base.len() > expected_prefix.len() {
            let remainder = &base[expected_prefix.len()..];
            if remainder.starts_with('-') {
                Some(remainder[1..].to_string())
            } else {
                return None; // Invalid format
            }
        } else {
            None
        };

        Some(Self {
            group_id,
            artifact_id: artifact_id.to_string(),
            version: Some(version.to_string()),
            classifier,
            extension,
            is_metadata: false,
            is_checksum,
            checksum_type,
        })
    }
}

/// Build the 502 sent when model or handler output cannot be turned into a
/// valid HTTP response (bad status code, malformed header name, ...).
fn bad_gateway() -> Response<Full<Bytes>> {
    Response::builder()
        .status(502)
        .body(Full::new(Bytes::from(
            "Bad Gateway: invalid Maven response",
        )))
        .expect("502 response with a literal body is always valid")
}

/// The 413 for a request body over `MAX_REQUEST_BODY_BYTES`.
///
/// Static text: the peer learns the fact and the limit, never why the read failed.
fn payload_too_large() -> Response<Full<Bytes>> {
    Response::builder()
        .status(413)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(format!(
            "Payload Too Large: request bodies are limited to {} bytes\n",
            MAX_REQUEST_BODY_BYTES
        ))))
        .expect("413 response with a literal body is always valid")
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
/// moving. Without it a busy Maven server draws every peer as idle having sent
/// and received nothing.
#[allow(clippy::too_many_arguments)]
async fn handle_maven_request_with_llm(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<MavenProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let bytes_received = approximate_request_bytes(&req);
    let response = handle_maven_request_with_llm_inner(
        req,
        connection_id,
        server_id,
        llm_client,
        app_state.clone(),
        status_tx,
        protocol,
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

/// Handle a single Maven artifact request with integrated LLM actions
async fn handle_maven_request_with_llm_inner(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<MavenProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let log = Log::new(Some(&status_tx));

    // Extract headers
    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(name.to_string(), value_str.to_string());
        }
    }

    // Drain the body, bounded. Maven GETs carry no body, but `Incoming` has no
    // default limit and the body must still be consumed for keep-alive, so an
    // unauthenticated peer could otherwise make this server buffer an arbitrary
    // number of bytes it then throws away. `Limited` errors as soon as the cap is
    // passed rather than after buffering the whole thing.
    if let Err(e) = http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        log.warn(format!(
            "Maven {} {} decision=refused_body_too_large (limit {} bytes) -> 413: {}",
            method, uri, MAX_REQUEST_BODY_BYTES, e
        ));
        return Ok(payload_too_large());
    }

    // Summary + full payload FileOnly: the maven_artifact_request event template
    // renders the equivalent line to the TUI.
    log.debug(format!(
        "Maven request: {} {} from {:?}",
        method, uri, connection_id
    ));

    // Parse Maven artifact from URI
    let artifact = MavenArtifact::parse(&uri);

    if let Some(ref art) = artifact {
        log.trace(format!("Parsed Maven artifact: {:?}", art));
    }

    // Create Maven artifact request event
    let event_params = if let Some(art) = artifact {
        serde_json::json!({
            "method": method,
            "uri": uri,
            "group_id": art.group_id,
            "artifact_id": art.artifact_id,
            "version": art.version,
            "classifier": art.classifier,
            "extension": art.extension,
            "is_metadata": art.is_metadata,
            "is_checksum": art.is_checksum,
            "checksum_type": art.checksum_type,
            "headers": headers,
        })
    } else {
        // Invalid Maven path format
        log.debug(format!("Invalid Maven artifact path: {}", uri));

        return Ok(Response::builder()
            .status(404)
            .body(Full::new(Bytes::from("Invalid Maven artifact path")))
            .unwrap());
    };

    let event = Event::new(&MAVEN_ARTIFACT_REQUEST_EVENT, event_params);

    // Call LLM to handle Maven request
    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(execution_result) => {
            log.debug("LLM Maven response received");

            // Display messages
            for msg in execution_result.messages {
                let _ = status_tx.send(msg);
            }

            // Extract Maven response from protocol results
            // Default to 404 Not Found
            let mut status_code = 404;
            let mut response_headers = HashMap::new();
            let mut response_body = Vec::new();
            let mut produced_response = false;

            for protocol_result in execution_result.protocol_results {
                if let ActionResult::Output(output_data) = protocol_result {
                    // Parse the output as JSON containing Maven response fields
                    if let Ok(json_value) =
                        serde_json::from_slice::<serde_json::Value>(&output_data)
                    {
                        produced_response = true;
                        if let Some(status) = json_value.get("status").and_then(|v| v.as_u64()) {
                            status_code = status as u16;
                        }
                        if let Some(headers_obj) =
                            json_value.get("headers").and_then(|v| v.as_object())
                        {
                            for (k, v) in headers_obj {
                                if let Some(v_str) = v.as_str() {
                                    response_headers.insert(k.clone(), v_str.to_string());
                                }
                            }
                        }

                        // Handle both UTF-8 text and base64-encoded binary content.
                        // body_base64 is checked first: it is only ever set by the
                        // action executor, which has already validated the encoding.
                        if let Some(encoded) =
                            json_value.get("body_base64").and_then(|v| v.as_str())
                        {
                            use base64::Engine;
                            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                                Ok(decoded) => response_body = decoded,
                                Err(e) => {
                                    // Fail closed: shipping the declared status with an
                                    // empty body would look like a successful zero-byte
                                    // artifact download and Maven would cache it.
                                    tracing::error!(
                                        "Maven {} {} decision=fail_closed_bad_body error={}",
                                        method,
                                        uri,
                                        e
                                    );
                                    log.warn(format!(
                                        "Maven {} {} decision=fail_closed_bad_body: response \
                                         body_base64 is not valid base64: {}",
                                        method, uri, e
                                    ));
                                    return Ok(bad_gateway());
                                }
                            }
                        } else if let Some(body_str) =
                            json_value.get("body").and_then(|v| v.as_str())
                        {
                            response_body = body_str.as_bytes().to_vec();
                        }
                    }
                }
            }

            // Three outcomes stay distinguishable in the log: the model produced a
            // response (`model_answer`), the model produced nothing usable and the
            // default 404 stands (`model_no_action`), and the LLM call itself failed
            // (`fail_closed_llm_*`, in the Err arm below).
            let decision = if produced_response {
                "model_answer"
            } else {
                "model_no_action"
            };

            // Response summary FileOnly: the send_maven_* action template already
            // reports the send to the TUI.
            log.debug(format!(
                "Maven {} {} decision={} -> {} ({} bytes)",
                method,
                uri,
                decision,
                status_code,
                response_body.len()
            ));

            // Build the HTTP response. Status and header names come from model or
            // handler output, so an invalid one must not take the connection down.
            let mut response = Response::builder().status(status_code);

            // Add headers
            for (name, value) in response_headers {
                response = response.header(name, value);
            }

            match response.body(Full::new(Bytes::from(response_body))) {
                Ok(resp) => Ok(resp),
                Err(e) => {
                    // Non-fatal: invalid model/handler output; the client gets a 502.
                    log.warn(format!("Invalid Maven response ({}), sending 502", e));
                    Ok(bad_gateway())
                }
            }
        }
        Err(e) => {
            // The peer gets a category, the log gets the error. Nothing derived
            // from `e` may reach the wire: the bodies below are literals and the
            // status is chosen from the classification alone.
            let failure = crate::utils::WireFailure::classify(&e);
            let decision = if failure.is_overloaded() {
                "fail_closed_llm_overloaded"
            } else {
                "fail_closed_llm_error"
            };
            tracing::error!("Maven {} {} decision={} error={}", method, uri, decision, e);
            log.warn(format!(
                "Maven {} {} decision={}: {}",
                method, uri, decision, e
            ));

            if failure.is_overloaded() {
                // Transient saturation. 503 + Retry-After tells Maven to back off
                // and retry rather than record a permanent repository fault.
                return Ok(Response::builder()
                    .status(503)
                    .header(hyper::header::RETRY_AFTER, "1")
                    .body(Full::new(Bytes::from("Service Unavailable")))
                    .expect("503 response with a literal body is always valid"));
            }

            Ok(Response::builder()
                .status(500)
                .body(Full::new(Bytes::from("Internal Server Error")))
                .expect("500 response with a literal body is always valid"))
        }
    }
}
