//! OAuth2 authorization server implementation
//!
//! OAuth2 server implementing RFC 6749 (OAuth 2.0 Authorization Framework).
//! The LLM controls authorization decisions, token generation, and client validation.

pub mod actions;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::oauth2::actions::{
    OAuth2Protocol, OAUTH2_AUTHORIZE_EVENT, OAUTH2_INTROSPECT_EVENT, OAUTH2_RESULT_KEY,
    OAUTH2_REVOKE_EVENT, OAUTH2_TOKEN_EVENT,
};
use crate::state::app_state::AppState;

/// Largest request body this server will buffer.
///
/// Every endpoint here is reachable **before** any credential is checked, and the body is
/// parsed and handed to the model as prompt text, so an unbounded `collect()` let one
/// anonymous POST grow the process without limit. An OAuth2 form body is a handful of short
/// parameters; 64 KiB is far past anything a conforming client sends.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Buffer a request body, refusing anything over [`MAX_REQUEST_BYTES`].
///
/// Refusing is the point: continuing with a truncated body would hand the model a
/// well-formed request whose credentials had been cut off, and it would be answered as one.
async fn read_body_limited(body: Incoming) -> Result<Bytes, ()> {
    match http_body_util::Limited::new(body, MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(e) => {
            warn!("OAuth2: rejecting request body over {MAX_REQUEST_BYTES} bytes: {e}");
            Err(())
        }
    }
}

/// The form parameters that are safe to put in a log line or on the status stream.
///
/// `client_secret`, `password`, `token`, `code` and `refresh_token` are credentials. They
/// still reach the *model*, because deciding whether they are valid is the whole job, but a
/// `{:?}` of the whole parameter map used to copy them into `netget.log` and onto the TUI
/// status stream, where they outlive the request and are read by anyone with the file.
fn redact_params(params: &HashMap<String, String>) -> Vec<(&str, &str)> {
    const SECRET: &[&str] = &[
        "client_secret",
        "password",
        "token",
        "code",
        "refresh_token",
        "assertion",
    ];
    let mut pairs: Vec<(&str, &str)> = params
        .iter()
        .map(|(k, v)| {
            if SECRET.contains(&k.as_str()) {
                (k.as_str(), if v.is_empty() { "" } else { "<redacted>" })
            } else {
                (k.as_str(), v.as_str())
            }
        })
        .collect();
    pairs.sort_unstable();
    pairs
}

/// Narrow a model-supplied HTTP status to `u16` without wrapping.
///
/// `status as u16` on a `u64` truncates, and the truncation is the dangerous direction:
/// `65736` becomes `200`, turning a nonsense error status into a success the client
/// believes. Anything that is not a real status falls back to the RFC default.
fn status_or(value: Option<&serde_json::Value>, default: u16) -> u16 {
    match value.and_then(|v| v.as_u64()) {
        Some(raw) => u16::try_from(raw)
            .ok()
            .filter(|s| (100..=599).contains(s))
            .unwrap_or_else(|| {
                warn!("OAuth2: ignoring out-of-range status_code {raw}, using {default}");
                default
            }),
        None => default,
    }
}

