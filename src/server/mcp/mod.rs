//! MCP (Model Context Protocol) server implementation
//!
//! This module implements an MCP server that allows LLM to control all server capabilities.
//! MCP is built on JSON-RPC 2.0 and provides a standardized way for LLM applications
//! to access external resources, tools, and prompts.
//!
//! Key features:
//! - JSON-RPC 2.0 over HTTP/SSE transport
//! - Full LLM control over resources, tools, and prompts
//! - Session-based state management
//! - Three-phase initialization (initialize → response → initialized)
//! - Support for resource subscriptions, tool execution, and prompt templates

pub mod actions;
pub mod jsonrpc;

use anyhow::Result;
use axum::{
    extract::{Json, State as AxumState},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use jsonrpc::{ErrorCode, JsonRpcError, JsonRpcMessage, JsonRpcResponse};
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use crate::console_error;
#[cfg(feature = "mcp")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "mcp")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "mcp")]
use crate::logging::emit::Log;
#[cfg(feature = "mcp")]
use crate::protocol::Event;
#[cfg(feature = "mcp")]
use crate::server::connection::ConnectionId;
#[cfg(feature = "mcp")]
use crate::server::McpProtocol;
#[cfg(feature = "mcp")]
use crate::state::app_state::AppState;
#[cfg(feature = "mcp")]
use crate::state::server::{ConnectionStatus, ProtocolConnectionInfo, ServerId};
#[cfg(feature = "mcp")]
use actions::{
    MCP_INITIALIZE_EVENT, MCP_PROMPTS_GET_EVENT, MCP_PROMPTS_LIST_EVENT, MCP_RESOURCES_LIST_EVENT,
    MCP_RESOURCES_READ_EVENT, MCP_TOOLS_CALL_EVENT, MCP_TOOLS_LIST_EVENT,
};
#[cfg(feature = "mcp")]
use jsonrpc::RequestId;

/// MCP server shared state
#[derive(Clone)]
pub struct McpServerState {
    /// LLM client for generating responses
    pub llm_client: OllamaClient,
    /// Application state
    pub app_state: Arc<AppState>,
    /// Status message sender
    pub status_tx: mpsc::UnboundedSender<String>,
    /// Server ID for tracking connections
    pub server_id: ServerId,
    /// Protocol implementation
    pub protocol: Arc<McpProtocol>,
    /// Local address the server is bound to
    pub local_addr: SocketAddr,
}

/// Largest slice of a request echoed onto the status channel.
///
/// The whole request used to be serialized onto `status_tx` on every call. That channel is
/// unbounded with no backpressure (see the root CLAUDE.md), so a client posting bodies at
/// axum's 2 MiB default limit could enqueue faster than the TUI drains.
#[cfg(feature = "mcp")]
const MAX_TRACE_BYTES: usize = 4096;

/// MCP revisions this server will echo back in an `initialize` reply.
#[cfg(feature = "mcp")]
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Implementation-defined JSON-RPC error code for "the backend is at capacity, retry".
///
/// JSON-RPC 2.0 reserves -32000..=-32099 for server-defined errors. Reporting overload as
/// -32603 (InternalError) would tell the caller the server is broken when it is only busy.
#[cfg(feature = "mcp")]
const MCP_SERVER_BUSY_CODE: i32 = -32000;

/// Offered when the client asks for a revision not in the list above.
#[cfg(feature = "mcp")]
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// Turn an `mcp_error_response` action result into the JSON-RPC error it describes.
///
/// `mcp_error_response` is offered to the handler on every MCP event. Nothing used to consume
/// its result, so the chosen `code` and `message` were dropped and the caller received either
/// a generic `-32603` or - worse - a *success* reply such as `{"tools": []}`.
#[cfg(feature = "mcp")]
fn mcp_error_from_action(data: &Value) -> JsonRpcError {
    // i64 rather than i32: JSON-RPC codes are small, but `as i32` would wrap a large number
    // into a valid-looking code. Out-of-range values become InternalError.
    let code = data
        .get("code")
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok())
        .unwrap_or(ErrorCode::InternalError as i32);

    JsonRpcError {
        code,
        message: data
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Internal error")
            .to_string(),
        data: data.get("data").cloned(),
    }
}

