//! OCI Distribution Specification v1.1 registry server (pull path).
//!
//! Speaks the container-registry API that Docker Hub, GHCR, ECR and friends speak,
//! with the model deciding which repositories exist, what each manifest contains,
//! and which layers a client is told to fetch.
//!
//! Every `sha256:` value on the wire is computed here over the exact bytes being
//! sent — see the module docs in [`actions`] for how that is reconciled with
//! NetGet's rule that a protocol may not implement storage.
//!
//! Pull only. Push endpoints answer 405 `UNSUPPORTED`; a registry with no blob
//! store cannot honour an upload, and saying so is better than accepting one.

pub mod actions;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType};
use crate::server::connection::ConnectionId;
use crate::server::oci_registry::actions::{
    oci_error_body, sha256_digest, OciRegistryProtocol, DEFAULT_BLOB_MEDIA_TYPE,
};
use crate::state::app_state::AppState;

/// How `GET /v2/` is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionCheckMode {
    /// Answer 200 in-process, no LLM call. The default: every client sends this
    /// probe before every other request, and paying a model round-trip for it
    /// makes `crane ls` twice as expensive for no decision worth making.
    Auto,
    /// Raise `oci_version_check` and let the model decide — including demanding a
    /// bearer token via `send_oci_auth_challenge`.
    Llm,
}

impl VersionCheckMode {
    /// Parse the `version_check` startup parameter.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "static" => Ok(Self::Auto),
            "llm" | "model" => Ok(Self::Llm),
            other => Err(anyhow::anyhow!(
                "version_check must be \"auto\" or \"llm\", got \"{}\"",
                other
            )),
        }
    }
}

/// A parsed `/v2/…` request path.
///
/// Repository names may contain `/` (`library/alpine`, `a/b/c`), so the endpoint
/// suffix is located with `rfind` — the same way real registries disambiguate.
/// `/blobs/uploads` is matched before `/blobs/` because the former contains the
/// latter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OciRoute {
    /// `/v2/` — the API version probe.
    VersionCheck,
    /// `/v2/_catalog`
    Catalog,
    /// `/v2/{name}/tags/list`
    TagsList { name: String },
    /// `/v2/{name}/manifests/{reference}` — reference is a tag or a digest.
    Manifest { name: String, reference: String },
    /// `/v2/{name}/blobs/{digest}`
    Blob { name: String, digest: String },
    /// `/v2/{name}/blobs/uploads/…` — the push path, unimplemented.
    BlobUpload { name: String },
    /// `/v2/{name}/referrers/{digest}` — optional in the spec, unimplemented.
    Referrers { name: String },
}

/// Parse a request path into an [`OciRoute`], or `None` if it is not a `/v2/` route.
pub fn parse_v2_path(path: &str) -> Option<OciRoute> {
    if path == "/v2" || path == "/v2/" {
        return Some(OciRoute::VersionCheck);
    }
    let rest = path.strip_prefix("/v2/")?;
    if rest == "_catalog" || rest == "_catalog/" {
        return Some(OciRoute::Catalog);
    }
    if let Some(name) = rest.strip_suffix("/tags/list") {
        return Some(OciRoute::TagsList {
            name: name.to_string(),
        });
    }
    // Must precede the "/blobs/" arm: "/blobs/uploads/" contains "/blobs/".
    if let Some(i) = rest.rfind("/blobs/uploads") {
        return Some(OciRoute::BlobUpload {
            name: rest[..i].to_string(),
        });
    }
    if let Some(i) = rest.rfind("/manifests/") {
        let reference = &rest[i + "/manifests/".len()..];
        if !reference.is_empty() && !reference.contains('/') {
            return Some(OciRoute::Manifest {
                name: rest[..i].to_string(),
                reference: reference.to_string(),
            });
        }
    }
    if let Some(i) = rest.rfind("/blobs/") {
        let digest = &rest[i + "/blobs/".len()..];
        if !digest.is_empty() && !digest.contains('/') {
            return Some(OciRoute::Blob {
                name: rest[..i].to_string(),
                digest: digest.to_string(),
            });
        }
    }
    if let Some(i) = rest.rfind("/referrers/") {
        return Some(OciRoute::Referrers {
            name: rest[..i].to_string(),
        });
    }
    None
}

