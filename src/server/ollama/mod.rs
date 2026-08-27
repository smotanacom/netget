//! Ollama-compatible API server implementation
//!
//! V2: LLM controls all responses to API endpoints

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
use tracing::{debug, error};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::ollama::actions::{
    OllamaProtocol, OLLAMA_CHAT_REQUEST_EVENT, OLLAMA_GENERATE_REQUEST_EVENT,
    OLLAMA_MODELS_REQUEST_EVENT,
};
use crate::state::app_state::AppState;

/// Ollama-compatible API server with LLM control
pub struct OllamaServer;

impl OllamaServer {
    /// Spawn the Ollama API server with integrated LLM actions
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
        Log::new(Some(&status_tx)).info(format!("Ollama API server listening on {}", local_addr));

        let protocol = Arc::new(OllamaProtocol::new());

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
                            "Ollama API connection {} from {}",
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

                            // Create a service that handles Ollama API requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_ollama_request(
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
                                error!("Error serving Ollama API connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("Ollama API connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept Ollama API connection: {}", e));
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

/// Handle a single Ollama API request
async fn handle_ollama_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OllamaProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();

    // Summary FileOnly: the ollama_* event templates render the equivalent line
    // to the TUI.
    Log::new(Some(&status_tx)).debug(format!("Ollama API request: {} {}", method, path));

    // Route the request
    match (method.clone(), path) {
        (Method::GET, "/api/tags") => {
            handle_tags_list_v2(
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        (Method::POST, "/api/generate") => {
            handle_generate_v2(
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        (Method::POST, "/api/chat") => {
            handle_chat_v2(
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        (Method::POST, "/api/embeddings") => handle_embeddings(req, status_tx).await,
        (Method::POST, "/api/show") => {
            handle_show(
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        // The four model-management endpoints are decisions, so they go through the model.
        // They used to answer {"status":"success"} unconditionally without an event.
        (Method::POST, "/api/pull") => {
            handle_admin(
                "pull",
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        (Method::POST, "/api/create") => {
            handle_admin(
                "create",
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        (Method::POST, "/api/copy") => {
            handle_admin(
                "copy",
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        (Method::DELETE, "/api/delete") => {
            handle_admin(
                "delete",
                req,
                connection_id,
                llm_client,
                app_state,
                status_tx,
                protocol,
                server_id,
            )
            .await
        }
        _ => {
            Log::new(Some(&status_tx))
                .debug(format!("Ollama API: Unknown endpoint {} {}", method, path));
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    json!({
                        "error": "Not Found"
                    })
                    .to_string(),
                )))
                .unwrap())
        }
    }
}

/// Handle GET /api/tags - List available models (V2: LLM controlled)
async fn handle_tags_list_v2(
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OllamaProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let log = Log::new(Some(&status_tx));
    log.debug("Ollama API: Listing models (LLM controlled)");

    // Create event for models request
    let event = Event::new(&OLLAMA_MODELS_REQUEST_EVENT, json!({}));

    // Call LLM with event (includes Event ID for mock compatibility)
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
        Ok(llm_result) => {
            // Look for ollama_models_response action
            for action in &llm_result.raw_actions {
                if action.get("type").and_then(|v| v.as_str()) == Some("ollama_models_response") {
                    if let Some(models) = action.get("models").and_then(|v| v.as_array()) {
                        let ollama_models: Vec<Value> = models
                            .iter()
                            .map(|model_name| {
                                let name = model_name.as_str().unwrap_or("unknown");
                                json!({
                                    "name": name,
                                    "modified_at": "2024-01-01T00:00:00Z",
                                    "size": 0,
                                    "digest": "0000000000000000",
                                    "details": {
                                        "format": "gguf",
                                        "family": "llama"
                                    }
                                })
                            })
                            .collect();

                        let response = json!({
                            "models": ollama_models
                        });

                        return Ok(Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/json")
                            .body(Full::new(Bytes::from(response.to_string())))
                            .unwrap());
                    }
                }
            }

            // No action found, return empty list
            let response = json!({"models": []});
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(response.to_string())))
                .unwrap())
        }
        Err(e) => {
            // Non-fatal: the client gets a 500 (wire fallback).
            log.warn(format!("LLM error: {}", e));

            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    json!({
                        "error": crate::utils::WireFailure::classify(&e).text()
                    })
                    .to_string(),
                )))
                .unwrap())
        }
    }
}

/// Handle POST /api/generate - Generate text (V2: LLM controlled)
async fn handle_generate_v2(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    _status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OllamaProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Read request body
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    json!({"error": "Failed to read body"}).to_string(),
                )))
                .unwrap());
        }
    };

    // Parse JSON
    let request_json: Value = match serde_json::from_slice(&body_bytes) {
        Ok(json) => json,
        Err(e) => {
            error!("Failed to parse JSON: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    json!({"error": "Invalid JSON"}).to_string(),
                )))
                .unwrap());
        }
    };

    let model = request_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let prompt = request_json
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let stream = request_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    debug!(
        "Generate: model={}, prompt_len={}, stream={}",
        model,
        prompt.len(),
        stream
    );

    // Create event for generate request
    let event = Event::new(
        &OLLAMA_GENERATE_REQUEST_EVENT,
        json!({
            "model": model,
            "prompt": prompt,
            "stream": stream
        }),
    );

    // Call LLM with event (includes Event ID for mock compatibility)
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
        Ok(llm_result) => {
            // Look for ollama_generate_response action
            for action in &llm_result.raw_actions {
                if action.get("type").and_then(|v| v.as_str()) == Some("ollama_generate_response") {
                    if let Some(response_text) =
                        action.get("response_text").and_then(|v| v.as_str())
                    {
                        if stream {
                            // Streaming response: send NDJSON chunks (all at once)
                            return Ok(build_streaming_generate_response(&model, response_text));
                        } else {
                            // Non-streaming response
                            let response = json!({
                                "model": model,
                                "created_at": "2024-01-01T00:00:00Z",
                                "response": response_text,
                                "done": true
                            });

                            return Ok(Response::builder()
                                .status(StatusCode::OK)
                                .header("Content-Type", "application/json")
                                .body(Full::new(Bytes::from(response.to_string())))
                                .unwrap());
                        }
                    }
                }
            }

            // A deliberate refusal is not the same as no answer.
            if let Some(refusal) = model_error_response(&llm_result.raw_actions) {
                return Ok(refusal);
            }

            // No valid action, return error
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    json!({"error": "No response from LLM"}).to_string(),
                )))
                .unwrap())
        }
        Err(e) => {
            error!("LLM error: {}", e);
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(
                    json!({"error": crate::utils::WireFailure::classify(&e).text()}).to_string(),
                )))
                .unwrap())
        }
    }
}