/// Turn a failed LLM call into the JSON-RPC error the caller gets back.
///
/// Every MCP method routes its failure through here so the shape is identical
/// across all seven, and so the caller always receives a response - `handle_jsonrpc`
/// re-attaches the request `id` to whatever this returns, which is what lets the
/// client match the failure to its request instead of waiting on a reply that
/// never comes.
///
/// Overload is reported separately because it is transient: `-32603` says the
/// server is broken, while a server-defined `-32000` with a retry hint says it is
/// merely full. The JSON-RPC 2.0 spec reserves -32000..=-32099 for exactly this.
///
/// The peer-visible `message` comes from [`WireFailure`], which returns `&'static str`
/// precisely so nothing derived from the error can reach the wire: the backend URL, the
/// model name and the `anyhow` context chain go to the log and the status stream only.
///
/// The log line carries a `decision=` tag so the three outcomes an operator has to tell
/// apart stay greppable: `decision=fail_closed_llm_error_overloaded`,
/// `decision=fail_closed_llm_error_unavailable` (here), `decision=model_reject`
/// (the handler chose `mcp_error`) and `decision=model_no_answer` (the handler ran but
/// produced no usable action).
#[cfg(feature = "mcp")]
fn llm_failure_error(state: &McpServerState, method: &str, e: anyhow::Error) -> JsonRpcError {
    let failure = crate::utils::WireFailure::classify(&e);
    let decision = if failure.is_overloaded() {
        "fail_closed_llm_error_overloaded"
    } else {
        "fail_closed_llm_error_unavailable"
    };
    error!("MCP {} decision={}: {}", method, decision, e);
    Log::new(Some(&state.status_tx)).error(format!("MCP {} decision={}: {}", method, decision, e));

    // `data` reaches the client, so it carries the retry hint and nothing else - the error
    // itself was logged on both channels above. See `crate::utils::wire_failure`.
    if failure.is_overloaded() {
        return JsonRpcError {
            code: MCP_SERVER_BUSY_CODE,
            message: failure.text().to_string(),
            data: Some(serde_json::json!({"retryable": true})),
        };
    }

    JsonRpcError {
        code: ErrorCode::InternalError as i32,
        message: failure.text().to_string(),
        data: Some(serde_json::json!({"retryable": false})),
    }
}

/// Log that the handler explicitly chose to fail this request (`mcp_error`).
///
/// Distinct from a no-answer and from a backend failure: this one the model meant.
#[cfg(feature = "mcp")]
fn log_model_reject(state: &McpServerState, method: &str) {
    warn!(
        "MCP {} decision=model_reject: handler returned mcp_error",
        method
    );
    Log::new(Some(&state.status_tx)).warn(format!(
        "MCP {} decision=model_reject (handler returned mcp_error)",
        method
    ));
}

/// Log that the handler ran without erroring but produced no usable action, so the
/// hardcoded default below is what the caller receives.
#[cfg(feature = "mcp")]
fn log_model_no_answer(state: &McpServerState, method: &str, fallback: &str) {
    warn!(
        "MCP {} decision=model_no_answer: replying with {}",
        method, fallback
    );
    Log::new(Some(&state.status_tx)).warn(format!(
        "MCP {} decision=model_no_answer (replying with {})",
        method, fallback
    ));
}

/// Recover a request id from a payload that failed to parse as a JSON-RPC request.
///
/// JSON-RPC 2.0 requires the id to be echoed whenever it can be determined. The parse-failure
/// path used to pass `None` unconditionally, so a request whose `jsonrpc` field was missing or
/// whose `method` was not a string came back with `"id": null` even though the id was sitting
/// in the payload, leaving the client unable to match the error to its request.
#[cfg(feature = "mcp")]
fn recover_request_id(payload: &Value) -> Option<RequestId> {
    match payload.get("id")? {
        Value::String(s) => Some(RequestId::String(s.clone())),
        Value::Number(n) => n.as_i64().map(RequestId::Number),
        _ => None,
    }
}

/// MCP server that handles Model Context Protocol over HTTP
pub struct McpServer;