/// OCI repository-name grammar (spec §Pulling manifests):
/// `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*` per path component, `/`-separated.
///
/// Checked rather than assumed because a client that sends an uppercase name
/// expects `NAME_INVALID`, not a 200 for a repository that cannot exist.
pub fn is_valid_repository_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    name.split('/').all(|component| {
        !component.is_empty()
            && component
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && component
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && component.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            })
    })
}

/// Whether `reference` is a digest (`algo:hex`) rather than a tag.
pub fn is_digest_reference(reference: &str) -> bool {
    reference.contains(':')
}

/// Validate a digest string. Only sha256 is supported; the spec makes it the one
/// mandatory algorithm and anything else is honestly rejected rather than faked.
pub fn validate_sha256_digest(digest: &str) -> Result<(), String> {
    let (algo, hex_part) = digest
        .split_once(':')
        .ok_or_else(|| format!("digest '{}' is not in <algorithm>:<hex> form", digest))?;
    if algo != "sha256" {
        return Err(format!(
            "digest algorithm '{}' is not supported by this registry (only sha256)",
            algo
        ));
    }
    if hex_part.len() != 64 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "digest '{}' is not 64 lowercase hex characters",
            digest
        ));
    }
    if hex_part.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(format!("digest '{}' must be lowercase hex", digest));
    }
    Ok(())
}

/// OCI registry server that delegates every decision to the LLM.
pub struct OciRegistryServer;

impl OciRegistryServer {
    /// Bind and start serving.
    ///
    /// Bind failure is propagated so `server_startup` records `ServerStatus::Error`
    /// rather than reporting `Running` on a port this process does not hold.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> anyhow::Result<SocketAddr> {
        let version_check = match startup_params.as_ref() {
            Some(params) => match params.get_optional_string("version_check") {
                Ok(Some(s)) => VersionCheckMode::parse(&s)?,
                Ok(None) => VersionCheckMode::Auto,
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "OCI registry startup parameter error: {}",
                        e
                    ))
                }
            },
            None => VersionCheckMode::Auto,
        };

        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "OCI registry listening on {} (version_check={:?}, pull-only)",
            local_addr, version_check
        ));

        let protocol = Arc::new(OciRegistryProtocol::new());
        let task_registrar = app_state.clone();

        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        Log::new(Some(&status_tx)).debug(format!(
                            "OCI registry connection {} from {}",
                            connection_id, remote_addr
                        ));

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

                        let llm_client = llm_client.clone();
                        let app_state_conn = app_state.clone();
                        let status_tx_conn = status_tx.clone();
                        let protocol = protocol.clone();

                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let app_state_service = app_state_conn.clone();
                            let status_tx_service = status_tx_conn.clone();

                            let service = service_fn(move |req: Request<Incoming>| {
                                handle_oci_request(
                                    req,
                                    connection_id,
                                    remote_addr,
                                    llm_client.clone(),
                                    app_state_service.clone(),
                                    status_tx_service.clone(),
                                    protocol.clone(),
                                    server_id,
                                    version_check,
                                )
                            });

                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                debug!("OCI registry connection ended: {:?}", err);
                            }

                            app_state_conn
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_tx_conn.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept OCI registry connection: {}", e));
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

// ---------------------------------------------------------------------------
// Response construction
// ---------------------------------------------------------------------------