/// Handle POST /api/chat - Chat completion (V2: LLM controlled)
async fn handle_chat_v2(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    _status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OllamaProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Read request body
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from(
                    json!({"error": "Failed to read body"}).to_string(),
                )))
                .unwrap());
        }
    };

    // Parse JSON
    let request_json: Value = match serde_json::from_slice(&body_bytes) {
        Ok(json) => json,
        Err(e) => {
            error!("Failed to parse JSON: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from(
                    json!({"error": "Invalid JSON"}).to_string(),
                )))
                .unwrap());
        }
    };

    let model = request_json
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let messages = request_json
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let stream = request_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    debug!(
        "Chat: model={}, {} messages, stream={}",
        model,
        messages.len(),
        stream
    );

    // Create event for chat request
    let event = Event::new(
        &OLLAMA_CHAT_REQUEST_EVENT,
        json!({
            "model": model,
            "messages": messages,
            "stream": stream
        }),
    );

    // Call LLM with event (includes Event ID for mock compatibility)
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
        Ok(llm_result) => {
            // Look for ollama_chat_response action
            for action in &llm_result.raw_actions {
                if action.get("type").and_then(|v| v.as_str()) == Some("ollama_chat_response") {
                    if let Some(message_content) =
                        action.get("message_content").and_then(|v| v.as_str())
                    {
                        if stream {
                            // Streaming response: send NDJSON chunks (all at once)
                            return Ok(build_streaming_chat_response(&model, message_content));
                        } else {
                            // Non-streaming response
                            let response = json!({
                                "model": model,
                                "created_at": "2024-01-01T00:00:00Z",
                                "message": {
                                    "role": "assistant",
                                    "content": message_content
                                },
                                "done": true
                            });

                            return Ok(Response::builder()
                                .status(StatusCode::OK)
                                .header("Content-Type", "application/json")
                                .body(Full::new(Bytes::from(response.to_string())))
                                .unwrap());
                        }
                    }
                }
            }

            // A deliberate refusal is not the same as no answer.
            if let Some(refusal) = model_error_response(&llm_result.raw_actions) {
                return Ok(refusal);
            }

            // No valid action
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from(
                    json!({"error": "No response from LLM"}).to_string(),
                )))
                .unwrap())
        }
        Err(e) => {
            error!("LLM error: {}", e);
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Full::new(Bytes::from(
                    json!({"error": crate::utils::WireFailure::classify(&e).text()}).to_string(),
                )))
                .unwrap())
        }
    }
}

