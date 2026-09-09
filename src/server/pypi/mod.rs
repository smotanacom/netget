//! PyPI (Python Package Index) server implementation
//!
//! Implements PEP 503 Simple Repository API for serving Python packages.

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
use tracing::trace;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::PypiProtocol;
use crate::state::app_state::AppState;
use actions::PYPI_REQUEST_EVENT;

/// PyPI server that delegates package serving to LLM
pub struct PypiServer;

impl PypiServer {
    /// Spawn the PyPI server with integrated LLM actions
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
        Log::new(Some(&status_tx)).info(format!("PyPI server listening on {}", local_addr));

        let protocol = Arc::new(PypiProtocol::new());

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
                            "Accepted PyPI connection {} from {}",
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

                            // Create a service that handles requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_pypi_request_with_llm_actions(
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
                                Log::new(Some(&status_tx_clone))
                                    .error(format!("Error serving PyPI connection: {:?}", err));
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("PyPI connection {connection_id} closed"));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept PyPI connection: {}", e));
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

/// Build the 502 sent when model or handler output cannot be turned into a
/// valid HTTP response (bad status code, malformed header name, no answer at all).
///
/// The peer gets a category, never a diagnosis: what exactly was wrong with the action
/// output is netget's own business and goes to the log only.
/// See `crate::utils::wire_failure`.
fn bad_gateway() -> Response<Full<Bytes>> {
    Response::builder()
        .status(502)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(
            crate::utils::WireFailure::Unavailable.prefixed_text(),
        )))
        .expect("502 response with a literal body is always valid")
}

/// The 413 for a request body over [`MAX_REQUEST_BODY_BYTES`].
///
/// Static text: the peer learns the fact and the limit, never why the read failed.
fn payload_too_large() -> Response<Full<Bytes>> {
    Response::builder()
        .status(413)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(format!(
            "Payload Too Large: request bodies are limited to {} bytes\n",
            crate::server::http_common::handler::MAX_REQUEST_BODY_BYTES
        ))))
        .expect("413 response with a literal body is always valid")
}

