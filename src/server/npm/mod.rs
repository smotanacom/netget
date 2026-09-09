//! NPM registry server implementation
//!
//! NPM registry runs over HTTP. The LLM controls package metadata, tarballs,
//! listings, and search results.

pub mod actions;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use base64::Engine;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, trace};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::EventType;
use crate::server::connection::ConnectionId;
use crate::server::npm::actions::NpmProtocol;
use crate::state::app_state::AppState;

/// NPM registry server that delegates to LLM
pub struct NpmServer;

impl NpmServer {
    /// Spawn the NPM registry server with integrated LLM actions
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
        Log::new(Some(&status_tx)).info(format!("NPM registry server listening on {}", local_addr));

        let protocol = Arc::new(NpmProtocol::new());

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
                            "NPM connection {} from {}",
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

                            // Create a service that handles NPM registry requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_npm_request(
                                    req,
                                    connection_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                    server_id,
                                )
                            });

                            // Serve HTTP/1 on this connection
                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving NPM connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("NPM connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept NPM connection: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
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
/// moving. Without it a busy NPM server draws every peer as idle with zero
/// bytes in both directions — which is exactly what the rail is for.
#[allow(clippy::too_many_arguments)]
async fn handle_npm_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<NpmProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let bytes_received = approximate_request_bytes(&req);
    let response = handle_npm_request_inner(
        req,
        connection_id,
        llm_client,
        app_state.clone(),
        status_tx,
        protocol,
        server_id,
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

/// Handle a single NPM registry request
async fn handle_npm_request_inner(
    req: Request<Incoming>,
    _connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<NpmProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();
    let query = uri.query().unwrap_or("");

    Log::new(Some(&status_tx)).debug(format!("NPM request: {} {}", method, path));

    // Only handle GET requests
    if method != Method::GET {
        let response = json!({
            "error": "Method not allowed"
        });
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(response.to_string())))
            .unwrap());
    }

    // Route the request
    let (event_type, description) = if path == "/-/all" {
        ("NPM_LIST_REQUEST", "NPM package list request".to_string())
    } else if path.starts_with("/-/v1/search") {
        (
            "NPM_SEARCH_REQUEST",
            format!("NPM package search: {}", query),
        )
    } else if path.contains("/-/") {
        // Tarball request: /{package}/-/{tarball}.tgz
        let parts: Vec<&str> = path.split("/-/").collect();
        let package_name = parts.get(0).unwrap_or(&"").trim_start_matches('/');
        let tarball_name = parts.get(1).unwrap_or(&"");
        (
            "NPM_TARBALL_REQUEST",
            format!(
                "NPM tarball request: package={}, tarball={}",
                package_name, tarball_name
            ),
        )
    } else {
        // Package metadata request: /{package}
        let package_name = path.trim_start_matches('/');
        (
            "NPM_PACKAGE_REQUEST",
            format!("NPM package metadata request: {}", package_name),
        )
    };

    trace!("NPM event: {}: {}", event_type, &description);

    // Verify server exists
    if app_state.get_instruction(server_id).await.is_none() {
        error!("Server {} not found", server_id);
        let response = json!({
            "error": "Server not found"
        });
        return Ok(Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(response.to_string())))
            .unwrap());
    }

    // Build NPM event - use the static event type references
    let event_type_static: &'static EventType = match &event_type[..] {
        "NPM_PACKAGE_REQUEST" => &actions::NPM_PACKAGE_REQUEST,
        "NPM_TARBALL_REQUEST" => &actions::NPM_TARBALL_REQUEST,
        "NPM_LIST_REQUEST" => &actions::NPM_LIST_REQUEST,
        "NPM_SEARCH_REQUEST" => &actions::NPM_SEARCH_REQUEST,
        _ => {
            error!("Unknown NPM event type: {}", event_type);
            let error_response = json!({
                "error": format!("Internal error: unknown event type '{}'", event_type)
            });
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(error_response.to_string())))
                .unwrap());
        }
    };

    let event = crate::protocol::Event::new(
        event_type_static,
        json!({
            "method": method.as_str(),
            "path": path,
            "query": query,
            "description": description,
        }),
    );

    Log::new(Some(&status_tx)).debug(format!("Calling LLM for NPM request: {} {}", method, path));

    // Call LLM
    let llm_result = call_llm(
        &llm_client,
        &app_state,
        server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await;

    // Process LLM result
    match llm_result {
        Ok(execution_result) => {
            // Scan for the first action that is actually an NPM response. This was a
            // `for` loop with an unconditional `return` inside it, so it examined only
            // the first result and returned a 500 if that happened to be something
            // like `show_message`. It also tripped clippy's `never_loop`.
            for result in execution_result.protocol_results {
                if let Some(response) = process_npm_action_result(result, &status_tx).await {
                    return Ok(response);
                }
            }

            // No NPM actions found, return error
            let error_response = json!({
                "error": "No NPM action returned"
            });
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(error_response.to_string())))
                .unwrap())
        }
        Err(e) => {
            // Non-fatal: a wire fallback (JSON error response) is still delivered and the
            // HTTP connection continues.
            Log::new(Some(&status_tx)).warn(format!("NPM LLM call failed: {}", e));
            let error_response = json!({
                "error": crate::utils::WireFailure::classify(&e).text()
            });
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(error_response.to_string())))
                .unwrap())
        }
    }
}