/// The HTTP reply for an `ollama_error_response` action, if the model produced one.
///
/// `ollama_error_response` is declared, offered on every event and named in the event
/// descriptions ("or refuse with ollama_error_response") — but nothing read it. Its executor
/// returns a bare acknowledgement, and both request handlers scanned `raw_actions` only for
/// their own success action, so a model's deliberate refusal was dropped and the client got
/// the identical generic 500 `{"error": "No response from LLM"}` that a model saying nothing
/// produces. That is the one distinction that has to survive: a refusal and an outage must
/// not be diagnosed as each other.
///
/// The status defaults to 400 rather than 500 because the refusal is a statement about the
/// request, not about this server; `status_code` lets the model pick the Ollama-accurate one
/// (404 for an unknown model).
fn model_error_response(raw_actions: &[serde_json::Value]) -> Option<Response<Full<Bytes>>> {
    let action = raw_actions.iter().find(|action| {
        action.get("type").and_then(|v| v.as_str()) == Some("ollama_error_response")
    })?;

    let error_message = action
        .get("error_message")
        .and_then(|v| v.as_str())
        .unwrap_or("request refused");

    let status = action
        .get("status_code")
        .and_then(|v| v.as_u64())
        .and_then(|code| StatusCode::from_u16(code as u16).ok())
        .unwrap_or(StatusCode::BAD_REQUEST);

    let body = json!({ "error": error_message }).to_string();

    Some(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body.clone())))
            .unwrap_or_else(|_| {
                // Never fall back to a 2xx: `Response::new` is 200, which would turn the
                // model's refusal into a success.
                let mut response = Response::new(Full::new(Bytes::from(body)));
                *response.status_mut() = StatusCode::BAD_REQUEST;
                response
            }),
    )
}

/// Build streaming generate response (NDJSON format, all sent at once)
fn build_streaming_generate_response(model: &str, response_text: &str) -> Response<Full<Bytes>> {
    let mut ndjson = String::new();

    // Split response into words for streaming chunks
    let words: Vec<&str> = response_text.split_whitespace().collect();

    for (i, word) in words.iter().enumerate() {
        let chunk = json!({
            "model": model,
            "created_at": "2024-01-01T00:00:00Z",
            "response": if i == 0 { word.to_string() } else { format!(" {}", word) },
            "done": false
        });
        ndjson.push_str(&chunk.to_string());
        ndjson.push('\n');
    }

    // Final done chunk
    let final_chunk = json!({
        "model": model,
        "created_at": "2024-01-01T00:00:00Z",
        "response": "",
        "done": true
    });
    ndjson.push_str(&final_chunk.to_string());
    ndjson.push('\n');

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(Full::new(Bytes::from(ndjson)))
        .unwrap()
}