/// Build a response from parts that came from the model or from the request, without ever
/// panicking.
///
/// Every `Response::builder()` chain here used to end in `.body(..).unwrap()`. That is a
/// remotely reachable panic: `/authorize` puts the client-supplied `redirect_uri` straight
/// into the `Location` header, and `parse_query_params` percent-decodes it first, so
/// `?redirect_uri=http://x/%0D%0A` yields a header value containing CRLF. hyper rejects it,
/// `.body()` returns `Err`, and the `unwrap()` kills the connection task. (Had hyper
/// accepted it, the same input would have been response-splitting.)
///
/// This is a local copy of `http_common::handler::build_safe_response`, which the `oauth2`
/// feature cannot reach because `http_common` is gated on `feature = "http"`.
fn build_safe_response(
    status: u16,
    headers: impl IntoIterator<Item = (String, String)>,
    body: String,
) -> Response<Full<Bytes>> {
    let status_code = StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("OAuth2: invalid HTTP status {status}, sending 500 instead");
        StatusCode::INTERNAL_SERVER_ERROR
    });

    let mut builder = Response::builder().status(status_code);
    for (name, value) in headers {
        match (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => builder = builder.header(n, v),
            _ => warn!("OAuth2: dropping invalid response header {name:?}"),
        }
    }

    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|e| {
            error!("OAuth2: failed to build response ({e}), sending bare 500");
            let mut fallback = Response::new(Full::new(Bytes::from_static(
                br#"{"error":"server_error"}"#.as_slice(),
            )));
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

/// JSON response with the no-store/no-cache headers RFC 6749 §5.1 requires on token replies.
/// The HTTP status and RFC 6749 error code for a failure of *our* backend, never the client's
/// request.
///
/// RFC 6749 5.2 defines both: `temporarily_unavailable` ("the authorization server is
/// currently unable to handle the request due to a temporary overloading or maintenance") with
/// 503, and `server_error` ("unexpected condition") with 500. Neither is a 4xx and neither is
/// a grant-specific code, so no client can read either as a verdict on what it sent.
fn oauth2_backend_failure(err: &anyhow::Error) -> (u16, &'static str) {
    if crate::llm::is_overload_error(err) {
        (503, "temporarily_unavailable")
    } else {
        (500, "server_error")
    }
}

/// The `error_description` a client may see: a category, never the error itself
/// (`crate::utils::wire_failure`).
fn oauth2_failure_description(err: &anyhow::Error) -> &'static str {
    crate::utils::WireFailure::classify(err).prefixed_text()
}

fn json_response(status: u16, body: String) -> Response<Full<Bytes>> {
    build_safe_response(
        status,
        [
            ("content-type".to_string(), "application/json".to_string()),
            ("cache-control".to_string(), "no-store".to_string()),
            ("pragma".to_string(), "no-cache".to_string()),
        ],
        body,
    )
}

