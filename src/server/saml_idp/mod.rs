//! SAML Identity Provider (IDP) server implementation
//!
//! This module implements a SAML 2.0 Identity Provider that authenticates users
//! and generates signed SAML assertions. The LLM controls authentication decisions,
//! user attributes, and assertion generation.

pub mod actions;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::SamlIdpProtocol;
use crate::state::app_state::AppState;
use actions::SAML_IDP_REQUEST_EVENT;

/// Largest request body this server will buffer.
///
/// `/acs`, `/sso` and every other route here is reachable with no credential at all, and the
/// body is handed to the model verbatim as prompt text, so the previous unbounded
/// `req.collect()` let one anonymous POST grow the process without limit and drive an LLM
/// call with megabytes of attacker-chosen prompt. A base64 SAMLResponse form is a few tens of
/// KiB; 256 KiB is generous for even a large assertion with attributes.
const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// Narrow a model-supplied HTTP status to `u16` without wrapping.
///
/// `status as u16` on a `u64` truncates, and the truncation is the dangerous direction here:
/// `65736` becomes `200`, and a 2xx is the only thing an SP treats as a completed sign-in. A
/// status the model cannot have meant must not become the one that admits someone.
fn status_or(value: Option<&serde_json::Value>, default: u16) -> u16 {
    match value.and_then(|v| v.as_u64()) {
        Some(raw) => u16::try_from(raw)
            .ok()
            .filter(|s| (100..=599).contains(s))
            .unwrap_or_else(|| {
                warn!("SAML IDP: ignoring out-of-range status {raw}, using {default}");
                default
            }),
        None => default,
    }
}

/// Build a response from parts that came from the model, without ever panicking.
///
/// `status`, and every response header, arrive as model output (`send_error_response` even
/// documents `status_code` as a parameter). The previous code did
/// `Response::builder().status(status as u16)…body(..).unwrap()`, so a `status_code` of
/// 1000 — or a header value containing CR/LF — panicked inside the connection task instead
/// of answering. Local copy of `http_common::handler::build_safe_response`, which the
/// `saml-idp` feature cannot reach because `http_common` is gated on `feature = "http"`.
fn build_safe_response(
    status: u16,
    headers: impl IntoIterator<Item = (String, String)>,
    body: String,
) -> Response<Full<Bytes>> {
    let status_code = StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("SAML IDP: invalid HTTP status {status}, sending 500 instead");
        StatusCode::INTERNAL_SERVER_ERROR
    });

    let mut builder = Response::builder().status(status_code);
    for (name, value) in headers {
        match (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => builder = builder.header(n, v),
            _ => warn!("SAML IDP: dropping invalid response header {name:?}"),
        }
    }

    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|e| {
            error!("SAML IDP: failed to build response ({e}), sending bare 500");
            let mut fallback =
                Response::new(Full::new(Bytes::from_static(b"Internal Server Error")));
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

/// The one reply this server sends when it cannot produce a SAML Response.
///
/// Never an assertion, never a 2xx: the only thing an SP accepts as a sign-in is a 2xx
/// carrying a `SAMLResponse` form, so failing with anything else is failing closed. The two
/// [`crate::utils::WireFailure`] categories map onto distinct HTTP codes — 503 + `Retry-After`
/// for a saturated backend so the client backs off, 500 otherwise so it records a fault —
/// and the body is `WireFailure`'s `&'static str`, which cannot carry the error.
fn fail_closed_response(failure: crate::utils::WireFailure) -> Response<Full<Bytes>> {
    let (status, headers): (u16, Vec<(String, String)>) = if failure.is_overloaded() {
        (
            503,
            vec![
                (
                    "content-type".to_string(),
                    "text/plain; charset=utf-8".to_string(),
                ),
                ("retry-after".to_string(), "1".to_string()),
            ],
        )
    } else {
        (
            500,
            vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )],
        )
    };

    build_safe_response(status, headers, failure.prefixed_text().to_string())
}