/// Build a response without ever panicking on model-influenced parts.
fn build(status: u16, headers: Vec<(&str, String)>, body: Vec<u8>) -> Response<Full<Bytes>> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in headers {
        // A CR/LF smuggled through a model-supplied header value would be a
        // response-splitting attempt; hyper rejects it and we drop that header
        // rather than losing the whole response.
        if value.contains(['\r', '\n']) {
            warn!("OCI registry: dropping header '{}' containing CR/LF", name);
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .header("Content-Length", body.len().to_string())
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|e| {
            error!("OCI registry: failed to build response: {}", e);
            Response::new(Full::new(Bytes::new()))
        })
}

/// Build an OCI error envelope response.
fn error_response(
    status: u16,
    code: &str,
    message: &str,
    detail: Option<Value>,
) -> Response<Full<Bytes>> {
    let body = oci_error_body(code, message, detail.as_ref());
    build(
        status,
        vec![
            ("Content-Type", "application/json".to_string()),
            (
                "Docker-Distribution-Api-Version",
                "registry/2.0".to_string(),
            ),
        ],
        body.into_bytes(),
    )
}

/// The fail-closed answer when the LLM call itself errored.
///
/// The peer gets a *category*, never the error: no backend URL, no model name, no
/// `anyhow` chain. The two categories are mapped onto different HTTP statuses so a
/// client backs off rather than recording a permanent fault — saturation is 503 +
/// `Retry-After`, which `crane`/`docker` retry, and anything else is 500. The full
/// error goes to the log and the status stream instead.
fn llm_error_response(err: &anyhow::Error) -> Response<Full<Bytes>> {
    let failure = crate::utils::WireFailure::classify(err);
    if failure.is_overloaded() {
        return build(
            503,
            vec![
                ("Content-Type", "application/json".to_string()),
                (
                    "Docker-Distribution-Api-Version",
                    "registry/2.0".to_string(),
                ),
                ("Retry-After", "1".to_string()),
            ],
            oci_error_body("UNKNOWN", failure.text(), None).into_bytes(),
        );
    }
    error_response(500, "UNKNOWN", failure.text(), None)
}

/// The fail-closed answer when the model produced nothing usable.
///
/// Deliberately not a plausible-looking empty catalog or a 404: an LLM outage and
/// a deliberate "this image does not exist" must not be indistinguishable, which
/// is the OAuth2 fail-open lesson. The model says no with `send_oci_error`; this
/// is what silence looks like.
fn no_answer_response(what: &str) -> Response<Full<Bytes>> {
    error_response(
        500,
        "UNKNOWN",
        &format!(
            "the registry backend returned no usable response for the {} request",
            what
        ),
        None,
    )
}

// ---------------------------------------------------------------------------
// Request handling
// ---------------------------------------------------------------------------

/// Approximate the bytes this request cost on the wire.
///
/// hyper hands us a parsed `Request`, so the original head is gone; this
/// reconstructs its size from the parts that survive. It is an estimate, and
/// deliberately so — the alternative is a `↓` counter frozen at 0, which reads as
/// "this peer sent nothing" rather than "we did not measure".
fn approximate_request_bytes(req: &Request<Incoming>) -> u64 {
    use hyper::body::Body;
    let head: usize = req.method().as_str().len()
        + req.uri().to_string().len()
        + 12 // " HTTP/1.1\r\n" plus the blank line terminating the head
        + req
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len() + 4)
            .sum::<usize>();
    head as u64 + req.body().size_hint().lower()
}

/// Record one request/response exchange against the connection's counters, then
/// return the response unchanged.
///
/// `update_connection_stats` is what the dashboard rail's `↓ ↑` columns and the
/// connection-scoped task prompts read, and it is what keeps `last_activity`
/// moving. Without it a busy OCI registry server draws every peer as idle having sent
/// and received nothing.
#[allow(clippy::too_many_arguments)]
async fn handle_oci_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    remote_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OciRegistryProtocol>,
    server_id: crate::state::ServerId,
    version_check: VersionCheckMode,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let bytes_received = approximate_request_bytes(&req);
    let response = handle_oci_request_inner(
        req,
        connection_id,
        remote_addr,
        llm_client,
        app_state.clone(),
        status_tx,
        protocol,
        server_id,
        version_check,
    )
    .await;

    let bytes_sent = response
        .as_ref()
        .ok()
        .and_then(|resp| {
            use hyper::body::Body;
            resp.body().size_hint().exact()
        })
        .unwrap_or(0);
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(bytes_received),
            Some(bytes_sent),
            Some(1),
            Some(1),
        )
        .await;

    response
}