#[cfg(feature = "mcp")]
impl McpServer {
    /// Spawn MCP server with Axum on HTTP (default port 8000)
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
    ) -> Result<SocketAddr> {
        let listener = tokio::net::TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        info!("MCP server (JSON-RPC 2.0) listening on {}", local_addr);
        Log::new(Some(&status_tx)).info(format!("MCP server listening on {}", local_addr));

        let protocol = Arc::new(McpProtocol::new());

        let task_registrar = app_state.clone();
        let server_state = McpServerState {
            llm_client,
            app_state,
            status_tx: status_tx.clone(),
            server_id,
            protocol,
            local_addr,
        };

        // Build Axum router
        let app = Router::new()
            .route("/", post(handle_jsonrpc))
            .with_state(server_state);

        // Spawn server
        let accept_handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                console_error!(status_tx, "MCP server error: {}", e);
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

/// Handle incoming JSON-RPC 2.0 requests
#[cfg(feature = "mcp")]
async fn handle_jsonrpc(
    AxumState(state): AxumState<McpServerState>,
    Json(payload): Json<Value>,
) -> Response {
    trace!(
        "MCP received JSON-RPC request: {}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
    );

    let mut trace_body = serde_json::to_string(&payload).unwrap_or_default();
    if trace_body.len() > MAX_TRACE_BYTES {
        // Truncate on a char boundary: byte-slicing LLM- or client-supplied JSON panics on
        // multi-byte UTF-8 at the cut point.
        let cut = trace_body
            .char_indices()
            .take_while(|(i, _)| *i <= MAX_TRACE_BYTES)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        trace_body.truncate(cut);
        trace_body.push_str("… (truncated)");
    }
    Log::new(Some(&state.status_tx)).trace(format!("MCP received: {}", trace_body));

    // Parse JSON-RPC message
    let message = match JsonRpcMessage::from_value(payload.clone()) {
        Ok(msg) => msg,
        Err(e) => {
            error!("Failed to parse JSON-RPC message: {:?}", e);
            let response = JsonRpcResponse::error(recover_request_id(&payload), e);
            return Json(response).into_response();
        }
    };

    // Handle based on message type
    match message {
        JsonRpcMessage::Request(req) => {
            let request_id = req.id.clone();
            let method = req.method.clone();

            debug!("MCP request: method={}, id={:?}", method, request_id);

            // Route to appropriate handler
            let result = match method.as_str() {
                "initialize" => handle_initialize(&state, req.params).await,
                "ping" => handle_ping(),
                "resources/list" => handle_resources_list(&state).await,
                "resources/read" => handle_resources_read(&state, req.params).await,
                "resources/subscribe" => handle_resources_subscribe(&state, req.params).await,
                "resources/unsubscribe" => handle_resources_unsubscribe(&state, req.params).await,
                "resources/templates/list" => handle_resources_templates_list(&state).await,
                "tools/list" => handle_tools_list(&state).await,
                "tools/call" => handle_tools_call(&state, req.params).await,
                "prompts/list" => handle_prompts_list(&state).await,
                "prompts/get" => handle_prompts_get(&state, req.params).await,
                "logging/setLevel" => handle_logging_set_level(&state, req.params).await,
                "completion/complete" => handle_completion(&state, req.params).await,
                _ => Err(JsonRpcError::new(ErrorCode::MethodNotFound)),
            };

            let response = match result {
                Ok(value) => {
                    trace!(
                        "MCP response success: {}",
                        serde_json::to_string_pretty(&value).unwrap_or_default()
                    );
                    JsonRpcResponse::success(request_id, value)
                }
                Err(e) => {
                    error!("MCP error: code={}, message={}", e.code, e.message);
                    Log::new(Some(&state.status_tx)).error(format!("MCP error: {}", e.message));
                    JsonRpcResponse::error(request_id, e)
                }
            };

            Json(response).into_response()
        }
        JsonRpcMessage::Notification(notif) => {
            let method = notif.method.clone();
            debug!("MCP notification: method={}", method);

            // Handle notifications (no response)
            match method.as_str() {
                "notifications/initialized" => {
                    handle_initialized(&state).await;
                }
                "notifications/cancelled" => {
                    handle_cancelled(&state, notif.params).await;
                }
                "notifications/progress" => {
                    handle_progress(&state, notif.params).await;
                }
                _ => {
                    debug!("Unknown MCP notification: {}", method);
                }
            }

            // Notifications don't return responses
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Handle initialize request - LLM declares capabilities
#[cfg(feature = "mcp")]
async fn handle_initialize(
    state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let connection_id = ConnectionId::new(state.app_state.get_next_unified_id().await);
    let result = handle_initialize_inner(state, params, connection_id).await;
    // Close on every exit path, including the error ones.
    state
        .app_state
        .close_connection_on_server(state.server_id, connection_id)
        .await;
    result
}

#[cfg(feature = "mcp")]
async fn handle_initialize_inner(
    state: &McpServerState,
    params: Option<Value>,
    connection_id: ConnectionId,
) -> Result<Value, JsonRpcError> {
    info!("MCP initialize request");

    // Extract client info from params
    let client_info = params
        .as_ref()
        .and_then(|p| p.get("clientInfo"))
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    debug!("MCP client: {}", client_info);
    let _ = state
        .status_tx
        .send(format!("→ MCP client initializing: {}", client_info));

    // Record the initialize exchange as a connection so it shows up in the TUI and MCP
    // connection lists. It is marked closed at the end of this function: MCP here is
    // request-scoped HTTP POST with no persistent connection, and leaving every entry Active
    // meant an unauthenticated client could grow AppState without bound by repeating
    // `initialize`.
    //
    // No session record is created. `McpSession` held initialized/capabilities/subscriptions
    // plus tools/resources/prompts maps; the map it lived in was write-only - inserted here
    // and read nowhere in the tree - and every mutator on it (`mark_initialized`,
    // `subscribe`, `register_tool`, …) had zero call sites. It was both an unbounded leak and
    // a protocol-level store of tools/resources/prompts, which the no-storage rule forbids.
    // It is gone rather than left half-built: nothing consumed it, so nothing regresses.
    state
        .app_state
        .add_connection_to_server(
            state.server_id,
            crate::state::ConnectionState {
                id: connection_id,
                remote_addr: state.local_addr, // HTTP POST carries no peer addr here
                local_addr: state.local_addr,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: std::time::Instant::now(),
                status: ConnectionStatus::Active,
                status_changed_at: std::time::Instant::now(),
                protocol_info: ProtocolConnectionInfo::empty(),
            },
        )
        .await;

    let requested_version = params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    // Create event for LLM
    let event = Event::new(
        &MCP_INITIALIZE_EVENT,
        serde_json::json!({
            "method": "initialize",
            "client_info": client_info,
            "protocol_version": requested_version,
            "capabilities": params.as_ref().and_then(|p| p.get("capabilities")),
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    Log::new(Some(&state.status_tx)).debug("MCP calling LLM for initialize request");

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "initialize", e));
        }
    };

    // Display messages from LLM
    let log = Log::new(Some(&state.status_tx));
    for message in &execution_result.messages {
        log.info(format!("{}", message));
    }

    log.debug(format!(
        "MCP got {} protocol results",
        execution_result.protocol_results.len()
    ));

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "initialize");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_initialize" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default response if the handler does not provide one.
    //
    // The version is negotiated rather than hardcoded: MCP says the server echoes the client's
    // requested version if it can speak it, and otherwise offers its own. This used to answer
    // "2024-11-05" unconditionally, which tells a client on a newer revision that its request
    // was honored when it was not.
    log_model_no_answer(state, "initialize", "the negotiated protocol version");
    let agreed_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested_version.as_str()) {
        requested_version.as_str()
    } else {
        DEFAULT_PROTOCOL_VERSION
    };

    Ok(serde_json::json!({
        "protocolVersion": agreed_version,
        "capabilities": {
            "resources": {},
            "tools": {},
            "prompts": {}
        },
        "serverInfo": {
            "name": "netget-mcp",
            "version": "0.1.0"
        }
    }))
}

/// Handle ping request - simple health check
#[cfg(feature = "mcp")]
fn handle_ping() -> Result<Value, JsonRpcError> {
    Ok(serde_json::json!({}))
}

/// Handle resources/list request - LLM returns available resources
#[cfg(feature = "mcp")]
async fn handle_resources_list(state: &McpServerState) -> Result<Value, JsonRpcError> {
    Log::new(Some(&state.status_tx)).debug("MCP resources/list request");

    // Create event for LLM
    let event = Event::new(
        &MCP_RESOURCES_LIST_EVENT,
        serde_json::json!({
            "method": "resources/list",
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "resources/list", e));
        }
    };

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "resources/list");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_resources_list" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default: empty resources list
    log_model_no_answer(state, "resources/list", "an empty resources list");
    Ok(serde_json::json!({"resources": []}))
}

