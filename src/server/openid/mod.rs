//! OpenID Connect server implementation
//!
//! The LLM controls all OpenID Connect endpoints and generates responses including
//! discovery documents, authorization codes, JWT tokens, and user info.

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
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tracing::{debug, error, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::openid::actions::OpenIdProtocol;
use crate::state::app_state::AppState;

/// Largest request body this server will buffer.
///
/// Every endpoint here is reachable before any credential is checked, and the body is parsed
/// and handed to the model as prompt text, so the previous unbounded `collect()` let one
/// anonymous POST grow the process without limit. An OIDC form body is a handful of short
/// parameters; 64 KiB is far past anything a conforming relying party sends.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Narrow a model-supplied HTTP status to `u16` without wrapping.
///
/// `status as u16` on a `u64` truncates, and the truncation is the dangerous direction:
/// `65736` becomes `200`, so a `send_error_response` the model meant as a refusal arrives at
/// the relying party as the status it reads as success.
fn status_or(value: Option<&serde_json::Value>, default: u16) -> u16 {
    match value.and_then(|v| v.as_u64()) {
        Some(raw) => u16::try_from(raw)
            .ok()
            .filter(|s| (100..=599).contains(s))
            .unwrap_or_else(|| {
                warn!("OpenID: ignoring out-of-range status_code {raw}, using {default}");
                default
            }),
        None => default,
    }
}

/// OpenID Connect provider state
pub struct OpenIdState {
    /// Issuer URL
    pub issuer: Option<String>,
    /// Supported OAuth scopes
    pub supported_scopes: Vec<String>,
}

impl OpenIdState {
    pub fn new() -> Self {
        Self {
            issuer: None,
            supported_scopes: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
            ],
        }
    }
}

/// Determine OIDC endpoint type from request path
fn classify_endpoint(path: &str) -> &'static str {
    match path {
        "/.well-known/openid-configuration" => "discovery",
        "/authorize" => "authorization",
        "/token" => "token",
        "/userinfo" => "userinfo",
        "/jwks.json" | "/jwks" => "jwks",
        _ => "unknown",
    }
}

/// Parse `application/x-www-form-urlencoded` data (query string or POST body).
///
/// `+` means space in this encoding; the previous version left it literal, so a
/// `scope=openid+profile` body reached the model as `openid+profile`. A pair whose key or
/// value is not valid percent-encoding is skipped rather than turned into an empty-string
/// key, which previously made several malformed pairs overwrite one another.
fn parse_urlencoded(input: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in input.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let (key, value) = (key.replace('+', " "), value.replace('+', " "));
        let (Ok(key), Ok(value)) = (urlencoding::decode(&key), urlencoding::decode(&value)) else {
            debug!("OpenID: skipping malformed urlencoded parameter {pair:?}");
            continue;
        };
        params.insert(key.into_owned(), value.into_owned());
    }
    params
}