/// SAML IDP server that delegates authentication and assertion generation to LLM
pub struct SamlIdpServer;

impl SamlIdpServer {
    /// Spawn the SAML IDP server with LLM-controlled authentication
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

        Log::new(Some(&status_tx)).info(format!("SAML IDP server listening on {}", local_addr));

        let protocol = Arc::new(SamlIdpProtocol::new());

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
                            "Accepted SAML IDP connection {} from {}",
                            connection_id, remote_addr
                        ));

                        let status_tx_for_task = status_tx.clone();

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
                        let protocol_clone = protocol.clone();

                        // Spawn a task to handle this connection
                        tokio::spawn(async move {
                            let status_tx = status_tx_for_task;
                            let io = TokioIo::new(stream);

                            // Clone for service_fn closure
                            let llm_for_service = llm_client_clone.clone();
                            let state_for_service = app_state_clone.clone();
                            let status_for_service = status_tx.clone();
                            let protocol_for_service = protocol_clone.clone();

                            // Create a service that handles SAML IDP requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_for_service.clone();
                                let state_clone = state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_for_service.clone();
                                handle_saml_idp_request(
                                    req,
                                    connection_id,
                                    server_id,
                                    remote_addr,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                )
                            });

                            // Serve the connection
                            if let Err(e) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!(
                                    "Error serving SAML IDP connection {}: {}",
                                    connection_id, e
                                );
                            }

                            // Remove connection when done
                            debug!("SAML IDP connection {} closed", connection_id);
                            app_state_clone
                                .remove_connection_from_server(server_id, connection_id)
                                .await;
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept SAML IDP connection: {}", e));
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

