//! JSON-RPC 2.0 server implementation
//!
//! JSON-RPC runs over HTTP POST. The LLM controls all RPC method calls and responses.
//! Supports single requests, batch requests, and notifications.

pub mod actions;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::error;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::jsonrpc::actions::{JsonRpcProtocol, JSONRPC_METHOD_CALL_EVENT};
use crate::state::app_state::AppState;

/// JSON-RPC 2.0 standard error codes
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const INTERNAL_ERROR: i32 = -32603;

/// Backend saturated — transient, so the caller should back off and retry.
///
/// JSON-RPC 2.0 reserves -32000..=-32099 for implementation-defined server errors. Reporting
/// overload as -32603 tells the caller the server is broken when it is only busy, and the two
/// have to stay distinguishable for a client's retry policy to be right. `xmlrpc` and the MCP
/// server use the same code for the same reason.
const SERVER_BUSY: i32 = -32000;

/// Largest request body accepted, in bytes.
///
/// hyper imposes no limit of its own and the body was buffered whole, parsed into a
/// `serde_json::Value` and then pretty-printed into the trace log — so one client could grow
/// the process without bound.
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Largest batch accepted.
///
/// **Every batch member is a separate model call**, run sequentially on one held-open
/// connection, so batch length is a direct amplification factor on the LLM backend: at the
/// body cap above, a `{"jsonrpc":"2.0","method":"a","id":1}` member is about forty bytes, which
/// is a hundred thousand model calls from a single unauthenticated POST. The body cap alone
/// does not bound the expensive resource. 128 is far above any real JSON-RPC batch; a caller
/// that genuinely wants more should be behind a script or static handler, which costs no model
/// call at all.
const MAX_BATCH_LEN: usize = 128;

/// JSON-RPC 2.0 server that delegates to LLM
pub struct JsonRpcServer;

impl JsonRpcServer {
    /// Spawn the JSON-RPC server with integrated LLM actions
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
        Log::new(Some(&status_tx)).info(format!("JSON-RPC server listening on {}", local_addr));

        let protocol = Arc::new(JsonRpcProtocol::new());

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
                            "JSON-RPC connection {} from {}",
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

                            // Create a service that handles JSON-RPC requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_jsonrpc_request(
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
                                error!("Error serving JSON-RPC connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("JSON-RPC connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept JSON-RPC connection: {}", e));
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

/// Handle a single JSON-RPC request
async fn handle_jsonrpc_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<JsonRpcProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let log = Log::new(Some(&status_tx));

    log.debug(format!("JSON-RPC request: {} {}", method, uri.path()));

    // JSON-RPC requires POST method
    if method != Method::POST {
        return Ok(build_error_response(
            INVALID_REQUEST,
            "JSON-RPC requires POST method",
            None,
            None,
        ));
    }

    // Read request body, capped. hyper imposes no limit and the whole body is buffered,
    // parsed and then pretty-printed into the trace log below.
    let body_bytes = match http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            // Non-fatal: the client gets an error response (wire fallback). The peer learns
            // the limit, which is actionable; the codec's own message is not.
            tracing::warn!("JSON-RPC request body rejected (decision=reject_oversized): {e}");
            log.warn(format!("Failed to read request body: {}", e));
            return Ok(build_error_response(
                INVALID_REQUEST,
                &format!(
                    "Request body rejected (limit {} bytes)",
                    MAX_REQUEST_BODY_BYTES
                ),
                None,
                None,
            ));
        }
    };

    // Parse JSON
    let request_value: Value = match serde_json::from_slice(&body_bytes) {
        Ok(json) => json,
        Err(e) => {
            // Non-fatal: the client gets a Parse error response (wire fallback).
            log.warn(format!("Failed to parse JSON: {}", e));
            return Ok(build_error_response(PARSE_ERROR, "Parse error", None, None));
        }
    };

    // Full payload FileOnly: the jsonrpc_method_call event template renders the
    // method to the TUI.
    log.trace(format!(
        "JSON-RPC request body: {}",
        serde_json::to_string_pretty(&request_value).unwrap_or_default()
    ));

    // Check if it's a batch request (array) or single request (object)
    match request_value {
        Value::Array(requests) if !requests.is_empty() => {
            // Batch request
            if requests.len() > MAX_BATCH_LEN {
                tracing::warn!(
                    "JSON-RPC batch of {} rejected (decision=reject_oversized_batch, limit={})",
                    requests.len(),
                    MAX_BATCH_LEN
                );
                log.warn(format!(
                    "JSON-RPC batch of {} rejected (limit {})",
                    requests.len(),
                    MAX_BATCH_LEN
                ));
                return Ok(build_error_response(
                    INVALID_REQUEST,
                    &format!("Batch too large (limit {} requests)", MAX_BATCH_LEN),
                    None,
                    None,
                ));
            }

            log.debug(format!(
                "Processing batch JSON-RPC request with {} items",
                requests.len()
            ));

            let mut responses = Vec::new();
            for request in requests {
                if let Some(response) = process_single_request(
                    request,
                    connection_id,
                    &llm_client,
                    &app_state,
                    &status_tx,
                    &protocol,
                    server_id,
                )
                .await
                {
                    responses.push(response);
                }
            }

            // Spec §6: "If there are no Response objects contained within the
            // Response array as it is to be sent to the client, the server MUST NOT
            // return an empty Array and should return nothing at all." A batch of
            // nothing but notifications used to answer HTTP 200 with `[]`.
            if responses.is_empty() {
                return Ok(Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Full::new(Bytes::new()))
                    .unwrap());
            }

            // Return batch response
            let response_json = json!(responses);
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(response_json.to_string())))
                .unwrap())
        }
        Value::Object(_) => {
            // Single request
            if let Some(response) = process_single_request(
                request_value,
                connection_id,
                &llm_client,
                &app_state,
                &status_tx,
                &protocol,
                server_id,
            )
            .await
            {
                let response_json = response;
                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(response_json.to_string())))
                    .unwrap())
            } else {
                // Notification (no response)
                Ok(Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Full::new(Bytes::new()))
                    .unwrap())
            }
        }
        Value::Array(_) => {
            // Empty batch request
            Ok(build_error_response(
                INVALID_REQUEST,
                "Empty batch request",
                None,
                None,
            ))
        }
        _ => {
            // Invalid request type
            Ok(build_error_response(
                INVALID_REQUEST,
                "Request must be an object or array",
                None,
                None,
            ))
        }
    }
}