/// Handle resources/read request - LLM returns resource content
#[cfg(feature = "mcp")]
async fn handle_resources_read(
    state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let uri = params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| JsonRpcError::new(ErrorCode::InvalidParams))?;

    Log::new(Some(&state.status_tx)).debug(format!("MCP resources/read: {}", uri));

    // Create event for LLM
    let event = Event::new(
        &MCP_RESOURCES_READ_EVENT,
        serde_json::json!({
            "method": "resources/read",
            "uri": uri,
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "resources/read", e));
        }
    };

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "resources/read");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_resources_read" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default: resource not found
    log_model_no_answer(state, "resources/read", "a JSON-RPC error");
    Err(JsonRpcError::custom(
        ErrorCode::InternalError,
        format!("Resource not found: {}", uri),
    ))
}

/// Handle resources/subscribe request
#[cfg(feature = "mcp")]
async fn handle_resources_subscribe(
    _state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let uri = params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| JsonRpcError::new(ErrorCode::InvalidParams))?;

    debug!("MCP resources/subscribe: uri={}", uri);

    // TODO: Add LLM integration for subscription management
    Ok(serde_json::json!({}))
}

/// Handle resources/unsubscribe request
#[cfg(feature = "mcp")]
async fn handle_resources_unsubscribe(
    _state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let uri = params
        .as_ref()
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| JsonRpcError::new(ErrorCode::InvalidParams))?;

    debug!("MCP resources/unsubscribe: uri={}", uri);
    Ok(serde_json::json!({}))
}