/// Build streaming chat response (NDJSON format, all sent at once)
fn build_streaming_chat_response(model: &str, content: &str) -> Response<Full<Bytes>> {
    let mut ndjson = String::new();

    // Split content into words for streaming chunks
    let words: Vec<&str> = content.split_whitespace().collect();

    for (i, word) in words.iter().enumerate() {
        let chunk = json!({
            "model": model,
            "created_at": "2024-01-01T00:00:00Z",
            "message": {
                "role": "assistant",
                "content": if i == 0 { word.to_string() } else { format!(" {}", word) }
            },
            "done": false
        });
        ndjson.push_str(&chunk.to_string());
        ndjson.push('\n');
    }

    // Final done chunk
    let final_chunk = json!({
        "model": model,
        "created_at": "2024-01-01T00:00:00Z",
        "message": {
            "role": "assistant",
            "content": ""
        },
        "done": true
    });
    ndjson.push_str(&final_chunk.to_string());
    ndjson.push('\n');

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson")
        .body(Full::new(Bytes::from(ndjson)))
        .unwrap()
}

// Keep the simple endpoints unchanged
async fn handle_embeddings(
    req: Request<Incoming>,
    status_tx: mpsc::UnboundedSender<String>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from(
                    json!({"error": "Failed to read body"}).to_string(),
                )))
                .unwrap());
        }
    };

    let _request_json: Value = match serde_json::from_slice(&body_bytes) {
        Ok(json) => json,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from(
                    json!({"error": "Invalid JSON"}).to_string(),
                )))
                .unwrap());
        }
    };

    Log::new(Some(&status_tx)).debug("Embeddings request received");

    // Return mock embeddings (768 dimensions)
    let embedding: Vec<f32> = (0..768).map(|i| (i as f32) / 768.0).collect();

    let response = json!({
        "embedding": embedding
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(response.to_string())))
        .unwrap())
}

/// Answer `/api/show` from the model, or refuse.
///
/// This used to reply with a fabricated Modelfile (`FROM {name}`), a hardcoded
/// `temperature 0.7` and a `gguf`/`llama` details block — for any name at all, with no event
/// and no `call_llm` in the path. A server told "this instance serves only llama2" happily
/// described every model a client asked about, including ones it had just refused to pull.
/// Nothing is invented now: no `ollama_show_response` means the request is refused.
#[allow(clippy::too_many_arguments)]
async fn handle_show(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OllamaProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let log = Log::new(Some(&status_tx));

    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(bad_request("Failed to read body")),
    };
    let request_json: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let model = request_json
        .get("name")
        .or_else(|| request_json.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let event = Event::new(
        &actions::OLLAMA_SHOW_REQUEST_EVENT,
        json!({ "model": model }),
    );

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
        Ok(result) => {
            for msg in result.messages {
                let _ = status_tx.send(msg);
            }
            if let Some(response) = model_error_response(&result.raw_actions) {
                log.info(format!(
                    "Ollama show refused for '{}' (decision=model_reject)",
                    model
                ));
                return Ok(response);
            }
            use crate::llm::actions::protocol_trait::ActionResult;
            if let Some(data) = result.protocol_results.iter().find_map(|r| match r {
                ActionResult::Custom { name, data } if name == "ollama_show_response" => Some(data),
                _ => None,
            }) {
                log.debug(format!("Ollama show answered for '{}'", model));
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(data.to_string())))
                    .unwrap());
            }
            log.warn(format!(
                "Ollama show refused for '{}' (decision=fail_closed_no_action): the handler \
                 produced no ollama_show_response",
                model
            ));
            Ok(server_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::utils::WireFailure::Unavailable.text(),
            ))
        }
        Err(e) => {
            let failure = crate::utils::WireFailure::classify(&e);
            error!(
                "Ollama show for '{}' decision=fail_closed_llm_error category={:?}: {:#}",
                model, failure, e
            );
            let status = if failure.is_overloaded() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Ok(server_error(status, failure.text()))
        }
    }
}