/// Process a single JSON-RPC request
/// Returns None for notifications (no id field)
async fn process_single_request(
    request: Value,
    connection_id: ConnectionId,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    protocol: &Arc<JsonRpcProtocol>,
    server_id: crate::state::ServerId,
) -> Option<Value> {
    let log = Log::new(Some(status_tx));
    // A batch member that is not an object is not a request at all. Spec §6
    // requires an Invalid Request response for each such member, with a null id;
    // it used to be dropped silently, so `[1,2,3]` produced an empty array.
    if !request.is_object() {
        return Some(json!({
            "jsonrpc": "2.0",
            "error": {
                "code": INVALID_REQUEST,
                "message": "Request must be a JSON object"
            },
            "id": Value::Null
        }));
    }

    // Extract fields
    let jsonrpc_version = request.get("jsonrpc").and_then(|v| v.as_str());
    let method = request.get("method").and_then(|v| v.as_str());
    let params = request.get("params").cloned();
    let id = request.get("id").cloned();

    // Spec §4: "A Notification is a Request object without an 'id' member."
    // An explicit `"id": null` is a Request (discouraged, but valid) and must be
    // answered with `"id": null`. Only the absence of the member makes it a
    // notification.
    let is_notification = id.is_none();

    // Validate JSON-RPC version
    if jsonrpc_version != Some("2.0") {
        if !is_notification {
            return Some(json!({
                "jsonrpc": "2.0",
                "error": {
                    "code": INVALID_REQUEST,
                    "message": "Invalid JSON-RPC version, must be '2.0'"
                },
                "id": id
            }));
        } else {
            return None; // Notifications never return errors
        }
    }

    // Validate method
    let method = match method {
        Some(m) if !m.is_empty() => m,
        _ => {
            if !is_notification {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": INVALID_REQUEST,
                        "message": "Missing or invalid 'method' field"
                    },
                    "id": id
                }));
            } else {
                return None; // Notifications never return errors
            }
        }
    };

    log.debug(format!(
        "JSON-RPC method call: method={}, is_notification={}",
        method, is_notification
    ));

    // Call LLM with method details
    let response_value = match call_llm_for_method(
        method,
        params.as_ref(),
        id.clone(),
        llm_client,
        app_state,
        status_tx,
        protocol,
        connection_id,
        server_id,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            // Three outcomes an operator has to tell apart, and only a `decision=` tag can
            // carry the difference here: the backend was saturated (transient), the backend
            // erred (not), and — in `call_llm_for_method` below — the handler ran but answered
            // with nothing usable. A notification is answered with silence whichever it was,
            // so without these tags a swallowed failure left no trace at all.
            let failure = crate::utils::WireFailure::classify(&e);
            let (code, decision) = if failure.is_overloaded() {
                (SERVER_BUSY, "fail_closed_llm_overloaded")
            } else {
                (INTERNAL_ERROR, "fail_closed_llm_error")
            };
            tracing::error!(
                "JSON-RPC {} failed (decision={decision}, code={code}, notification={is_notification}): {e:#}",
                method
            );
            log.warn(format!(
                "JSON-RPC {} failed (decision={}): {}",
                method, decision, e
            ));
            if !is_notification {
                // Category only — never the error text. See `crate::utils::wire_failure`.
                return Some(json!({
                    "jsonrpc": "2.0",
                    "error": {
                        "code": code,
                        "message": failure.text(),
                        "data": {"retryable": failure.is_overloaded()}
                    },
                    "id": id
                }));
            } else {
                return None; // Notifications never return errors
            }
        }
    };

    // Return response (or None for notifications)
    if !is_notification {
        Some(response_value)
    } else {
        log.trace("Notification processed, no response sent");
        None
    }
}