/// Handle resources/templates/list request
#[cfg(feature = "mcp")]
async fn handle_resources_templates_list(_state: &McpServerState) -> Result<Value, JsonRpcError> {
    debug!("MCP resources/templates/list");

    // TODO: Add LLM integration
    Ok(serde_json::json!({
        "resourceTemplates": []
    }))
}

/// Handle tools/list request - LLM returns available tools
#[cfg(feature = "mcp")]
async fn handle_tools_list(state: &McpServerState) -> Result<Value, JsonRpcError> {
    Log::new(Some(&state.status_tx)).debug("MCP tools/list request");

    // Create event for LLM
    let event = Event::new(
        &MCP_TOOLS_LIST_EVENT,
        serde_json::json!({
            "method": "tools/list",
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "tools/list", e));
        }
    };

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "tools/list");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_tools_list" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default: empty tools list
    log_model_no_answer(state, "tools/list", "an empty tools list");
    Ok(serde_json::json!({"tools": []}))
}

/// Handle tools/call request - LLM executes tool
#[cfg(feature = "mcp")]
async fn handle_tools_call(
    state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let tool_name = params
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or_else(|| JsonRpcError::new(ErrorCode::InvalidParams))?;

    let tool_arguments = params.as_ref().and_then(|p| p.get("arguments"));

    Log::new(Some(&state.status_tx)).debug(format!("MCP tools/call: {}", tool_name));

    // Create event for LLM
    let event = Event::new(
        &MCP_TOOLS_CALL_EVENT,
        serde_json::json!({
            "method": "tools/call",
            "name": tool_name,
            "arguments": tool_arguments,
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "tools/call", e));
        }
    };

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "tools/call");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_tools_call" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default: tool execution failed
    log_model_no_answer(state, "tools/call", "a JSON-RPC error");
    Err(JsonRpcError::custom(
        ErrorCode::InternalError,
        format!("Tool execution failed: {}", tool_name),
    ))
}

/// Handle prompts/list request - LLM returns available prompts
#[cfg(feature = "mcp")]
async fn handle_prompts_list(state: &McpServerState) -> Result<Value, JsonRpcError> {
    Log::new(Some(&state.status_tx)).debug("MCP prompts/list request");

    // Create event for LLM
    let event = Event::new(
        &MCP_PROMPTS_LIST_EVENT,
        serde_json::json!({
            "method": "prompts/list",
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "prompts/list", e));
        }
    };

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "prompts/list");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_prompts_list" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default: empty prompts list
    log_model_no_answer(state, "prompts/list", "an empty prompts list");
    Ok(serde_json::json!({"prompts": []}))
}