/// The single JSON payload an OAuth2 executor produced, if the model emitted one.
///
/// `execute_action` tags each payload with [`OAUTH2_RESULT_KEY`] so the caller can tell an
/// approval from a denial. The old code just scanned for a `code` field, which meant an
/// `oauth2_error_response` — the model's only way to refuse — looked identical to "no
/// action at all" and was replaced by the hardcoded success default.
fn first_oauth2_payload(
    results: Vec<crate::llm::ActionResult>,
) -> Option<(String, serde_json::Value)> {
    results.into_iter().find_map(|result| {
        let crate::llm::ActionResult::Output(bytes) = result else {
            return None;
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let kind = value.get(OAUTH2_RESULT_KEY)?.as_str()?.to_string();
        Some((kind, value))
    })
}

/// OAuth2 authorization server
pub struct OAuth2Server;

impl OAuth2Server {
    /// Spawn the OAuth2 server with integrated LLM actions
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
        Log::new(Some(&status_tx)).info(format!("OAuth2 server listening on {}", local_addr));

        let protocol = Arc::new(OAuth2Protocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        info!("OAuth2 connection {} from {}", connection_id, remote_addr);

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        use serde_json::json;
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
                            protocol_info: ProtocolConnectionInfo::new(json!({
                                "recent_requests": []
                            })),
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

                            // Create a service that handles OAuth2 requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_oauth2_request(
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
                                error!("Error serving OAuth2 connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("OAuth2 connection {connection_id} closed"));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept OAuth2 connection: {}", e);
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

/// Handle a single OAuth2 request
async fn handle_oauth2_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OAuth2Protocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path();

    Log::new(Some(&status_tx)).debug(format!("OAuth2 request: {} {}", method, path));

    // Track request in connection info
    app_state
        .update_connection_stats(server_id, connection_id, None, None, Some(1), None)
        .await;

    // Route the request
    let response = match (method.clone(), path) {
        (Method::GET, "/authorize") | (Method::POST, "/authorize") => {
            handle_authorize_request(
                req,
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
            )
            .await
        }
        (Method::POST, "/token") => {
            handle_token_request(
                req,
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
            )
            .await
        }
        (Method::POST, "/introspect") => {
            handle_introspect_request(
                req,
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
            )
            .await
        }
        (Method::POST, "/revoke") => {
            handle_revoke_request(
                req,
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
            )
            .await
        }
        _ => {
            debug!("OAuth2: Unknown endpoint {} {}", method, path);
            Ok(json_response(
                404,
                json!({
                    "error": "invalid_request",
                    "error_description": "Unknown endpoint"
                })
                .to_string(),
            ))
        }
    };

    // Update connection stats
    let body_size = response
        .as_ref()
        .ok()
        .map(|resp| resp.body().size_hint().exact().unwrap_or(0))
        .unwrap_or(0);
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            None,
            Some(body_size),
            None,
            Some(1),
        )
        .await;

    let _ = status_tx.send("__UPDATE_UI__".to_string());
    response
}

/// Handle /authorize endpoint (RFC 6749 Section 3.1)
async fn handle_authorize_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OAuth2Protocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let uri = req.uri().clone();
    let method = req.method().clone();

    // Parse query parameters (GET) or form body (POST)
    let params = if method == Method::GET {
        parse_query_params(uri.query().unwrap_or(""))
    } else {
        // Read body for POST
        match read_body_limited(req.into_body()).await {
            Ok(body_bytes) => parse_query_params(&String::from_utf8_lossy(&body_bytes)),
            // Fail closed: an empty parameter map would reach the model as an authorization
            // request with no client_id at all.
            Err(()) => {
                return Ok(json_response(
                    413,
                    json!({
                        "error": "invalid_request",
                        "error_description": "request body too large"
                    })
                    .to_string(),
                ));
            }
        }
    };

    Log::new(Some(&status_tx)).debug(format!(
        "OAuth2 authorize request: {:?}",
        redact_params(&params)
    ));

    // Create LLM event
    let event = Event::new(
        &OAUTH2_AUTHORIZE_EVENT,
        json!({
            "response_type": params.get("response_type").cloned().unwrap_or_default(),
            "client_id": params.get("client_id").cloned().unwrap_or_default(),
            "redirect_uri": params.get("redirect_uri").cloned().unwrap_or_default(),
            "scope": params.get("scope").cloned().unwrap_or_default(),
            "state": params.get("state").cloned().unwrap_or_default(),
        }),
    );

    // Call LLM
    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        Ok(execution_result) => {
            let redirect_uri = params
                .get("redirect_uri")
                .cloned()
                .unwrap_or_else(|| "urn:ietf:wg:oauth:2.0:oob".to_string());
            let request_state = params.get("state").cloned();

            match first_oauth2_payload(execution_result.protocol_results) {
                // Approval: redirect with the code the model minted.
                Some((kind, payload)) if kind == "authorize" => {
                    let Some(code) = payload.get("code").and_then(|v| v.as_str()) else {
                        error!("OAuth2 authorize action carried no code; refusing the request");
                        return Ok(authorize_redirect(
                            &redirect_uri,
                            &[
                                ("error", "server_error"),
                                ("error_description", "authorization action produced no code"),
                            ],
                            request_state.as_deref(),
                        ));
                    };
                    info!("OAuth2 authorization approved for {}", redirect_uri);
                    Ok(authorize_redirect(
                        &redirect_uri,
                        &[("code", code)],
                        payload
                            .get("state")
                            .and_then(|v| v.as_str())
                            .or(request_state.as_deref()),
                    ))
                }
                // Denial. RFC 6749 §4.1.2.1: report it by redirecting with `error`, never by
                // handing back a code. The previous code looked for `code`, found none, and
                // used its `AUTH_CODE_123` default — so every refusal the model expressed was
                // delivered to the client as a successful authorization.
                Some((kind, payload)) if kind == "error" => {
                    let error = payload
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("access_denied");
                    let description = payload.get("error_description").and_then(|v| v.as_str());
                    warn!("OAuth2 authorization denied: {}", error);
                    let mut pairs = vec![("error", error)];
                    if let Some(d) = description {
                        pairs.push(("error_description", d));
                    }
                    Ok(authorize_redirect(
                        &redirect_uri,
                        &pairs,
                        request_state.as_deref(),
                    ))
                }
                // Anything else — no action, or an action that belongs to another endpoint.
                // Fail closed: an authorization server that mints a code when it was told
                // nothing is worse than one that errors.
                other => {
                    if let Some((kind, _)) = other {
                        warn!("OAuth2 authorize got a '{kind}' result; treating as no decision");
                    } else {
                        warn!("OAuth2 authorize produced no action; denying");
                    }
                    Ok(authorize_redirect(
                        &redirect_uri,
                        &[
                            ("error", "server_error"),
                            (
                                "error_description",
                                "no authorization decision was produced",
                            ),
                        ],
                        request_state.as_deref(),
                    ))
                }
            }
        }
        Err(e) => {
            // 4xx tells the client *it* got something wrong and the request is not worth
            // repeating. The client got nothing wrong: our backend did. RFC 6749 5.2 pairs
            // `server_error` with 500 and `temporarily_unavailable` with 503 for exactly this
            // distinction, and no branch here can produce an authorization code.
            let (status, code) = oauth2_backend_failure(&e);
            Log::new(Some(&status_tx)).error(format!(
                "OAuth2 /authorize failing with {} {}: {}",
                status, code, e
            ));
            Ok(json_response(
                status,
                json!({
                    "error": code,
                    "error_description": oauth2_failure_description(&e)
                })
                .to_string(),
            ))
        }
    }
}

