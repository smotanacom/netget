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

/// Largest request body this server will buffer.
///
/// `/acs`, `/sso` and every other route here is reachable with no credential at all, and the
/// body is handed to the model verbatim as prompt text, so the previous unbounded
/// `req.collect()` let one anonymous POST grow the process without limit and drive an LLM
/// call with megabytes of attacker-chosen prompt. A base64 SAMLResponse form is a few tens of
/// KiB; 256 KiB is generous for even a large assertion with attributes.
pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

/// How long a peer that has connected and produced nothing may hold a slot.
///
/// HTTP is client-speaks-first: the server says nothing until a request line arrives, so a peer
/// that has completed the TCP handshake and sent no byte has asked nothing and negotiated
/// nothing. That state carries no protocol yet, which is why this number is the same across
/// netget's HTTP-shaped servers while the idle bound below is not. Apache's `mod_reqtimeout`
/// gives the request header 20s and nginx's `client_header_timeout` 60s; 30s sits between the
/// two deployed norms.
///
/// hyper's own `header_read_timeout` is **not** this bound: its 30-second default is inert
/// unless `http1::Builder::timer` is also set, which nothing here does — hyper downgrades a
/// defaulted duration to `None` when no timer is present and applies no deadline at all.
const FIRST_BYTE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a connection with nothing outstanding may sit idle between requests.
///
/// These endpoints are reached two ways and the two pull in opposite directions. A **browser**
/// does one redirect round and never comes back on that connection, which is the case Apache's
/// `KeepAliveTimeout` default of 5s is tuned for. A **relying party's back-channel** client
/// (token exchange, introspection, a JWKS or metadata fetch) reuses a pooled connection, which
/// is the case nginx's 75-second `keepalive_timeout` is tuned for. 60s is comfortably past any
/// pooled back-channel round trip while not letting a browser that navigated away hold a slot
/// for minutes. Nothing here streams or long-polls, so no legitimate request sits open.
///
/// **The five-minute numbers SAML and OIDC do quote are not this number.** An assertion's
/// `NotOnOrAfter` and an `id_token`'s `exp` bound how long a *credential* may be presented, not
/// how long a socket may be silent; copying one into the other is the mistake this comment
/// exists to prevent. And a request still being answered is not silence either: the watchdog
/// reads [`ConnectionActivity`](crate::server::accept_bounded::ConnectionActivity), which
/// reports a connection with work in flight — a model round-trip, or an event a `manual` rule
/// parked for a human — as not idle at all.
const IDLE_BETWEEN_REQUESTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Concurrent connections this server admits.
///
/// The shared default. Each admitted connection may buffer one body of up to
/// [`MAX_REQUEST_BYTES`] (256 KiB), so the cap is what turns that per-connection bound into a total
/// one and holds the worst case well inside the ~1 GiB ceiling netget's HTTP family is held
/// to. A protocol only declares a smaller number when its per-connection cost is larger.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] is told before the socket closes.
///
/// `503 Service Unavailable` with a `Retry-After`, written directly onto the socket because the
/// peer has not sent a request line for hyper to answer. Fixed bytes: nothing derived from an
/// error reaches the wire (see `crate::utils::wire_failure`).
const CONNECTION_CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    Content-Length: 0\r\nRetry-After: 5\r\nConnection: close\r\n\r\n";

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
                warn!("SAML SP: ignoring out-of-range status {raw}, using {default}");
                default
            }),
        None => default,
    }
}

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
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "SAML SP",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((stream, remote_addr, permit)) => {
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
                        let now = crate::utils::clock::Instant::now();
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
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                let status_tx = status_tx_for_task;
                                // Held for the life of the connection, so the cap counts live
                                // clients rather than accepts.
                                let _permit = permit;

                                // First-byte bound, before hyper sees the socket.
                                //
                                // hyper owns every read once `serve_connection` starts, and a
                                // deadline on those reads would be wrong here rather than merely
                                // awkward: hyper keeps polling the connection for more input
                                // while a request is being answered, so such a deadline would
                                // fire in the middle of a model round-trip. `peek` waits for data
                                // without consuming it, so the request line is still there for
                                // hyper afterwards, and it bounds exactly the case that needs
                                // bounding - a peer that has connected and sent nothing at all.
                                let spoke = matches!(
                                    tokio::time::timeout(
                                        FIRST_BYTE_READ_TIMEOUT,
                                        stream.peek(&mut [0u8; 1]),
                                    )
                                    .await,
                                    Ok(Ok(n)) if n > 0
                                );

                                let io = TokioIo::new(stream);

                                // Clone for service_fn closure
                                let llm_for_service = llm_client_clone.clone();
                                let state_for_service = app_state_clone.clone();
                                let status_for_service = status_tx.clone();
                                let protocol_for_service = protocol_clone.clone();

                                // Tracks whether this connection is answering anything. A
                                // request waiting on the model, or parked for a human by a
                                // `manual` rule, holds the count above zero, so the idle watchdog
                                // below cannot close the connection the answer belongs to however
                                // long it takes - only genuine silence counts.
                                let activity = std::sync::Arc::new(
                                    crate::server::accept_bounded::ConnectionActivity::new(),
                                );
                                let activity_for_service = std::sync::Arc::clone(&activity);

                                // Create a service that handles SAML SP requests with LLM
                                let service = service_fn(move |req: Request<Incoming>| {
                                    let llm_clone = llm_for_service.clone();
                                    let state_clone = state_for_service.clone();
                                    let status_clone = status_for_service.clone();
                                    let protocol_clone = protocol_for_service.clone();
                                    let activity = std::sync::Arc::clone(&activity_for_service);
                                    async move {
                                        let _busy = activity.busy();
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
                                        .await
                                    }
                                });

                                // Serve HTTP/1 on this connection, bounded at both ends.
                                if !spoke {
                                    Log::new(Some(&status_tx)).debug(format!(
                                        "SAML SP peer {} sent nothing for {}s; closing before \
                                         any request",
                                        remote_addr,
                                        FIRST_BYTE_READ_TIMEOUT.as_secs()
                                    ));
                                } else {
                                    let conn = http1::Builder::new().serve_connection(io, service);
                                    tokio::pin!(conn);
                                    tokio::select! {
                                        result = &mut conn => {
                                            if let Err(err) = result {
                                                error!(
                                                    "Error serving SAML SP connection \
                                                     {}: {}",
                                                    connection_id,
                                                    err
                                                );
                                            }
                                        }
                                        _ = crate::server::accept_bounded::watch_idle(
                                            std::sync::Arc::clone(&activity),
                                            IDLE_BETWEEN_REQUESTS_TIMEOUT,
                                        ) => {
                                            Log::new(Some(&status_tx)).debug(format!(
                                                "SAML SP connection {} idle for {}s; closing",
                                                connection_id,
                                                IDLE_BETWEEN_REQUESTS_TIMEOUT.as_secs()
                                            ));
                                        }
                                    }
                                }

                                // Remove connection when done
                                debug!("SAML SP connection {} closed", connection_id);
                                app_state_clone
                                    .remove_connection_from_server(server_id, connection_id)
                                    .await;
                                let _ = status_tx.send("__UPDATE_UI__".to_string());
                            })
                            .await;
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
    // Bounded, and refused rather than truncated: a cut-off SAMLResponse would reach the
    // model as a well-formed request whose assertion happened to end early.
    let body_bytes = match http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes().to_vec(),
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!(
                "SAML SP {} {} decision=fail_closed_body_rejected (limit {} bytes): {}",
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
                if json_value.get("status").is_some() {
                    *status_code = status_or(json_value.get("status"), 200);
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
