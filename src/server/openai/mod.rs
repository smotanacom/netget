//! OpenAI-compatible API server implementation
//!
//! OpenAI API runs over HTTP. The LLM uses Ollama to generate chat completions
//! and return them in OpenAI-compatible format.

pub mod actions;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, error};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::openai::actions::{OpenAiProtocol, OPENAI_REQUEST_EVENT};
use crate::state::app_state::AppState;

/// OpenAI-compatible API server that delegates to LLM/Ollama
pub struct OpenAiServer;

impl OpenAiServer {
    /// Spawn the OpenAI API server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        _send_first: bool,
        server_id: crate::state::ServerId,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("OpenAI API server listening on {}", local_addr));

        let protocol = Arc::new(OpenAiProtocol::new());

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
                            "OpenAI API connection {} from {}",
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

                            // Create a service that handles OpenAI API requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_openai_request(
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
                                error!("Error serving OpenAI API connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("OpenAI API connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept OpenAI API connection: {}", e));
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
/// Same value and same reasoning as `http_common::handler::MAX_REQUEST_BODY_BYTES`, declared
/// here rather than imported because the `openai` feature does not pull in `http`, so that
/// module is configured out of an `--features openai` build.
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// An OpenAI-shaped `{"error": {...}}` body at the given status.
///
/// `message` is always a fixed string or a `WireFailure` category — never an error's own
/// text. A peer gets the category; the log gets the error.
fn error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
    code: &str,
) -> Response<Full<Bytes>> {
    let body = json!({
        "error": { "message": message, "type": error_type, "code": code }
    })
    .to_string();

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap_or_else(|_| {
            // Never fall back to `Response::new`, which is a 200: that would turn a refusal
            // into a success.
            let mut response = Response::new(Full::new(Bytes::from(body)));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}

/// Handle a single OpenAI API request with LLM actions
async fn handle_openai_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OpenAiProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().to_string();
    let uri = req.uri().clone();
    let path = uri.path().to_string();

    Log::new(Some(&status_tx)).debug(format!("OpenAI API request: {} {}", method, path));

    // Read the request body, bounded. `Incoming` has no default limit, so `req.collect()`
    // buffered whatever the peer chose to send — and this endpoint is unauthenticated, so a
    // single `POST /v1/chat/completions` with an endless chunked body was enough to walk the
    // process out of memory. `Limited` errors as soon as the cap is passed, so an oversized
    // upload costs at most the cap rather than all of it.
    //
    // The cap is small because the body is handed to the model as prompt text: a model
    // cannot read 8 MiB, so every byte past that is cost without benefit. 413 rather than an
    // empty body, because a truncated body looks complete to the model and it would answer a
    // request it never saw.
    let limited = http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES);
    let body_bytes = match limited.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!(
                "OpenAI {} {} decision=fail_closed_body_too_large: {} (limit {} bytes)",
                method, path, e, MAX_REQUEST_BODY_BYTES
            ));
            return Ok(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large",
                "invalid_request_error",
                "request_too_large",
            ));
        }
    };

    let body_text = String::from_utf8_lossy(&body_bytes);

    // Create OpenAI request event
    let event = Event::new(
        &OPENAI_REQUEST_EVENT,
        json!({
            "method": method,
            "path": path,
            "body": if body_text.is_empty() { "" } else { body_text.as_ref() }
        }),
    );

    // Call LLM to generate OpenAI response
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
            debug!("LLM OpenAI response received");

            // Display messages
            for msg in execution_result.messages {
                let _ = status_tx.send(msg);
            }

            // Build HTTP response from action results
            build_openai_response(
                execution_result.protocol_results,
                &method,
                &path,
                &status_tx,
            )
        }
        Err(e) => {
            // The peer gets a category and the log gets the error. `Overloaded` is
            // transient, so it is a 503 and a client backs off; anything else is a 500 so it
            // is not retried forever. The connection itself continues.
            let failure = crate::utils::WireFailure::classify(&e);
            let status = if failure.is_overloaded() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Log::new(Some(&status_tx)).warn(format!(
                "OpenAI {} {} decision=fail_closed_llm_error category={}: {}",
                method,
                path,
                if failure.is_overloaded() {
                    "overloaded"
                } else {
                    "unavailable"
                },
                e
            ));

            Ok(error_response(
                status,
                failure.text(),
                "server_error",
                "internal_error",
            ))
        }
    }
}

/// Build HTTP response from OpenAI action results
fn build_openai_response(
    protocol_results: Vec<ActionResult>,
    method: &str,
    path: &str,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Find openai_response result
    for result in &protocol_results {
        if let ActionResult::Custom { name, data } = result {
            if name == "openai_response" {
                // `as u16` wrapped. The executors bound `status` to 100-599 before it gets
                // here, so nothing can reach this with 65736 today — but the narrowing sat
                // one careless executor away from turning a refusal into a 200, which is the
                // defect this protocol's `status_range_test` exists for. Read it without
                // narrowing, and treat anything unusable as a server error rather than as
                // the 200 the old default supplied.
                let status = match data.get("status").and_then(|v| v.as_u64()) {
                    None => 200,
                    Some(code) => u16::try_from(code)
                        .ok()
                        .filter(|code| (100..=599).contains(code))
                        .unwrap_or_else(|| {
                            error!(
                                "OpenAI executor produced status {} which is not an HTTP \
                                 status; answering 500",
                                code
                            );
                            500
                        }),
                };

                let headers = data
                    .get("headers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();

                let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("{}");

                Log::new(Some(status_tx))
                    .debug(format!("OpenAI {} {} -> {}", method, path, status));

                // Every header name and value here came out of an action's data, so this
                // must not end in `.unwrap()`: a name with a space in it, or a value
                // carrying CR/LF (a response-splitting attempt), makes the builder return
                // Err and would panic the connection task. Drop the individual bad header
                // and keep the response.
                let mut response_builder = Response::builder().status(status);
                for header in headers {
                    if let (Some(name_val), Some(value_val)) = (
                        header.get(0).and_then(|v| v.as_str()),
                        header.get(1).and_then(|v| v.as_str()),
                    ) {
                        let candidate = Response::builder().header(name_val, value_val);
                        if candidate.headers_ref().is_some() {
                            response_builder = response_builder.header(name_val, value_val);
                        } else {
                            error!("OpenAI dropping unusable response header {:?}", name_val);
                        }
                    }
                }

                return Ok(response_builder
                    .body(Full::new(Bytes::from(body.to_string())))
                    .unwrap_or_else(|e| {
                        error!("OpenAI could not build the response ({e}); answering 500");
                        error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            crate::utils::WireFailure::Unavailable.text(),
                            "server_error",
                            "internal_error",
                        )
                    }));
            }
        }
    }

    // The model was consulted and produced no response action. "LLM did not return valid
    // response" named netget's own internals on a stranger's terminal; the peer gets a
    // category and the reason stays in the log.
    Log::new(Some(status_tx)).warn(format!(
        "OpenAI {} {} decision=fail_closed_no_action: no openai_response action in the result",
        method, path
    ));

    Ok(error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        crate::utils::WireFailure::Unavailable.text(),
        "server_error",
        "internal_error",
    ))
}