/// Build the reply for an LLM/backend failure.
///
/// The two categories get distinct HTTP codes on purpose: pip retries a 503 with
/// `Retry-After` and records a 500 as a hard failure, so collapsing them would make a
/// transient overload look permanent. The body is a static category string — the error
/// itself is logged, never written to the socket.
fn llm_failure_response(failure: crate::utils::WireFailure) -> Response<Full<Bytes>> {
    let mut builder = Response::builder()
        .header("Content-Type", "text/plain; charset=utf-8")
        .status(if failure.is_overloaded() { 503 } else { 500 });
    if failure.is_overloaded() {
        builder = builder.header("Retry-After", "5");
    }
    builder
        .body(Full::new(Bytes::from(failure.prefixed_text())))
        .expect("failure response with a literal body is always valid")
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
/// moving. Without it a busy PyPI server draws every peer as idle having sent
/// and received nothing.
#[allow(clippy::too_many_arguments)]
async fn handle_pypi_request_with_llm_actions(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<PypiProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let bytes_received = approximate_request_bytes(&req);
    let response = handle_pypi_request_with_llm_actions_inner(
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

/// Handle a single PyPI request with integrated LLM actions
async fn handle_pypi_request_with_llm_actions_inner(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<PypiProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // TRACE: Handler invoked (FileOnly)
    Log::new(Some(&status_tx)).trace(format!(
        "🔍 PyPI handler called for connection {}",
        connection_id
    ));

    // Extract request details first for logging
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let path = req.uri().path().to_string();

    // Extract headers
    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(name.to_string(), value_str.to_string());
        }
    }

    // Read the body, bounded. `Incoming` has no default limit, and this body is
    // buffered whole *and* interpolated into the LLM prompt below, so an
    // unauthenticated POST of arbitrary length would allocate twice over — the
    // unbounded-upload shape. `Limited` errors as soon as the cap is passed rather
    // than after buffering, so an oversized request costs at most the cap.
    let body_bytes = match http_body_util::Limited::new(
        req.into_body(),
        crate::server::http_common::handler::MAX_REQUEST_BODY_BYTES,
    )
    .collect()
    .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            // Do NOT fall through to an empty body: a truncated request handed to the
            // model looks like a complete one, and the model would answer a request it
            // never saw. 413 is the honest reply, and it costs no LLM call.
            Log::new(Some(&status_tx)).warn(format!(
                "PyPI {} {} decision=refused_body_too_large (limit {} bytes) \u{2192} 413: {}",
                method,
                uri,
                crate::server::http_common::handler::MAX_REQUEST_BODY_BYTES,
                e
            ));
            return Ok(payload_too_large());
        }
    };

    // Determine request type based on path
    // PEP 503 project URLs end with '/', but pip and poetry both follow
    // redirects, so a client may also ask for /simple/<name> without one.
    let request_type = if path == "/" || path == "/simple" || path == "/simple/" {
        "list_packages"
    } else if path.starts_with("/simple/") {
        "list_files"
    } else if path.starts_with("/packages/") {
        "download_file"
    } else {
        "unknown"
    };

    // Extract package name if applicable
    let package_name = if request_type == "list_files" {
        path.trim_start_matches("/simple/")
            .trim_end_matches('/')
            .to_string()
    } else if request_type == "download_file" {
        path.trim_start_matches("/packages/")
            .split('/')
            .last()
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };

    // DEBUG: Log request summary (FileOnly)
    Log::new(Some(&status_tx)).debug(format!(
        "PyPI request: {} {} [{}] ({} bytes) from {:?}",
        method,
        uri,
        request_type,
        body_bytes.len(),
        connection_id
    ));

    // TRACE: Log full request details
    trace!("PyPI request headers:");
    for (name, value) in &headers {
        trace!("  {}: {}", name, value);
    }

    // Create PyPI request event (FileOnly)
    Log::new(Some(&status_tx)).trace(format!(
        "🔍 Creating PyPI event: path={}, request_type={}",
        path, request_type
    ));

    let body_text = String::from_utf8_lossy(&body_bytes);
    let event = Event::new(
        &PYPI_REQUEST_EVENT,
        serde_json::json!({
            "method": method,
            "uri": uri,
            "path": path,
            "headers": headers,
            "body": body_text,
            "request_type": request_type,
            "package_name": package_name,
        }),
    );

    Log::new(Some(&status_tx)).trace("🔍 Calling LLM for PyPI request");

    // Call LLM to generate PyPI response
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
            Log::new(Some(&status_tx)).debug(format!(
                "LLM PyPI response received, {} protocol results",
                execution_result.protocol_results.len()
            ));

            // Display messages
            for msg in execution_result.messages {
                let _ = status_tx.send(msg);
            }

            // Extract the PyPI response from the protocol results.
            //
            // There is deliberately no default here. This used to pre-set
            // `status_code = 200` with an empty body, so a model that answered with no
            // action at all — or a handler configured with `actions: []` — produced a
            // `200 OK` with zero bytes. To pip that reads as "the project exists and has
            // no distributions", i.e. silence became a successful answer. A no-answer is
            // a failure and must look like one on the wire.
            let mut status_code: Option<u16> = None;
            let mut response_headers = HashMap::new();
            let mut response_body: Vec<u8> = Vec::new();
            let mut body_set = false;
            let mut decode_failed = false;

            for protocol_result in execution_result.protocol_results {
                if let ActionResult::Output(output_data) = protocol_result {
                    // Parse the output as JSON containing PyPI response fields
                    if let Ok(json_value) =
                        serde_json::from_slice::<serde_json::Value>(&output_data)
                    {
                        if let Some(status) = json_value.get("status").and_then(|v| v.as_u64()) {
                            if (100..=599).contains(&status) {
                                status_code = Some(status as u16);
                            } else {
                                Log::new(Some(&status_tx)).error(format!(
                                    "PyPI response status {} is not a valid HTTP status code",
                                    status
                                ));
                                decode_failed = true;
                            }
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
                        // body_base64 is only ever set by the action executor,
                        // which has already validated the encoding.
                        if let Some(encoded) =
                            json_value.get("body_base64").and_then(|v| v.as_str())
                        {
                            use base64::Engine;
                            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                                Ok(decoded) => {
                                    response_body = decoded;
                                    body_set = true;
                                }
                                Err(e) => {
                                    // Serving 0 bytes here would hand pip a truncated
                                    // distribution that fails its hash check much later,
                                    // far from the cause. Refuse instead.
                                    Log::new(Some(&status_tx)).error(format!(
                                        "PyPI response body_base64 is not valid base64: {}",
                                        e
                                    ));
                                    decode_failed = true;
                                }
                            }
                        } else if let Some(body) = json_value.get("body").and_then(|v| v.as_str()) {
                            response_body = body.as_bytes().to_vec();
                            body_set = true;
                        }
                    }
                }
            }

            if decode_failed {
                // The model/handler answered, but the answer cannot be put on the wire.
                Log::new(Some(&status_tx)).error(format!(
                    "PyPI {} {} decision=fail_closed_bad_action → 502",
                    method, uri
                ));
                return Ok(bad_gateway());
            }

            // `send_pypi_response` always yields a status plus exactly one of
            // `body`/`body_base64` (see `execute_send_pypi_response`), so both being
            // present is precisely "a PyPI response was produced".
            let Some(status_code) = status_code.filter(|_| body_set) else {
                // Distinct from an LLM error (below) and from a model that deliberately
                // answered 404: nothing usable came back at all.
                Log::new(Some(&status_tx)).warn(format!(
                    "PyPI {} {} decision=fail_closed_no_action \
                     (no send_pypi_response in the answer) → 502",
                    method, uri
                ));
                return Ok(bad_gateway());
            };

            Log::new(Some(&status_tx)).info(format!(
                "PyPI {} {} decision=model_response → {} ({} bytes)",
                method,
                uri,
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
                    Log::new(Some(&status_tx)).error(format!(
                        "PyPI {} {} decision=fail_closed_bad_action: invalid response ({}) → 502",
                        method, uri, e
                    ));
                    Ok(bad_gateway())
                }
            }
        }
        Err(e) => {
            // The backend failed. The peer gets a category and a code it can act on; the
            // error itself — backend URL, model name, anyhow chain — goes to the log only.
            let failure = crate::utils::WireFailure::classify(&e);
            let code = if failure.is_overloaded() { 503 } else { 500 };
            Log::new(Some(&status_tx)).warn(format!(
                "PyPI {} {} decision=fail_closed_llm_error → {} ({:?}): {}",
                method, uri, code, failure, e
            ));
            Ok(llm_failure_response(failure))
        }
    }
}
