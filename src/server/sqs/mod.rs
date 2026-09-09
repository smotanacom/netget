//! AWS SQS (Simple Queue Service) compatible server implementation
//!
//! Implements an SQS-compatible HTTP/JSON API on port 9324.
//! The LLM controls all queue operations and maintains "virtual" queues through conversation context.

pub mod actions;

use std::convert::Infallible;
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
use tracing::error;

use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::server::connection::ConnectionId;
use crate::server::SqsProtocol;
use crate::state::app_state::AppState;
use crate::{console_error, console_info};

/// Largest request body this server will buffer.
///
/// The body was read with an unbounded `req.into_body().collect()`, so a single request
/// could grow the process without limit. A real SQS message is capped at 256 KiB and a
/// SendMessageBatch carries ten of them, so 4 MiB is well clear of anything an SDK sends.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// SQS server that delegates queue operations to LLM
pub struct SqsServer;

impl SqsServer {
    /// Spawn the SQS server with integrated LLM actions
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
        console_info!(status_tx, "SQS server listening on {}", local_addr);

        let protocol = Arc::new(SqsProtocol::new());

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
                            "SQS connection {} from {}",
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

                            // Create a service that handles SQS requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_sqs_request_with_llm(
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
                                error!("Error serving SQS connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("SQS connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "Failed to accept SQS connection: {}", e);
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        // Without this the handle was dropped on the floor: the task kept running and the
        // socket stayed bound after the server was closed, so restarting on the same port
        // failed with EADDRINUSE while the UI reported the server as stopped.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

/// Handle a single SQS request with LLM, then record what crossed the wire.
///
/// The rail's byte counters and the connection-scoped task prompts read
/// `bytes_received`/`bytes_sent`, and this server left both at the zero it registered the
/// connection with, so every SQS peer showed no traffic at all however much it moved.
async fn handle_sqs_request_with_llm(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SqsProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let received = req.body().size_hint().lower();
    let response = handle_sqs_request_inner(
        req,
        llm_client,
        app_state.clone(),
        status_tx.clone(),
        protocol,
        server_id,
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

async fn handle_sqs_request_inner(
    req: Request<Incoming>,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SqsProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Extract request details
    let method = req.method().to_string();
    let uri = req.uri().to_string();

    // Extract SQS operation from x-amz-target header
    // Format: "AmazonSQS.SendMessage", "AmazonSQS.ReceiveMessage", etc.
    let operation = req
        .headers()
        .get("x-amz-target")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split('.').nth(1))
        .unwrap_or("Unknown")
        .to_string();

    // Read the JSON body, capped at MAX_REQUEST_BYTES. Refuse rather than continue with an
    // empty body: an operation whose parameters were truncated away used to reach the model
    // as a well-formed request with no arguments, and be answered as one.
    let body_bytes = match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!(
                "SQS {} decision=fail_closed_body_rejected (limit {} bytes): {}",
                operation, MAX_REQUEST_BYTES, e
            ));
            return Ok(build_sqs_response(
                413,
                &new_request_id(),
                serde_json::json!({
                    "__type": "InvalidParameterValue",
                    "message": "request body rejected",
                })
                .to_string(),
            ));
        }
    };

    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    Log::new(Some(&status_tx)).debug(format!(
        "SQS request: {} {} operation={} ({} bytes)",
        method,
        uri,
        operation,
        body_bytes.len()
    ));

    // Try to extract queue URL from JSON body
    let queue_url = if !body_str.is_empty() {
        serde_json::from_str::<serde_json::Value>(&body_str)
            .ok()
            .and_then(|v| v.get("QueueUrl").and_then(|q| q.as_str()).map(String::from))
    } else {
        None
    };

    Log::new(Some(&status_tx)).trace(format!("SQS request body: {}", body_str));

    // Create SQS request event
    let event = crate::protocol::Event::new(
        &actions::SQS_REQUEST_EVENT,
        serde_json::json!({
            "operation": operation,
            "queue_url": queue_url,
            "request_body": body_str,
        }),
    );

    let llm_result = crate::llm::action_helper::call_llm(
        &llm_client,
        &app_state,
        server_id,
        None, // Connection ID not needed for stateless HTTP
        &event,
        protocol.as_ref(),
    )
    .await;

    // Process action results to build HTTP response
    match llm_result {
        Ok(execution_result) => {
            // Look for SQS-specific response actions
            for result in execution_result.protocol_results {
                match result {
                    ActionResult::Custom { name, data } => {
                        if name == "sqs_response" {
                            let status =
                                data.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                            let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("{}");

                            // `decision=` is a stable grep target, as in `src/server/radius/`:
                            // an operator has to be able to tell the model answering from
                            // netget failing to reach one.
                            let decision = if (400..=599).contains(&status) {
                                "model_reject"
                            } else {
                                "model_answer"
                            };
                            let log = Log::new(Some(&status_tx));
                            log.debug(format!(
                                "SQS {} decision={} status={}",
                                operation, decision, status
                            ));
                            log.trace(format!("SQS response body: {}", body));

                            let request_id = new_request_id();

                            return Ok(build_sqs_response(status, &request_id, body.to_string()));
                        }
                    }
                    _ => {
                        // Other actions don't affect HTTP response
                    }
                }
            }

            // The handler ran but produced no `sqs_response` — a model that refused, a static
            // handler with an empty action list, or an answer whose actions were all
            // unrecognised.
            //
            // This used to answer 200 `{}`, at debug level. For SendMessage or DeleteMessage
            // that is a successful call as far as any AWS SDK is concerned, so a declined send
            // was reported as delivered and a declined delete as removed; for ReceiveMessage it
            // reads as "the queue is empty", a claim about the queue nothing supports.
            //
            // Fail closed with the same AWS error envelope the backend-error arm below uses.
            Log::new(Some(&status_tx)).warn(format!(
                "SQS {} decision=fail_closed_no_action (no sqs_response action produced); \
                 answering 500 rather than an empty 200",
                operation
            ));

            let request_id = new_request_id();

            Ok(build_sqs_response(
                500,
                &request_id,
                serde_json::json!({
                    "__type": "InternalFailure",
                    "message": crate::utils::WireFailure::Unavailable.prefixed_text(),
                })
                .to_string(),
            ))
        }
        Err(e) => {
            // netget could not reach a decision at all. The peer gets a category, the log
            // gets the error — never the backend URL, the model name or netget's own retry
            // machinery. The two categories map onto different codes so an SDK backs off
            // instead of recording a permanent fault: `ServiceUnavailable` + 503 +
            // Retry-After is retryable in every AWS SDK's default policy, `InternalFailure`
            // + 500 is not. Collapsing both onto 500, as this used to, makes a transient
            // backend saturation look permanent.
            let failure = crate::utils::WireFailure::classify(&e);
            Log::new(Some(&status_tx)).warn(format!(
                "SQS {} decision=fail_closed_llm_error category={:?}: {}",
                operation, failure, e
            ));

            let (status, aws_type) = match failure {
                crate::utils::WireFailure::Overloaded => (503, "ServiceUnavailable"),
                crate::utils::WireFailure::Unavailable => (500, "InternalFailure"),
            };

            let mut response = build_sqs_response(
                status,
                &new_request_id(),
                serde_json::json!({
                    "__type": aws_type,
                    "message": failure.text(),
                })
                .to_string(),
            );
            if failure == crate::utils::WireFailure::Overloaded {
                response
                    .headers_mut()
                    .insert("Retry-After", hyper::header::HeaderValue::from_static("5"));
            }
            Ok(response)
        }
    }
}

/// Build an AWS-JSON response.
///
/// `status` originates in model output. `Response::builder().status()` rejects anything
/// outside 100-999 and the previous `.unwrap()` turned that into a panic, killing the
/// hyper connection task and leaving the client waiting on a socket that never answers.
/// `SqsProtocol::execute_action` already rejects out-of-range values with a message the
/// model sees; this is the belt-and-braces path.
fn build_sqs_response(status: u16, request_id: &str, body: String) -> Response<Full<Bytes>> {
    let status = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("Invalid SQS status code {}, sending 500 instead", status);
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });

    Response::builder()
        .status(status)
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("x-amzn-RequestId", request_id)
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("{}"))))
}

/// Timestamp-derived request id, echoed in `x-amzn-RequestId`.
fn new_request_id() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}