/// Ask the model whether to perform a model-management operation, and refuse unless it says so.
///
/// `/api/pull`, `/api/create`, `/api/copy` and `/api/delete` used to answer
/// `{"status": "success"}` unconditionally, with no event and no LLM call anywhere in the path.
/// A server instructed "this instance only serves llama2, refuse anything else" reported every
/// pull as downloaded and every delete as removed, and `/api/pull` additionally invented a
/// digest of `sha256:0000000000000000`. The model was never asked, so its instruction could not
/// be wrong — it simply had no effect.
///
/// Confirmation requires an explicit `ollama_admin_ok`. A model refusal
/// (`ollama_error_response`), an answer containing neither, and a backend failure all refuse,
/// and stay distinguishable: the refusal carries the model's own message and status, the other
/// two carry only a `WireFailure` category and are separated by `decision=` in the log.
#[allow(clippy::too_many_arguments)]
async fn handle_admin(
    operation: &str,
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OllamaProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let log = Log::new(Some(&status_tx));

    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Ok(bad_request("Failed to read body"));
        }
    };

    // DELETE may legitimately arrive with no body; the others carry JSON. An unparseable body
    // is still shown to the model rather than answered here, so the decision stays in one place.
    let request_json: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    let model = request_json
        .get("name")
        .or_else(|| request_json.get("model"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let destination = request_json
        .get("destination")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let event = Event::new(
        &actions::OLLAMA_ADMIN_REQUEST_EVENT,
        json!({
            "operation": operation,
            "model": model,
            "destination": destination,
        }),
    );

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
        Ok(result) => {
            for msg in result.messages {
                let _ = status_tx.send(msg);
            }

            // A deliberate refusal first: it must not be collapsed into the no-answer path.
            if let Some(response) = model_error_response(&result.raw_actions) {
                log.info(format!(
                    "Ollama {} refused for '{}' (decision=model_reject)",
                    operation, model
                ));
                return Ok(response);
            }

            use crate::llm::actions::protocol_trait::ActionResult;
            if let Some(data) = result.protocol_results.iter().find_map(|r| match r {
                ActionResult::Custom { name, data } if name == "ollama_admin_ok" => Some(data),
                _ => None,
            }) {
                log.info(format!(
                    "Ollama {} acknowledged for '{}' (decision=model_accept)",
                    operation, model
                ));
                // `pull` reports progress fields; the others just report status. Only what the
                // model supplied is echoed — no invented digest.
                let mut body = json!({ "status": "success" });
                if let Some(d) = data.get("digest") {
                    body["digest"] = d.clone();
                }
                if let Some(t) = data.get("total") {
                    body["total"] = t.clone();
                }
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(body.to_string())))
                    .unwrap());
            }

            log.warn(format!(
                "Ollama {} refused for '{}' (decision=fail_closed_no_action): the handler \
                 produced no ollama_admin_ok",
                operation, model
            ));
            Ok(server_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::utils::WireFailure::Unavailable.text(),
            ))
        }
        Err(e) => {
            let failure = crate::utils::WireFailure::classify(&e);
            error!(
                "Ollama {} for '{}' decision=fail_closed_llm_error category={:?}: {:#}",
                operation, model, failure, e
            );
            log.warn(format!(
                "Ollama {} refused for '{}' (decision=fail_closed_llm_error)",
                operation, model
            ));
            let status = if failure.is_overloaded() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            Ok(server_error(status, failure.text()))
        }
    }
}

/// A 400 with Ollama's `{"error": ...}` envelope.
fn bad_request(message: &str) -> Response<Full<Bytes>> {
    server_error(StatusCode::BAD_REQUEST, message)
}

/// An `{"error": ...}` body at the given status. Never 2xx: these are all refusals.
fn server_error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = json!({ "error": message }).to_string();
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.clone())))
        .unwrap_or_else(|_| {
            let mut response = Response::new(Full::new(Bytes::from(body)));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        })
}