/// Call LLM to handle the JSON-RPC method call
async fn call_llm_for_method(
    method: &str,
    params: Option<&Value>,
    request_id: Option<Value>,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    protocol: &Arc<JsonRpcProtocol>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
) -> anyhow::Result<Value> {
    // Create JSON-RPC method call event.
    //
    // `is_notification` is explicit because `id` cannot carry the distinction: a
    // missing id and an explicit `"id": null` both serialise to null here, yet the
    // first must not be answered and the second must be. Without this field a
    // script handler had no way to tell them apart.
    let event_data = if let Some(params_val) = params {
        json!({
            "method": method,
            "params": params_val,
            "id": request_id,
            "is_notification": request_id.is_none(),
        })
    } else {
        json!({
            "method": method,
            "id": request_id,
            "is_notification": request_id.is_none(),
        })
    };

    let event = Event::new(&JSONRPC_METHOD_CALL_EVENT, event_data);
    let log = Log::new(Some(status_tx));

    log.debug(format!("Calling LLM for JSON-RPC method: {}", method));

    // Call LLM with event
    let llm_result = call_llm(
        llm_client,
        app_state,
        server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await?;

    log.trace(format!(
        "LLM actions for JSON-RPC: {:?}",
        llm_result.raw_actions.len()
    ));

    // Pick the response out of everything the handler produced.
    //
    // call_llm has already executed every action, so we read the results rather
    // than re-executing (which used to render each log template twice and record
    // the pre-id-fill action in the access log). Crucially we *scan* instead of
    // taking raw_actions.first(): raw_actions includes common actions, so a
    // perfectly reasonable response that leads with show_message or update_memory
    // used to be rejected as a "non-JSON-RPC action" and turned into -32603. The
    // protocol's own documentation and its notification test both used that shape.
    let response = llm_result
        .protocol_results
        .iter()
        .find_map(|result| collect_jsonrpc_response(result));

    let Some(mut response) = response else {
        // The handler ran without erroring and produced nothing usable. Distinct from a
        // backend failure (logged as `fail_closed_*` by the caller) and from a handler that
        // deliberately chose `jsonrpc_error` — which reaches the wire as the code it named.
        // This used to be silent on both sinks, so an instruction the model could not follow
        // looked identical to a broken backend.
        tracing::warn!(
            "JSON-RPC {} answered -32603 (decision=model_no_answer): handler produced neither \
             jsonrpc_success nor jsonrpc_error",
            method
        );
        log.warn(format!(
            "JSON-RPC {} (decision=model_no_answer): no jsonrpc_success or jsonrpc_error",
            method
        ));
        return Ok(json!({
            "jsonrpc": "2.0",
            "error": {
                "code": INTERNAL_ERROR,
                "message": "Handler did not produce a jsonrpc_success or jsonrpc_error action"
            },
            "id": request_id.unwrap_or(Value::Null)
        }));
    };

    // The correlation id belongs to the request, not to the model. JSON-RPC 2.0
    // §5 requires it to equal the request id, preserving type (a string id must
    // come back as a string), so it is overwritten unconditionally: a handler that
    // invents an id would otherwise produce a reply the client cannot match, and
    // over keep-alive that failure is silent.
    if let Some(obj) = response.as_object_mut() {
        obj.insert("id".to_string(), request_id.clone().unwrap_or(Value::Null));
        obj.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    }

    Ok(response)
}

/// Extract a JSON-RPC response object from one action result, if it is one.
/// Handles `Multiple` so a nested result is not silently dropped.
fn collect_jsonrpc_response(result: &ActionResult) -> Option<Value> {
    match result {
        ActionResult::Custom { name, data } if name == "jsonrpc_response" => Some(data.clone()),
        ActionResult::Multiple(inner) => inner.iter().find_map(collect_jsonrpc_response),
        _ => None,
    }
}

/// Build a JSON-RPC error response
//
// `track_method_call` used to sit here, maintaining a ten-entry `recent_methods` ring inside
// the connection's `protocol_info`. Nothing in the tree read it — not the dashboard, which
// reads `protocol_info` only through IMAP-specific accessors, not the MCP surface, not a test
// — and it cost a write lock on the one global `AppState` `RwLock` on every single request,
// plus a clone-and-reparse of the whole vector. It is deleted rather than left as a
// throughput cost with no reader; the method name is already in the access log and in the
// event's own log template.
fn build_error_response(
    code: i32,
    message: &str,
    data: Option<Value>,
    id: Option<Value>,
) -> Response<Full<Bytes>> {
    let mut error = json!({
        "code": code,
        "message": message
    });

    if let Some(data_val) = data {
        error
            .as_object_mut()
            .unwrap()
            .insert("data".to_string(), data_val);
    }

    let response = json!({
        "jsonrpc": "2.0",
        "error": error,
        "id": id.unwrap_or(Value::Null)
    });

    Response::builder()
        .status(StatusCode::OK) // JSON-RPC always returns 200 OK
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(response.to_string())))
        .unwrap()
}