/// Handle prompts/get request - LLM returns prompt template
#[cfg(feature = "mcp")]
async fn handle_prompts_get(
    state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let prompt_name = params
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .ok_or_else(|| JsonRpcError::new(ErrorCode::InvalidParams))?;

    let prompt_arguments = params.as_ref().and_then(|p| p.get("arguments"));

    Log::new(Some(&state.status_tx)).debug(format!("MCP prompts/get: {}", prompt_name));

    // Create event for LLM
    let event = Event::new(
        &MCP_PROMPTS_GET_EVENT,
        serde_json::json!({
            "method": "prompts/get",
            "name": prompt_name,
            "arguments": prompt_arguments,
        }),
    );

    // Reuse the server's protocol instance rather than allocating a throwaway per request.
    let protocol = state.protocol.clone();

    // Call LLM with action system
    let execution_result = match call_llm(
        &state.llm_client,
        &state.app_state,
        state.server_id,
        None,
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Err(llm_failure_error(state, "prompts/get", e));
        }
    };

    // Process action results
    for protocol_result in &execution_result.protocol_results {
        use crate::llm::actions::protocol_trait::ActionResult;
        if let ActionResult::Custom { name, data } = protocol_result {
            // A handler that returned mcp_error_response used to be ignored entirely: the
            // loop matched one name and fell through to the hardcoded default, so a chosen
            // JSON-RPC error was silently converted into a *success* reply. Honor it first.
            if name == "mcp_error" {
                log_model_reject(state, "prompts/get");
                return Err(mcp_error_from_action(data));
            }
            if name == "mcp_prompts_get" {
                if let Some(response) = data.get("response") {
                    return Ok(response.clone());
                }
            }
        }
    }

    // Default: prompt not found
    log_model_no_answer(state, "prompts/get", "a JSON-RPC error");
    Err(JsonRpcError::custom(
        ErrorCode::InternalError,
        format!("Prompt not found: {}", prompt_name),
    ))
}

/// Handle logging/setLevel request
#[cfg(feature = "mcp")]
async fn handle_logging_set_level(
    state: &McpServerState,
    params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    let level = params
        .as_ref()
        .and_then(|p| p.get("level"))
        .and_then(|l| l.as_str())
        .unwrap_or("info");

    // Was `debug!` on the file log but `[INFO]` on the TUI - level drift the Log facade
    // exists to prevent. Unified to INFO on both sinks: it was already user-visible.
    Log::new(Some(&state.status_tx)).info(format!("MCP log level set to: {}", level));

    Ok(serde_json::json!({}))
}

/// Handle completion/complete request - LLM provides completions
#[cfg(feature = "mcp")]
async fn handle_completion(
    _state: &McpServerState,
    _params: Option<Value>,
) -> Result<Value, JsonRpcError> {
    debug!("MCP completion/complete");

    // Not wired to the handler. The mcp_completion event type and the
    // mcp_completion_response action are no longer advertised, so nobody is told to write a
    // handler for something that never fires.
    Ok(serde_json::json!({
        "completion": {
            "values": [],
            "total": 0,
            "hasMore": false
        }
    }))
}

/// Handle initialized notification
#[cfg(feature = "mcp")]
async fn handle_initialized(state: &McpServerState) {
    Log::new(Some(&state.status_tx)).info("MCP client initialized");
}

/// Handle cancelled notification
#[cfg(feature = "mcp")]
async fn handle_cancelled(state: &McpServerState, params: Option<Value>) {
    if let Some(req_id) = params.as_ref().and_then(|p| p.get("requestId")) {
        Log::new(Some(&state.status_tx)).debug(format!("MCP cancelled: {:?}", req_id));
    }
}

/// Handle progress notification
#[cfg(feature = "mcp")]
async fn handle_progress(_state: &McpServerState, params: Option<Value>) {
    if let Some(progress) = params {
        trace!("MCP progress: {}", progress);
    }
}