/// Build the 302 that answers `/authorize`, percent-encoding every value.
///
/// `code`, `state` and the error strings all reach here unescaped — from the model, or
/// echoed from the client's own query string — so a value containing `&` or `=` used to
/// inject extra parameters into the callback URL.
fn authorize_redirect(
    redirect_uri: &str,
    pairs: &[(&str, &str)],
    state: Option<&str>,
) -> Response<Full<Bytes>> {
    let mut query: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect();
    if let Some(state) = state.filter(|s| !s.is_empty()) {
        query.push(format!("state={}", urlencoding::encode(state)));
    }

    let separator = if redirect_uri.contains('?') { "&" } else { "?" };
    let location = format!("{}{}{}", redirect_uri, separator, query.join("&"));

    build_safe_response(302, [("location".to_string(), location)], String::new())
}

/// Handle /token endpoint (RFC 6749 Section 3.2)
async fn handle_token_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OAuth2Protocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Parse form body
    let body_bytes = match read_body_limited(req.into_body()).await {
        Ok(bytes) => bytes,
        Err(()) => {
            return Ok(json_response(
                413,
                json!({
                    "error": "invalid_request",
                    "error_description": "request body too large"
                })
                .to_string(),
            ));
        }
    };

    let body_str = String::from_utf8_lossy(&body_bytes);
    let params = parse_query_params(&body_str);

    Log::new(Some(&status_tx)).debug(format!(
        "OAuth2 token request: {:?}",
        redact_params(&params)
    ));

    // Create LLM event
    let event = Event::new(
        &OAUTH2_TOKEN_EVENT,
        json!({
            "grant_type": params.get("grant_type").cloned().unwrap_or_default(),
            "code": params.get("code").cloned().unwrap_or_default(),
            "redirect_uri": params.get("redirect_uri").cloned().unwrap_or_default(),
            "client_id": params.get("client_id").cloned().unwrap_or_default(),
            "client_secret": params.get("client_secret").cloned().unwrap_or_default(),
            "refresh_token": params.get("refresh_token").cloned().unwrap_or_default(),
            "username": params.get("username").cloned().unwrap_or_default(),
            "password": params.get("password").cloned().unwrap_or_default(),
            "scope": params.get("scope").cloned().unwrap_or_default(),
        }),
    );

    // Call LLM
    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        Ok(execution_result) => match first_oauth2_payload(execution_result.protocol_results) {
            Some((kind, mut payload)) if kind == "token" => {
                strip_envelope(&mut payload);
                info!("OAuth2 token issued");
                Ok(json_response(200, payload.to_string()))
            }
            // A denial must carry an error status. RFC 6749 §5.2 says 400 (401 for
            // invalid_client); the old code returned the error body with 200, so a
            // conforming client parsed a refusal as a successful token response.
            Some((kind, mut payload)) if kind == "error" => {
                let status = status_or(payload.get("status_code"), 400);
                strip_envelope(&mut payload);
                warn!("OAuth2 token request denied ({status})");
                Ok(json_response(status, payload.to_string()))
            }
            // Fail closed: minting the old hardcoded `ACCESS_TOKEN_123` whenever the model
            // said nothing meant an LLM outage silently issued working credentials.
            other => {
                if let Some((kind, _)) = other {
                    warn!("OAuth2 token got a '{kind}' result; no token issued");
                } else {
                    warn!("OAuth2 token request produced no action; refusing");
                }
                Ok(json_response(
                    400,
                    json!({
                        "error": "invalid_grant",
                        "error_description": "no token decision was produced"
                    })
                    .to_string(),
                ))
            }
        },
        Err(e) => {
            // Never `invalid_grant` here. That code means the presented grant is bad, and a
            // conforming client reacts by discarding its refresh token and forcing the user to
            // sign in again - so an LLM outage would have logged every session out
            // permanently, and the damage would outlive the outage. A backend failure is a
            // server error, and clients retry those.
            let (status, code) = oauth2_backend_failure(&e);
            Log::new(Some(&status_tx)).error(format!(
                "OAuth2 /token failing with {} {}: {}",
                status, code, e
            ));
            Ok(json_response(
                status,
                json!({
                    "error": code,
                    "error_description": oauth2_failure_description(&e)
                })
                .to_string(),
            ))
        }
    }
}