/// Handle a SAML IDP request with LLM decision making
async fn handle_saml_idp_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    remote_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SamlIdpProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());

    Log::new(Some(&status_tx)).debug(format!("SAML IDP {} {} from {}", method, path, remote_addr));

    // Extract headers
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // Read request body
    // Bounded, and refused rather than truncated: a cut-off SAMLResponse would reach the
    // model as a well-formed request whose assertion happened to end early.
    let body_bytes = match http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!(
                "SAML IDP {} {} decision=fail_closed_body_rejected (limit {} bytes): {}",
                method, path, MAX_REQUEST_BYTES, e
            ));
            return Ok(build_safe_response(
                413,
                [(
                    "content-type".to_string(),
                    "text/plain; charset=utf-8".to_string(),
                )],
                "request body too large".to_string(),
            ));
        }
    };

    // TRACE level for full payloads
    if !body_bytes.is_empty() {
        trace!("SAML IDP request body: {} bytes", body_bytes.len());
        if let Ok(body_str) = String::from_utf8(body_bytes.clone()) {
            trace!("SAML IDP request body content: {}", body_str);
        }
    }

    // Update connection stats
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(body_bytes.len() as u64),
            None,
            None,
            None,
        )
        .await;

    // Build event for LLM
    let event = Event::new(
        &SAML_IDP_REQUEST_EVENT,
        serde_json::json!({
            "method": method.to_string(),
            "path": path,
            "query": query,
            "headers": headers,
            "body": if body_bytes.is_empty() {
                serde_json::Value::Null
            } else if let Ok(body_str) = String::from_utf8(body_bytes.clone()) {
                serde_json::Value::String(body_str)
            } else {
                // Never base64 in event data: a model cannot decode it, and this body is
                // always text on a working SAML binding (the assertion's own base64 arrives
                // inside a urlencoded form field). Say what happened instead.
                serde_json::Value::String(format!(
                    "<{} bytes of non-UTF-8 data; not a SAML binding this server understands>",
                    body_bytes.len()
                ))
            },
            "client_ip": remote_addr.ip().to_string(),
        }),
    );

    // Call LLM for decision
    debug!("Calling LLM for SAML IDP request decision");
    let action_result = call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await;

    // Execute actions and build response
    let response = match action_result {
        Ok(result) => {
            if result.protocol_results.is_empty() {
                // The model answered, but with nothing this handler can put on the wire.
                // Distinct in the log from an LLM *error* and from an explicit rejection.
                warn!(
                    "SAML IDP {} {} decision=fail_closed_no_action: model produced no actions",
                    method, path
                );
                Log::new(Some(&status_tx)).warn(format!(
                    "SAML IDP {} {}: model produced no actions",
                    method, path
                ));
                fail_closed_response(crate::utils::WireFailure::Unavailable)
            } else {
                // Parse HTTP response from protocol results
                use crate::llm::actions::protocol_trait::ActionResult;

                let mut status_code = 200u16;
                let mut response_headers = std::collections::HashMap::new();
                let mut response_body = String::new();
                // Did any action actually yield a response-shaped Output? Without this the
                // loop below silently left status 200 with an empty body whenever the model's
                // output was not JSON, or was JSON without status/headers/body — an empty
                // 200 is not a sign-in any SP accepts, so it is a fail-open in disguise.
                let mut produced_response = false;

                for protocol_result in result.protocol_results {
                    if let ActionResult::Output(output_data) = protocol_result {
                        // Parse JSON response data
                        if let Ok(json_value) =
                            serde_json::from_slice::<serde_json::Value>(&output_data)
                        {
                            if json_value.get("status").is_some() {
                                status_code = status_or(json_value.get("status"), 200);
                                produced_response = true;
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
                            if let Some(body) = json_value.get("body").and_then(|v| v.as_str()) {
                                response_body = body.to_string();
                                produced_response = true;
                            }
                            // Headers alone deliberately do NOT count. Every action this
                            // protocol defines sets a status and a body; JSON carrying only
                            // headers left the default 200 with an empty body standing, which
                            // is the fail-open the `produced_response` flag exists to prevent.
                        }
                    }
                }

                if !produced_response {
                    warn!(
                        "SAML IDP {} {} decision=fail_closed_unusable_output: \
                         actions ran but none yielded a response",
                        method, path
                    );
                    Log::new(Some(&status_tx)).warn(format!(
                        "SAML IDP {} {}: actions ran but none yielded a response",
                        method, path
                    ));
                    fail_closed_response(crate::utils::WireFailure::Unavailable)
                } else {
                    // The model answered. A 4xx/5xx it chose itself is its own refusal
                    // (`send_error_response`) and must stay distinguishable in the log from
                    // the two fail-closed paths above.
                    let decision = if status_code >= 400 {
                        "model_reject"
                    } else {
                        "model_answer"
                    };
                    debug!(
                        "SAML IDP {} {} decision={} status={}",
                        method, path, decision, status_code
                    );
                    build_safe_response(status_code, response_headers, response_body)
                }
            }
        }
        Err(e) => {
            // SAML rides on HTTP here, and the failure is ours rather than the peer's, so it
            // is a 5xx: 503 + Retry-After while the backend is saturated so the peer backs
            // off and retries, 500 otherwise. Critically it is not a SAML Response at all - a
            // 2xx carrying an assertion is the only thing an SP will accept as a sign-in, and
            // no branch on this path can produce one.
            //
            // Only the *category* reaches the socket; the error itself goes to the log and
            // the status stream, where an operator looks.
            let failure = crate::utils::WireFailure::classify(&e);
            let category = if failure.is_overloaded() {
                "overloaded"
            } else {
                "unavailable"
            };
            error!(
                "SAML IDP {} {} decision=fail_closed_llm_error category={}: {}",
                method, path, category, e
            );
            Log::new(Some(&status_tx)).error(format!(
                "LLM error for SAML IDP {} {} (category={}): {}",
                method, path, category, e
            ));
            fail_closed_response(failure)
        }
    };

    // Update bytes sent
    let response_size = response.body().size_hint().exact().unwrap_or(0);
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            None,
            Some(response_size),
            None,
            None,
        )
        .await;

    debug!(
        "SAML IDP response: {} ({} bytes)",
        response.status(),
        response_size
    );

    Ok(response)
}