#[allow(clippy::too_many_arguments)]
async fn handle_oci_request_inner(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    remote_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<OciRegistryProtocol>,
    server_id: crate::state::ServerId,
    version_check: VersionCheckMode,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    let accept = req
        .headers()
        .get_all(hyper::header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    let user_agent = req
        .headers()
        .get(hyper::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let authorization = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    Log::new(Some(&status_tx)).debug(format!("OCI {} {}", method, path));
    trace!(
        "OCI registry request: {} {} accept={:?} ua={}",
        method,
        path,
        accept,
        user_agent
    );

    let route = match parse_v2_path(&path) {
        Some(r) => r,
        None => {
            return Ok(error_response(
                404,
                "UNSUPPORTED",
                &format!(
                    "'{}' is not an OCI Distribution v2 endpoint; this registry serves /v2/",
                    path
                ),
                None,
            ))
        }
    };

    // Pull-only: every mutating method is refused up front, honestly.
    if !matches!(method, Method::GET | Method::HEAD) {
        Log::new(Some(&status_tx)).debug(format!(
            "OCI {} {} refused: this registry is pull-only",
            method, path
        ));
        return Ok(error_response(
            405,
            "UNSUPPORTED",
            "this registry implements the OCI Distribution v1.1 pull path only; \
             push (blob uploads and manifest PUT) is not supported",
            Some(json!({"method": method.as_str(), "path": path})),
        ));
    }

    match &route {
        OciRoute::BlobUpload { .. } => {
            return Ok(error_response(
                405,
                "UNSUPPORTED",
                "blob uploads are not supported: this registry has no storage and could not \
                 serve back what you pushed",
                None,
            ))
        }
        OciRoute::Referrers { .. } => {
            return Ok(error_response(
                404,
                "UNSUPPORTED",
                "the referrers API is not implemented by this registry",
                None,
            ))
        }
        _ => {}
    }

    // Repository-name validation before anything reaches the model.
    let repo_name = match &route {
        OciRoute::TagsList { name }
        | OciRoute::Manifest { name, .. }
        | OciRoute::Blob { name, .. } => Some(name.clone()),
        _ => None,
    };
    if let Some(name) = &repo_name {
        if !is_valid_repository_name(name) {
            return Ok(error_response(
                400,
                "NAME_INVALID",
                &format!("'{}' is not a valid OCI repository name", name),
                None,
            ));
        }
    }

    // Digest syntax validation, likewise.
    if let OciRoute::Blob { digest, .. } = &route {
        if let Err(msg) = validate_sha256_digest(digest) {
            return Ok(error_response(400, "DIGEST_INVALID", &msg, None));
        }
    }
    if let OciRoute::Manifest { reference, .. } = &route {
        if is_digest_reference(reference) {
            if let Err(msg) = validate_sha256_digest(reference) {
                return Ok(error_response(400, "DIGEST_INVALID", &msg, None));
            }
        }
    }

    // The version probe: answered in-process unless the operator asked for the model.
    if matches!(route, OciRoute::VersionCheck) && version_check == VersionCheckMode::Auto {
        trace!("OCI /v2/ answered in-process (version_check=auto)");
        return Ok(build(
            200,
            vec![
                ("Content-Type", "application/json".to_string()),
                (
                    "Docker-Distribution-Api-Version",
                    "registry/2.0".to_string(),
                ),
            ],
            b"{}".to_vec(),
        ));
    }

    if app_state.get_instruction(server_id).await.is_none() {
        error!("OCI registry: server {} not found", server_id);
        return Ok(error_response(
            500,
            "UNKNOWN",
            "registry instance is no longer running",
            None,
        ));
    }

    let query_params = parse_query(&query);
    let (event_type, event_data): (&'static EventType, Value) = match &route {
        OciRoute::VersionCheck => (
            &actions::OCI_VERSION_CHECK,
            json!({
                "method": method.as_str(),
                "path": path,
                "authorization": authorization,
                "user_agent": user_agent,
                "client": remote_addr.to_string(),
            }),
        ),
        OciRoute::Catalog => (
            &actions::OCI_CATALOG_REQUEST,
            json!({
                "method": method.as_str(),
                "path": path,
                "page_size": query_params.get("n"),
                "last": query_params.get("last"),
                "authorization": authorization,
                "user_agent": user_agent,
                "client": remote_addr.to_string(),
            }),
        ),
        OciRoute::TagsList { name } => (
            &actions::OCI_TAGS_REQUEST,
            json!({
                "method": method.as_str(),
                "path": path,
                "name": name,
                "page_size": query_params.get("n"),
                "last": query_params.get("last"),
                "authorization": authorization,
                "user_agent": user_agent,
                "client": remote_addr.to_string(),
            }),
        ),
        OciRoute::Manifest { name, reference } => (
            &actions::OCI_MANIFEST_REQUEST,
            json!({
                "method": method.as_str(),
                "path": path,
                "name": name,
                "reference": reference,
                "by_digest": is_digest_reference(reference),
                "accept": accept,
                "authorization": authorization,
                "user_agent": user_agent,
                "client": remote_addr.to_string(),
            }),
        ),
        OciRoute::Blob { name, digest } => (
            &actions::OCI_BLOB_REQUEST,
            json!({
                "method": method.as_str(),
                "path": path,
                "name": name,
                "digest": digest,
                "accept": accept,
                "authorization": authorization,
                "user_agent": user_agent,
                "client": remote_addr.to_string(),
            }),
        ),
        // Handled above.
        OciRoute::BlobUpload { .. } | OciRoute::Referrers { .. } => unreachable!(),
    };

    let event = Event::new(event_type, event_data);
    let llm_result = call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await;

    let execution = match llm_result {
        Ok(e) => e,
        Err(e) => {
            // Three outcomes must stay distinguishable in the log: the model
            // refused (`decision=model_reject`, logged in `build_from_action`),
            // the model answered nothing (`decision=fail_closed_no_action`), and
            // the LLM call errored (here). The tag is stable so an operator can
            // grep `decision=fail_closed_` for every request netget could not
            // answer. The error text is logged; it never reaches the socket.
            let failure = crate::utils::WireFailure::classify(&e);
            error!(
                "OCI registry {} {} decision=fail_closed_llm_error category={:?}: {}",
                method, path, failure, e
            );
            Log::new(Some(&status_tx)).warn(format!(
                "OCI registry {} {} decision=fail_closed_llm_error category={:?}",
                method, path, failure
            ));
            return Ok(llm_error_response(&e));
        }
    };

    for result in execution.protocol_results {
        if let Some(response) = build_from_action(result, &route, &status_tx) {
            return Ok(response);
        }
    }

    Log::new(Some(&status_tx)).warn(format!(
        "OCI registry {} {} decision=fail_closed_no_action - no usable action, refusing",
        method, path
    ));
    Ok(no_answer_response(route_label(&route)))
}

fn route_label(route: &OciRoute) -> &'static str {
    match route {
        OciRoute::VersionCheck => "version check",
        OciRoute::Catalog => "catalog",
        OciRoute::TagsList { .. } => "tag list",
        OciRoute::Manifest { .. } => "manifest",
        OciRoute::Blob { .. } => "blob",
        OciRoute::BlobUpload { .. } => "blob upload",
        OciRoute::Referrers { .. } => "referrers",
    }
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// Turn one action result into a response, or `None` if it is not one of ours so
/// the caller can keep scanning (a model may legitimately emit `set_memory`
/// alongside the real answer).
fn build_from_action(
    result: ActionResult,
    route: &OciRoute,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Option<Response<Full<Bytes>>> {
    let (name, data) = match result {
        ActionResult::Custom { name, data } => (name, data),
        _ => return None,
    };

    match name.as_str() {
        "oci_version_ok" => Some(build(
            200,
            vec![
                ("Content-Type", "application/json".to_string()),
                (
                    "Docker-Distribution-Api-Version",
                    "registry/2.0".to_string(),
                ),
            ],
            b"{}".to_vec(),
        )),

        "oci_auth_challenge" => {
            let realm = data.get("realm").and_then(|v| v.as_str()).unwrap_or("");
            let mut challenge = format!("Bearer realm=\"{}\"", realm);
            if let Some(service) = data.get("service").and_then(|v| v.as_str()) {
                challenge.push_str(&format!(",service=\"{}\"", service));
            }
            if let Some(scope) = data.get("scope").and_then(|v| v.as_str()) {
                challenge.push_str(&format!(",scope=\"{}\"", scope));
            }
            let message = data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("authentication required");
            Log::new(Some(status_tx)).debug(format!("OCI -> 401 auth challenge realm={}", realm));
            Some(build(
                401,
                vec![
                    ("Content-Type", "application/json".to_string()),
                    ("WWW-Authenticate", challenge),
                    (
                        "Docker-Distribution-Api-Version",
                        "registry/2.0".to_string(),
                    ),
                ],
                oci_error_body("UNAUTHORIZED", message, None).into_bytes(),
            ))
        }

        "oci_catalog" => {
            let repositories = data
                .get("repositories")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let body = json!({"repositories": repositories}).to_string();
            let mut headers = vec![
                ("Content-Type", "application/json".to_string()),
                (
                    "Docker-Distribution-Api-Version",
                    "registry/2.0".to_string(),
                ),
            ];
            if let Some(last) = data.get("next_last").and_then(|v| v.as_str()) {
                headers.push((
                    "Link",
                    format!("</v2/_catalog?last={}>; rel=\"next\"", last),
                ));
            }
            Log::new(Some(status_tx)).debug(format!(
                "OCI -> catalog ({} repositories)",
                repositories.as_array().map(|a| a.len()).unwrap_or(0)
            ));
            Some(build(200, headers, body.into_bytes()))
        }

        "oci_tags" => {
            let requested_name = match route {
                OciRoute::TagsList { name } => name.clone(),
                _ => String::new(),
            };
            let name = data
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or(requested_name);
            let tags = data.get("tags").cloned().unwrap_or_else(|| json!([]));
            let body = json!({"name": name, "tags": tags}).to_string();
            Log::new(Some(status_tx)).debug(format!(
                "OCI -> tag list for {} ({} tags)",
                name,
                tags.as_array().map(|a| a.len()).unwrap_or(0)
            ));
            Some(build(
                200,
                vec![
                    ("Content-Type", "application/json".to_string()),
                    (
                        "Docker-Distribution-Api-Version",
                        "registry/2.0".to_string(),
                    ),
                ],
                body.into_bytes(),
            ))
        }

        "oci_manifest" => {
            let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("");
            let digest = data.get("digest").and_then(|v| v.as_str()).unwrap_or("");
            let media_type = data
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or(actions::DEFAULT_MANIFEST_MEDIA_TYPE);

            // Re-hash the bytes we are about to write. `execute_manifest` already
            // computed this over the same string; recomputing here means the header
            // is derived from the wire bytes on this code path too, not carried
            // along in a field that a future refactor could desynchronise.
            let computed = sha256_digest(body.as_bytes());
            if computed != digest {
                error!(
                    "OCI manifest digest desynchronised: action said {} but the body hashes \
                     to {}; refusing",
                    digest, computed
                );
                return Some(error_response(
                    500,
                    "MANIFEST_INVALID",
                    "internal digest mismatch while building the manifest response",
                    None,
                ));
            }

            // Requested by digest? Then the content must be the content that digest
            // names. Fail closed — serving other bytes under that name is precisely
            // what content addressing exists to prevent.
            if let OciRoute::Manifest { reference, .. } = route {
                if is_digest_reference(reference) && reference != &computed {
                    Log::new(Some(status_tx)).warn(format!(
                        "OCI -> 404 MANIFEST_UNKNOWN: requested {} but content hashes to {}",
                        reference, computed
                    ));
                    return Some(error_response(
                        404,
                        "MANIFEST_UNKNOWN",
                        "the content supplied for this manifest does not hash to the requested \
                         digest, so it cannot be served under it",
                        Some(json!({"requested": reference, "computed": computed})),
                    ));
                }
            }

            Log::new(Some(status_tx)).debug(format!(
                "OCI -> manifest {} ({}, {} bytes)",
                computed,
                media_type,
                body.len()
            ));
            Some(build(
                200,
                vec![
                    ("Content-Type", media_type.to_string()),
                    ("Docker-Content-Digest", computed.clone()),
                    ("Etag", format!("\"{}\"", computed)),
                    (
                        "Docker-Distribution-Api-Version",
                        "registry/2.0".to_string(),
                    ),
                ],
                body.as_bytes().to_vec(),
            ))
        }

        "oci_blob" => {
            let bytes = match data.get("body_hex").and_then(|v| v.as_str()) {
                Some(h) => match hex::decode(h) {
                    Ok(b) => b,
                    Err(e) => {
                        error!("OCI blob: internal hex decode failed: {}", e);
                        return Some(error_response(
                            500,
                            "UNKNOWN",
                            "internal error decoding blob content",
                            None,
                        ));
                    }
                },
                None => return Some(no_answer_response("blob")),
            };
            let computed = sha256_digest(&bytes);
            let media_type = data
                .get("media_type")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_BLOB_MEDIA_TYPE);

            if let OciRoute::Blob { digest, .. } = route {
                if digest != &computed {
                    Log::new(Some(status_tx)).warn(format!(
                        "OCI -> 404 BLOB_UNKNOWN: requested {} but content hashes to {}",
                        digest, computed
                    ));
                    return Some(error_response(
                        404,
                        "BLOB_UNKNOWN",
                        "the content supplied for this blob does not hash to the requested \
                         digest, so it cannot be served under it",
                        Some(json!({"requested": digest, "computed": computed})),
                    ));
                }
            }

            Log::new(Some(status_tx)).debug(format!(
                "OCI -> blob {} ({} bytes)",
                computed,
                bytes.len()
            ));
            Some(build(
                200,
                vec![
                    ("Content-Type", media_type.to_string()),
                    ("Docker-Content-Digest", computed),
                    (
                        "Docker-Distribution-Api-Version",
                        "registry/2.0".to_string(),
                    ),
                ],
                bytes,
            ))
        }

        "oci_error" => {
            let code = data
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN");
            let message = data
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("request refused");
            let status = data
                .get("status")
                .and_then(|v| v.as_u64())
                .map(|s| s as u16)
                .unwrap_or(500);
            let detail = data.get("detail").cloned().filter(|d| !d.is_null());
            Log::new(Some(status_tx)).debug(format!(
                "OCI -> {} {} decision=model_reject: {}",
                status, code, message
            ));
            Some(error_response(status, code, message, detail))
        }

        other => {
            trace!("OCI registry: ignoring non-OCI action result '{}'", other);
            None
        }
    }
}