/// Drop the internal routing fields before the payload goes on the wire.
fn strip_envelope(payload: &mut serde_json::Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.remove(OAUTH2_RESULT_KEY);
        obj.remove("status_code");
    }
}

/// Handle /introspect endpoint (RFC 7662)
async fn handle_introspect_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OAuth2Protocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Parse form body
    let body_bytes = match read_body_limited(req.into_body()).await {
        Ok(bytes) => bytes,
        // Not `{"active": false}`: that is a statement about the token, and nobody looked at
        // it. 413 says only that the request was refused.
        Err(()) => {
            return Ok(json_response(
                413,
                json!({
                    "error": "invalid_request",
                    "error_description": "request body too large"
                })
                .to_string(),
            ));
        }
    };

    let body_str = String::from_utf8_lossy(&body_bytes);
    let params = parse_query_params(&body_str);

    debug!(
        "OAuth2 introspect request: token_present={} hint={:?}",
        params.contains_key("token"),
        params.get("token_type_hint")
    );

    // Create LLM event
    let event = Event::new(
        &OAUTH2_INTROSPECT_EVENT,
        json!({
            "token": params.get("token").cloned().unwrap_or_default(),
            "token_type_hint": params.get("token_type_hint").cloned().unwrap_or_default(),
        }),
    );

    // Call LLM
    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        Ok(execution_result) => match first_oauth2_payload(execution_result.protocol_results) {
            Some((kind, mut payload)) if kind == "introspect" => {
                strip_envelope(&mut payload);
                info!("OAuth2 token introspected");
                Ok(json_response(200, payload.to_string()))
            }
            Some((kind, mut payload)) if kind == "error" => {
                let status = status_or(payload.get("status_code"), 400);
                strip_envelope(&mut payload);
                Ok(json_response(status, payload.to_string()))
            }
            // Fail closed. The old default was `{"active": true, ...}`, so any token at all
            // introspected as valid whenever the model produced nothing — a resource server
            // trusting this endpoint would have accepted every bearer token in existence.
            other => {
                if let Some((kind, _)) = other {
                    warn!("OAuth2 introspect got a '{kind}' result; reporting inactive");
                } else {
                    warn!("OAuth2 introspect produced no action; reporting inactive");
                }
                Ok(json_response(200, json!({"active": false}).to_string()))
            }
        },
        Err(e) => {
            // `{"active": false}` with a 200 is a *statement about the token* - it says the
            // authorization server looked and the token is not valid. Nobody looked. It is
            // fail-closed, which is why it was the previous answer, but it is
            // indistinguishable from the model deciding the token is bad, and a resource
            // server has no way to tell "revoked" from "we are broken". A 5xx says only that
            // the introspection did not happen, which every resource server already treats as
            // "cannot validate" and therefore also refuses.
            let (status, code) = oauth2_backend_failure(&e);
            Log::new(Some(&status_tx)).error(format!(
                "OAuth2 /introspect failing with {} {}: {}",
                status, code, e
            ));
            Ok(json_response(
                status,
                json!({
                    "error": code,
                    "error_description": oauth2_failure_description(&e)
                })
                .to_string(),
            ))
        }
    }
}