/// Build a response from parts that came from the model, without ever panicking.
///
/// `handle_llm_response` used to end in `.body(..).unwrap()` while feeding the builder a
/// `location` header assembled from the model's `redirect_uri`. hyper rejects a header
/// value containing CR/LF, `.body()` then returns `Err`, and the `unwrap()` took down the
/// connection task. Local copy of `http_common::handler::build_safe_response`, which the
/// `openid` feature cannot reach because `http_common` is gated on `feature = "http"`.
fn build_safe_response(
    status: u16,
    headers: impl IntoIterator<Item = (String, String)>,
    body: String,
) -> Response<Full<Bytes>> {
    let status_code = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!("OpenID: invalid HTTP status {status}, sending 500 instead");
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });

    let mut builder = Response::builder().status(status_code);
    for (name, value) in headers {
        match (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => builder = builder.header(n, v),
            _ => warn!("OpenID: dropping invalid response header {name:?}"),
        }
    }

    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|e| {
            error!("OpenID: failed to build response ({e}), sending bare 500");
            let mut fallback = Response::new(Full::new(Bytes::from_static(
                br#"{"error":"server_error"}"#.as_slice(),
            )));
            *fallback.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

/// Handle LLM response and process actions
async fn handle_llm_response(
    execution_result: crate::llm::actions::executor::ExecutionResult,
    status_tx: mpsc::UnboundedSender<String>,
    method: String,
    path: String,
    openid_state: Arc<RwLock<OpenIdState>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    debug!("LLM OpenID response received");

    // Display messages
    for msg in execution_result.messages {
        let _ = status_tx.send(msg);
    }

    // Default response
    let mut status_code = 200;
    let mut response_headers = HashMap::new();
    let mut response_body = String::new();
    let mut redirect_location: Option<String> = None;

    // Process protocol results
    for protocol_result in execution_result.protocol_results {
        match protocol_result {
            crate::llm::actions::protocol_trait::ActionResult::Custom { name, data } => {
                match name.as_str() {
                    "send_discovery_document" => {
                        // Build OpenID Connect discovery document
                        let mut discovery = json!({
                            "issuer": data["issuer"],
                            "authorization_endpoint": data["authorization_endpoint"],
                            "token_endpoint": data["token_endpoint"],
                            "userinfo_endpoint": data["userinfo_endpoint"],
                            "jwks_uri": data["jwks_uri"],
                            "response_types_supported": data
                                .get("supported_response_types")
                                .filter(|v| !v.is_null())
                                .cloned()
                                .unwrap_or(json!(["code", "id_token", "token id_token"])),
                            "subject_types_supported": ["public"],
                            // Advertisement only — NetGet signs nothing. The model chooses what
                            // to claim here so it can stay consistent with the id_token strings
                            // and JWKS it serves; RS256 stays the default for compatibility with
                            // relying parties that reject "none".
                            "id_token_signing_alg_values_supported": data
                                .get("id_token_signing_alg_values_supported")
                                .filter(|v| !v.is_null())
                                .cloned()
                                .unwrap_or(json!(["RS256"])),
                        });

                        if let Some(scopes) = data.get("supported_scopes").filter(|v| !v.is_null())
                        {
                            discovery["scopes_supported"] = scopes.clone();
                        }

                        response_body = serde_json::to_string_pretty(&discovery)
                            .unwrap_or_else(|_| discovery.to_string());
                        response_headers
                            .insert("content-type".to_string(), "application/json".to_string());
                        status_code = 200;
                    }
                    "send_authorization_response" => {
                        // A redirect carrying neither a grant nor an error is not an answer.
                        // Falling through with an empty parameter list produced a bare 302 to
                        // the client's callback, which set `redirect_location` and so passed
                        // the fail-closed check below while saying nothing at all.
                        if !["code", "error", "id_token", "access_token"]
                            .iter()
                            .any(|k| data.get(*k).and_then(|v| v.as_str()).is_some())
                        {
                            Log::new(Some(&status_tx)).warn(format!(
                                "OpenID {} {} decision=fail_closed_empty_authorization: \
                                 send_authorization_response carried no code, error or token",
                                method, path
                            ));
                            continue;
                        }
                        // Build redirect URL with query parameters
                        let redirect_uri = data["redirect_uri"].as_str().unwrap_or("");
                        let mut redirect_url = redirect_uri.to_string();
                        let mut params = Vec::new();

                        if let Some(code) = data.get("code").and_then(|v| v.as_str()) {
                            params.push(format!("code={}", urlencoding::encode(code)));
                        }
                        if let Some(state) = data.get("state").and_then(|v| v.as_str()) {
                            params.push(format!("state={}", urlencoding::encode(state)));
                        }
                        if let Some(error) = data.get("error").and_then(|v| v.as_str()) {
                            params.push(format!("error={}", urlencoding::encode(error)));
                        }
                        if let Some(error_desc) =
                            data.get("error_description").and_then(|v| v.as_str())
                        {
                            params.push(format!(
                                "error_description={}",
                                urlencoding::encode(error_desc)
                            ));
                        }

                        if !params.is_empty() {
                            let separator = if redirect_url.contains('?') { "&" } else { "?" };
                            redirect_url =
                                format!("{}{}{}", redirect_url, separator, params.join("&"));
                        }

                        redirect_location = Some(redirect_url.clone());
                        status_code = 302;
                        response_headers.insert("location".to_string(), redirect_url);
                    }
                    "send_token_response" => {
                        // Build OAuth token response
                        let mut token_response = json!({
                            "access_token": data["access_token"],
                            "token_type": data.get("token_type").cloned().unwrap_or(json!("Bearer")),
                        });

                        if let Some(id_token) = data.get("id_token") {
                            token_response["id_token"] = id_token.clone();
                        }
                        if let Some(refresh_token) = data.get("refresh_token") {
                            token_response["refresh_token"] = refresh_token.clone();
                        }
                        if let Some(expires_in) = data.get("expires_in") {
                            token_response["expires_in"] = expires_in.clone();
                        }
                        if let Some(scope) = data.get("scope") {
                            token_response["scope"] = scope.clone();
                        }

                        response_body = token_response.to_string();
                        response_headers
                            .insert("content-type".to_string(), "application/json".to_string());
                        response_headers
                            .insert("cache-control".to_string(), "no-store".to_string());
                        response_headers.insert("pragma".to_string(), "no-cache".to_string());
                        status_code = 200;
                    }
                    "send_userinfo_response" => {
                        // Build userinfo response
                        let mut userinfo = json!({
                            "sub": data["sub"],
                        });

                        if let Some(name) = data.get("name") {
                            userinfo["name"] = name.clone();
                        }
                        if let Some(email) = data.get("email") {
                            userinfo["email"] = email.clone();
                        }
                        if let Some(email_verified) = data.get("email_verified") {
                            userinfo["email_verified"] = email_verified.clone();
                        }
                        if let Some(picture) = data.get("picture") {
                            userinfo["picture"] = picture.clone();
                        }
                        if let Some(additional_claims) =
                            data.get("additional_claims").and_then(|v| v.as_object())
                        {
                            for (k, v) in additional_claims {
                                userinfo[k] = v.clone();
                            }
                        }

                        response_body = userinfo.to_string();
                        response_headers
                            .insert("content-type".to_string(), "application/json".to_string());
                        status_code = 200;
                    }
                    "send_jwks_response" => {
                        // Build JWKS response
                        let jwks = json!({
                            "keys": data.get("keys").cloned().unwrap_or(json!([]))
                        });

                        response_body = jwks.to_string();
                        response_headers
                            .insert("content-type".to_string(), "application/json".to_string());
                        status_code = 200;
                    }
                    "send_error_response" => {
                        // The model refusing is a real answer, and it is logged with its own
                        // `decision=` token so an operator can tell a deliberate denial apart
                        // from the model saying nothing and from the backend erroring.
                        Log::new(Some(&status_tx)).info(format!(
                            "OpenID {} {} decision=model_reject error={}",
                            method,
                            path,
                            data.get("error").and_then(|v| v.as_str()).unwrap_or("?")
                        ));
                        // Build OAuth error response
                        let error_response = json!({
                            "error": data["error"],
                            "error_description": data.get("error_description").cloned().unwrap_or(json!("")),
                        });

                        response_body = error_response.to_string();
                        response_headers
                            .insert("content-type".to_string(), "application/json".to_string());
                        status_code = status_or(data.get("status_code"), 400);
                    }
                    // The model reconfiguring the provider mid-flight. This used to fall into
                    // the `_` arm below: `configure_provider` was advertised, executed, and
                    // then discarded, so the issuer it set was never seen again. It produces
                    // no HTTP response of its own — deliberately, so a request answered with
                    // *only* a configure_provider still falls through to the fail-closed 500
                    // rather than an empty 200.
                    "configure_provider" => {
                        let mut state = openid_state.write().await;
                        if let Some(issuer) = data.get("issuer").and_then(|v| v.as_str()) {
                            Log::new(Some(&status_tx))
                                .info(format!("OpenID issuer set to {issuer}"));
                            state.issuer = Some(issuer.to_string());
                        }
                        if let Some(scopes) =
                            data.get("supported_scopes").and_then(|v| v.as_array())
                        {
                            let scopes: Vec<String> = scopes
                                .iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect();
                            if !scopes.is_empty() {
                                Log::new(Some(&status_tx))
                                    .info(format!("OpenID scopes set to {scopes:?}"));
                                state.supported_scopes = scopes;
                            }
                        }
                    }
                    _ => {
                        debug!("Unknown custom action: {}", name);
                    }
                }
            }
            _ => {}
        }
    }

    // The model produced nothing renderable — no action at all, or only actions this handler
    // does not know. That is not a sign-in: fail closed with RFC 6749 §5.2 `server_error`,
    // and describe it with a category only. The peer is a relying party and netget's
    // internals (the backend, the model, the retry machinery) are not its business; the
    // detail belongs in the log line below.
    if response_body.is_empty() && redirect_location.is_none() {
        response_body = json!({
            "error": "server_error",
            "error_description": crate::utils::WireFailure::Unavailable.prefixed_text(),
        })
        .to_string();
        response_headers.insert("content-type".to_string(), "application/json".to_string());
        status_code = 500;
        Log::new(Some(&status_tx)).error(format!(
            "OpenID failing {} {} with 500 server_error decision=no_answer \
             (the model returned no renderable OIDC response action)",
            method, path
        ));
    }

    // FileOnly: each send_*_response action's own log_template already reports the
    // outcome ("-> OIDC ...") to the TUI at INFO.
    Log::new(Some(&status_tx)).debug(format!(
        "OpenID {} {} -> {} ({} bytes{})",
        method,
        path,
        status_code,
        response_body.len(),
        if redirect_location.is_some() {
            ", redirect"
        } else {
            ""
        }
    ));

    Ok(build_safe_response(
        status_code,
        response_headers,
        response_body,
    ))
}

/// OpenID Connect server that uses LLM to handle all endpoints
pub struct OpenIdServer;

impl OpenIdServer {
    /// Spawn OpenID Connect server with LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> anyhow::Result<SocketAddr> {
        // Create shared OpenID state
        let openid_state = Arc::new(RwLock::new(OpenIdState::new()));
        let protocol = Arc::new(OpenIdProtocol::new());

        // Configure from startup params if provided
        if let Some(ref params) = startup_params {
            let mut state = openid_state.write().await;

            if let Some(issuer) = params.get_optional_string("issuer")? {
                Log::new(Some(&status_tx)).info(format!("OpenID issuer configured: {}", issuer));
                state.issuer = Some(issuer);
            }

            if let Some(scopes_array) = params.get_optional_array("supported_scopes")? {
                let scopes: Vec<String> = scopes_array
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                if !scopes.is_empty() {
                    Log::new(Some(&status_tx))
                        .info(format!("OpenID scopes configured: {:?}", scopes));
                    state.supported_scopes = scopes;
                }
            }
        }

        Log::new(Some(&status_tx)).info("Starting OpenID Connect server...");

        // Start HTTP server
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("OpenID server listening on {}", local_addr));

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
                            "OpenID connection {} from {}",
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
                            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                                "endpoint": None::<String>,
                                "authenticated": false,
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
                        let openid_state_clone = openid_state.clone();

                        // Spawn a task to handle this connection
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);

                            // Clone for service closure
                            let status_for_service = status_tx_clone.clone();
                            let app_state_for_service = app_state_clone.clone();
                            let openid_state_for_service = openid_state_clone.clone();

                            // Create a service that handles OpenID requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                let openid_state_clone = openid_state_for_service.clone();
                                handle_openid_request(
                                    req,
                                    connection_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                    openid_state_clone,
                                    server_id,
                                )
                            });

                            // Serve HTTP/1 on this connection
                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving OpenID connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("OpenID connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept OpenID connection: {}", e));
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