/// A 500 for an answer the model gave that this server cannot turn into a response.
///
/// Every branch below used to `.unwrap()` on the field it needed. A missing field is
/// the model's mistake, and `.unwrap()` inside a per-connection tokio task is the
/// worst possible response to it: the panic is swallowed, the server keeps reporting
/// `Running`, and the client hangs until its own timeout with nothing in the log
/// connecting the two. An explicit 500 tells the client *and* the operator.
///
/// `reason` names action fields and this server's own expectations of them, so it goes
/// to the operator only — the peer gets a category. See `crate::utils::wire_failure`.
fn npm_server_error(
    status_tx: &mpsc::UnboundedSender<String>,
    reason: &str,
) -> Response<Full<Bytes>> {
    Log::new(Some(status_tx)).error(format!("NPM: could not build a response — {}", reason));
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(
            json!({ "error": crate::utils::WireFailure::Unavailable.prefixed_text() }).to_string(),
        )))
        .unwrap()
}

/// Process LLM action result and build HTTP response
/// Build an NPM response from one action result.
///
/// Returns `None` when the action is not an NPM response action, so the caller can
/// keep scanning. It used to return a 500 for anything it did not recognise, and the
/// caller returned unconditionally on the first result — so a model emitting the
/// documented `show_message` + NPM-response pair got a 500 and lost the real answer.
async fn process_npm_action_result(
    action_result: crate::llm::ActionResult,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Option<Response<Full<Bytes>>> {
    use crate::llm::ActionResult;

    match action_result {
        ActionResult::Custom { name, data } => {
            match name.as_str() {
                "npm_package_metadata" => {
                    // `.unwrap()` here used to panic the connection task, which
                    // tokio::spawn swallows: the server kept reporting Running while
                    // the client hung forever. A missing field is the model's mistake,
                    // not ours — answer it.
                    let Some(metadata) = data.get("metadata") else {
                        return Some(npm_server_error(
                            status_tx,
                            "npm_package_metadata carried no 'metadata' field",
                        ));
                    };

                    // FileOnly: the npm_package_metadata action's own log_template already
                    // reports "-> NPM package metadata" to the TUI at INFO.
                    Log::new(Some(status_tx)).debug("NPM package metadata response");
                    Some(
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/json")
                            .body(Full::new(Bytes::from(metadata.to_string())))
                            .unwrap(),
                    )
                }
                "npm_package_tarball" => {
                    let Some(tarball_data) = data.get("tarball_data").and_then(|v| v.as_str())
                    else {
                        return Some(npm_server_error(
                            status_tx,
                            "npm_package_tarball carried no 'tarball_data' string",
                        ));
                    };

                    // Fail closed on undecodable base64. This was
                    // `.unwrap_or_default()`, which turned a malformed answer into
                    // **HTTP 200 with a zero-byte body** — `npm install` then failed
                    // deep inside tar extraction with nothing pointing back here, and
                    // an empty package was indistinguishable from a real one. The
                    // action's own example made it likely rather than theoretical: it
                    // showed an elided `"H4sIAAAAAAAAA..."`, which does not decode.
                    let decoded =
                        match base64::engine::general_purpose::STANDARD.decode(tarball_data) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                return Some(npm_server_error(
                                    status_tx,
                                    &format!(
                                        "tarball_data is not valid base64 ({}). A tarball is \
                                     binary, so base64 is the only faithful form; send \
                                     the whole encoded string, never an abbreviation \
                                     ending in \"...\"",
                                        e
                                    ),
                                ));
                            }
                        };

                    if decoded.is_empty() {
                        return Some(npm_server_error(
                            status_tx,
                            "tarball_data decoded to zero bytes; an empty .tgz is not a \
                             package npm can install",
                        ));
                    }

                    // FileOnly: the npm_package_tarball action's own log_template already
                    // reports "-> NPM tarball (...)" to the TUI at INFO.
                    Log::new(Some(status_tx)).debug(format!(
                        "NPM package tarball response: {} bytes",
                        decoded.len()
                    ));
                    Some(
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/octet-stream")
                            .body(Full::new(Bytes::from(decoded)))
                            .unwrap(),
                    )
                }
                "npm_package_list" => {
                    let Some(packages) = data.get("packages") else {
                        return Some(npm_server_error(
                            status_tx,
                            "npm_package_list carried no 'packages' field",
                        ));
                    };

                    // FileOnly: the npm_package_list action's own log_template already
                    // reports "-> NPM package list" to the TUI at INFO.
                    Log::new(Some(status_tx)).debug("NPM package list response");
                    Some(
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/json")
                            .body(Full::new(Bytes::from(packages.to_string())))
                            .unwrap(),
                    )
                }
                "npm_package_search" => {
                    let Some(results) = data.get("results") else {
                        return Some(npm_server_error(
                            status_tx,
                            "npm_package_search carried no 'results' field",
                        ));
                    };

                    // FileOnly: the npm_package_search action's own log_template already
                    // reports "-> NPM search results" to the TUI at INFO.
                    Log::new(Some(status_tx)).debug("NPM package search response");
                    Some(
                        Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/json")
                            .body(Full::new(Bytes::from(results.to_string())))
                            .unwrap(),
                    )
                }
                "npm_error" => {
                    let error_message = data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error");
                    let status_code = data
                        .get("status_code")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(500) as u16;

                    // FileOnly: the npm_error action's own log_template already reports
                    // "-> NPM error {status_code}: {error}" to the TUI at INFO.
                    Log::new(Some(status_tx))
                        .debug(format!("NPM error: {} ({})", error_message, status_code));
                    let error_response = json!({
                        "error": error_message
                    });
                    Some(
                        Response::builder()
                            .status(
                                StatusCode::from_u16(status_code)
                                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                            )
                            .header("Content-Type", "application/json")
                            .body(Full::new(Bytes::from(error_response.to_string())))
                            .unwrap(),
                    )
                }
                _ => {
                    error!("Unknown NPM action: {}", name);
                    // Not an NPM action: let the caller keep scanning.
                    None
                }
            }
        }
        _ => {
            error!("Unexpected action result type for NPM request");
            // Not an NPM action: let the caller keep scanning.
            None
        }
    }
}