/// Handle /revoke endpoint (RFC 7009)
async fn handle_revoke_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OAuth2Protocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Parse form body
    let body_bytes = match read_body_limited(req.into_body()).await {
        Ok(bytes) => bytes,
        // RFC 7009 §2.2's blanket 200 is for a token the server *processed* and could not
        // find. This request was never processed, and a 200 would tell the client the token
        // is gone when nothing revoked it.
        Err(()) => {
            return Ok(json_response(
                413,
                json!({
                    "error": "invalid_request",
                    "error_description": "request body too large"
                })
                .to_string(),
            ));
        }
    };

    let body_str = String::from_utf8_lossy(&body_bytes);
    let params = parse_query_params(&body_str);

    debug!(
        "OAuth2 revoke request: token_present={} hint={:?}",
        params.contains_key("token"),
        params.get("token_type_hint")
    );

    // Create LLM event
    let event = Event::new(
        &OAUTH2_REVOKE_EVENT,
        json!({
            "token": params.get("token").cloned().unwrap_or_default(),
            "token_type_hint": params.get("token_type_hint").cloned().unwrap_or_default(),
        }),
    );

    // Call LLM
    if let Err(e) = call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        // RFC 7009 2.2.1 anticipates exactly this: "if the server responds with HTTP status
        // code 503, the client must assume the token still exists and may retry after a
        // reasonable delay". A 200 would tell the client the token is gone when nothing
        // processed the request - the model never saw it, so nothing was revoked, and the
        // client would stop trying.
        let (status, code) = oauth2_backend_failure(&e);
        Log::new(Some(&status_tx)).error(format!(
            "OAuth2 /revoke failing with {} {}: {}",
            status, code, e
        ));
        return Ok(json_response(
            status,
            json!({
                "error": code,
                "error_description": oauth2_failure_description(&e)
            })
            .to_string(),
        ));
    }

    info!("OAuth2 token revoked");
    // RFC 7009 §2.2: answer 200 whether or not the token was valid. Nothing the model says
    // changes this, which is why OAUTH2_REVOKE_EVENT declares .with_no_actions().
    Ok(build_safe_response(200, [], String::new()))
}

/// Parse `application/x-www-form-urlencoded` data (query string or POST body).
///
/// `+` means space in this encoding, which the previous version did not handle: a
/// `scope=read+write` body arrived at the model as the literal string `read+write`.
/// A pair whose key or value is not valid percent-encoding is skipped rather than
/// collapsed into an empty-string key, which previously made two malformed pairs
/// overwrite each other.
fn parse_query_params(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.replace('+', " "), value.replace('+', " "));
        let (Ok(key), Ok(value)) = (urlencoding::decode(&key), urlencoding::decode(&value)) else {
            debug!("OAuth2: skipping malformed form parameter {pair:?}");
            continue;
        };
        params.insert(key.into_owned(), value.into_owned());
    }
    params
}