/// Handle a single OpenID Connect request
async fn handle_openid_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OpenIdProtocol>,
    openid_state: Arc<RwLock<OpenIdState>>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Extract request details (before consuming body)
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query_str = req.uri().query().map(|s| s.to_string());

    // Classify endpoint
    let endpoint_type = classify_endpoint(&path);

    // Extract headers
    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(name.to_string(), value_str.to_string());
        }
    }

    // Read body
    // Capped at MAX_REQUEST_BYTES, and refused rather than silently emptied: substituting an
    // empty body used to turn an over-limit POST into a well-formed request with no
    // parameters at all, which the model then answered as one.
    let body_bytes = match http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!(
                "OpenID {} {} decision=fail_closed_body_rejected (limit {} bytes): {}",
                method, path, MAX_REQUEST_BYTES, e
            ));
            return Ok(build_safe_response(
                413,
                [("content-type".to_string(), "application/json".to_string())],
                json!({
                    "error": "invalid_request",
                    "error_description": "request body too large"
                })
                .to_string(),
            ));
        }
    };
    let body_text = String::from_utf8_lossy(&body_bytes).to_string();

    // Parse query parameters
    let query_params = query_str
        .as_deref()
        .map(parse_urlencoded)
        .unwrap_or_default();

    // Parse form data if Content-Type is application/x-www-form-urlencoded
    let form_data = if headers
        .get("content-type")
        .map(|v| v.contains("application/x-www-form-urlencoded"))
        .unwrap_or(false)
    {
        parse_urlencoded(&body_text)
    } else {
        HashMap::new()
    };

    // DEBUG, FileOnly: the openid_request event's own log_template already reports the
    // request to the TUI at INFO.
    Log::new(Some(&status_tx)).debug(format!(
        "OpenID request: {} {} (endpoint: {}, {} bytes) from {:?}",
        method,
        path,
        endpoint_type,
        body_bytes.len(),
        connection_id
    ));

    // TRACE: Log full request details
    trace!("OpenID request headers:");
    for (name, value) in &headers {
        trace!("  {}: {}", name, value);
    }
    if !body_bytes.is_empty() {
        trace!("OpenID request body: {}", body_text);
    }
    if !query_params.is_empty() {
        trace!("OpenID query params: {:?}", query_params);
    }
    if !form_data.is_empty() {
        trace!("OpenID form data: {:?}", form_data);
    }

    // The `issuer` / `supported_scopes` startup parameters, and anything a later
    // `configure_provider` set, are reported to the model here. Until this existed they were
    // parsed into `OpenIdState` and then never read by anything — the handler took the state
    // as `_openid_state` — so both were advertised knobs that did nothing when turned.
    let (configured_issuer, configured_scopes) = {
        let state = openid_state.read().await;
        (state.issuer.clone(), state.supported_scopes.clone())
    };

    // Create event for LLM
    let event = Event::new(
        &*crate::server::openid::actions::OPENID_REQUEST_EVENT,
        json!({
            "method": method,
            "path": path,
            "query_params": query_params,
            "headers": headers,
            "body": if body_text.is_empty() { "" } else { &body_text },
            "form_data": form_data,
            "endpoint_type": endpoint_type,
            "configured_issuer": configured_issuer,
            "configured_scopes": configured_scopes,
        }),
    );

    // Call LLM to handle request
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
            handle_llm_response(execution_result, status_tx, method, path, openid_state).await
        }
        Err(e) => {
            // OpenID Connect inherits OAuth2's error codes (RFC 6749 5.2):
            // `temporarily_unavailable` with 503 when the backend is merely saturated,
            // `server_error` with 500 otherwise. Both are 5xx on purpose - a 4xx would tell
            // the relying party its request was at fault and stop it retrying - and neither
            // can carry an id_token, so no branch here can complete a sign-in.
            let overloaded = crate::llm::is_overload_error(&e);
            let (status, code) = if overloaded {
                (503, "temporarily_unavailable")
            } else {
                (500, "server_error")
            };
            // `decision=llm_error` distinguishes this from `decision=no_answer` (the model
            // answered, with nothing usable) and `decision=model_reject` (the model
            // deliberately refused). The error itself goes here and nowhere else.
            Log::new(Some(&status_tx)).error(format!(
                "OpenID failing {} {} with {} {} decision=llm_error overload={}: {}",
                method, path, status, code, overloaded, e
            ));

            Ok(build_safe_response(
                status,
                [("content-type".to_string(), "application/json".to_string())],
                json!({
                    "error": code,
                    "error_description": crate::utils::WireFailure::classify(&e).prefixed_text()
                })
                .to_string(),
            ))
        }
    }
}
