//! SAML Service Provider (SP) server implementation
//!
//! Serves the SP side of SAML 2.0 Web Browser SSO: `/login` starts SP-initiated SSO, `/acs`
//! receives the IDP's SAMLResponse, `/metadata` describes the SP. The model reads the
//! assertion and decides who the user is.
//!
//! **NetGet verifies nothing.** The SAMLResponse body is passed to the model as text; no XML
//! signature is checked (there is no key to check it against), and neither are issuer,
//! audience, expiry or assertion-ID replay. This is a simulator for exercising IDPs and for
//! honeypots — never an access-control boundary.

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
use crate::server::SamlSpProtocol;
use crate::state::app_state::AppState;
use actions::SAML_SP_REQUEST_EVENT;

/// Build a response from parts that came from the model, without ever panicking.
///
/// `status` and every response header are model output (`send_error_response` documents
/// `status_code`, and `process_assertion` puts a model-supplied user id into `Set-Cookie`).
/// The previous code did `Response::builder().status(status as u16)…body(..).unwrap()`, so a
/// `status_code` of 1000 — or a header value containing CR/LF — panicked inside the
/// connection task. Local copy of `http_common::handler::build_safe_response`, which the
/// `saml-sp` feature cannot reach because `http_common` is gated on `feature = "http"`.
fn build_safe_response(
    status: u16,
    headers: impl IntoIterator<Item = (String, String)>,
    body: String,
) -> Response<Full<Bytes>> {
    let status_code = StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("SAML SP: invalid HTTP status {status}, sending 500 instead");
        StatusCode::INTERNAL_SERVER_ERROR
    });

    let mut builder = Response::builder().status(status_code);
    for (name, value) in headers {
        match (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => builder = builder.header(n, v),
            _ => warn!("SAML SP: dropping invalid response header {name:?}"),
        }
    }

    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|e| {
            error!("SAML SP: failed to build response ({e}), sending bare 500");
            let mut fallback =
                Response::new(Full::new(Bytes::from_static(b"Internal Server Error")));
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

/// SAML SP server that delegates authentication and assertion generation to LLM
pub struct SamlSpServer;

impl SamlSpServer {
    /// Spawn the SAML SP server with LLM-controlled authentication
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

        Log::new(Some(&status_tx)).info(format!("SAML SP server listening on {}", local_addr));

        let protocol = Arc::new(SamlSpProtocol::new());

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
                            "Accepted SAML SP connection {} from {}",
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

                            // Create a service that handles SAML SP requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_for_service.clone();
                                let state_clone = state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_for_service.clone();
                                handle_saml_sp_request(
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
                                error!("Error serving SAML SP connection {}: {}", connection_id, e);
                            }

                            // Remove connection when done
                            debug!("SAML SP connection {} closed", connection_id);
                            app_state_clone
                                .remove_connection_from_server(server_id, connection_id)
                                .await;
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept SAML SP connection: {}", e));
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

/// Handle a SAML SP request with LLM decision making
async fn handle_saml_sp_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    remote_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SamlSpProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());

    Log::new(Some(&status_tx)).debug(format!("SAML SP {} {} from {}", method, path, remote_addr));

    // Extract headers
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    // Read request body
    let body_bytes = match req.collect().await {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(e) => {
            error!("Failed to read SAML SP request body: {}", e);
            return Ok(build_safe_response(
                400,
                [],
                "Failed to read request body".to_string(),
            ));
        }
    };

    // TRACE level for full payloads
    if !body_bytes.is_empty() {
        trace!("SAML SP request body: {} bytes", body_bytes.len());
        if let Ok(body_str) = String::from_utf8(body_bytes.clone()) {
            trace!("SAML SP request body content: {}", body_str);
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
        &SAML_SP_REQUEST_EVENT,
        serde_json::json!({
            "method": method.to_string(),
            "path": path.clone(),
            "query": query,
            "headers": headers,
            "body": if body_bytes.is_empty() {
                serde_json::Value::Null
            } else if let Ok(body_str) = String::from_utf8(body_bytes.clone()) {
                serde_json::Value::String(body_str)
            } else {
                // For binary data, use base64
                serde_json::Value::String(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &body_bytes))
            },
            "client_ip": remote_addr.ip().to_string(),
        }),
    );

    // Call LLM for decision
    debug!("Calling LLM for SAML SP request decision");
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
            // Three outcomes have to stay distinguishable in the log, because they mean very
            // different things to an operator: the model refused (`decision=model_reject`),
            // the model said nothing usable (`decision=fail_closed_no_answer`), and the
            // backend erred (`decision=fail_closed_llm_error`, in the `Err` arm below).
            //
            // The default status used to be 200: a model that answered only with a
            // common/memory action, or whose protocol result was not a parseable
            // `Output`, produced an empty `200 OK`. For an SP that is a permissive
            // default on a no-answer path — it is not a sign-in (no `Set-Cookie`), but it
            // tells the browser the request succeeded. There is no default now; a
            // response is emitted only if a usable `Output` was actually seen.
            use crate::llm::actions::protocol_trait::ActionResult;

            /// Flatten `Multiple` so an `Output` wrapped in one is not silently dropped.
            fn collect_outputs(result: ActionResult, out: &mut Vec<Vec<u8>>) {
                match result {
                    ActionResult::Output(data) => out.push(data),
                    ActionResult::Multiple(inner) => {
                        for r in inner {
                            collect_outputs(r, out);
                        }
                    }
                    _ => {}
                }
            }

            let mut outputs = Vec::new();
            for protocol_result in result.protocol_results {
                collect_outputs(protocol_result, &mut outputs);
            }

            let mut answer: Option<(u16, std::collections::HashMap<String, String>, String)> = None;
            for output_data in outputs {
                let Ok(json_value) = serde_json::from_slice::<serde_json::Value>(&output_data)
                else {
                    warn!(
                        "SAML SP: ignoring non-JSON action output ({} bytes)",
                        output_data.len()
                    );
                    continue;
                };
                let (status_code, response_headers, response_body) =
                    answer.get_or_insert_with(|| {
                        (200u16, std::collections::HashMap::new(), String::new())
                    });
                if let Some(status) = json_value.get("status").and_then(|v| v.as_u64()) {
                    *status_code = status as u16;
                }
                if let Some(headers_obj) = json_value.get("headers").and_then(|v| v.as_object()) {
                    for (k, v) in headers_obj {
                        if let Some(v_str) = v.as_str() {
                            response_headers.insert(k.clone(), v_str.to_string());
                        }
                    }
                }
                if let Some(body) = json_value.get("body").and_then(|v| v.as_str()) {
                    *response_body = body.to_string();
                }
            }

            match answer {
                Some((status_code, response_headers, response_body)) => {
                    // A model-chosen 4xx is `send_error_response` — a real refusal, not a
                    // netget failure. Tag it separately so an operator can tell a denial
                    // from an outage.
                    let decision = if status_code >= 400 {
                        "model_reject"
                    } else {
                        "model_answer"
                    };
                    debug!(
                        "SAML SP {} {} from {} decision={} status={}",
                        method, path, remote_addr, decision, status_code
                    );
                    build_safe_response(status_code, response_headers, response_body)
                }
                None => {
                    // Either no actions at all, or actions that carried no usable HTTP
                    // response. Both are "the model answered nothing" — fail closed with a
                    // 500 and a category, never a 2xx.
                    Log::new(Some(&status_tx)).warn(format!(
                        "SAML SP {} {} from {} decision=fail_closed_no_answer status=500 \
                         (no usable action output)",
                        method, path, remote_addr
                    ));
                    build_safe_response(
                        500,
                        [(
                            "content-type".to_string(),
                            "text/plain; charset=utf-8".to_string(),
                        )],
                        crate::utils::WireFailure::Unavailable
                            .prefixed_text()
                            .to_string(),
                    )
                }
            }
        }
        Err(e) => {
            // SAML rides on HTTP here, and the failure is ours rather than the peer's, so it
            // is a 5xx: 503 while the backend is saturated so the peer retries, 500
            // otherwise. Critically it is not a SAML Response at all - a 2xx carrying an
            // assertion is the only thing an SP will accept as a sign-in, and no branch on
            // this path can produce one.
            //
            // The body is `WireFailure`'s `&'static str` category, never the error: the peer
            // is an untrusted browser and the error names the backend, the model and netget's
            // own retry machinery. The full error goes to the log and the status stream.
            let failure = crate::utils::WireFailure::classify(&e);
            let overloaded = failure.is_overloaded();
            let status = if overloaded { 503 } else { 500 };
            Log::new(Some(&status_tx)).error(format!(
                "SAML SP {} {} from {} decision=fail_closed_llm_error status={} overload={}: {}",
                method, path, remote_addr, status, overloaded, e
            ));
            let mut headers = vec![(
                "content-type".to_string(),
                "text/plain; charset=utf-8".to_string(),
            )];
            if overloaded {
                // Tell the browser/IDP to back off rather than record a permanent fault.
                headers.push(("retry-after".to_string(), "5".to_string()));
            }
            build_safe_response(status, headers, failure.prefixed_text().to_string())
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
        "SAML SP response: {} ({} bytes)",
        response.status(),
        response_size
    );

    Ok(response)
}
