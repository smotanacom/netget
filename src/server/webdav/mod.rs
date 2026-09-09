//! WebDAV server (RFC 4918) with an LLM-supplied virtual filesystem.
//!
//! The server owns HTTP/1.1 framing (via `hyper`) and the DAV:multistatus XML; the model owns
//! everything a client can observe. There is **no filesystem** behind this protocol — not on
//! disk, not in memory. `PROPFIND` lists what the model says is there, `GET` returns what the
//! model says the file contains, and `PUT`/`MKCOL`/`DELETE`/`COPY`/`MOVE` succeed or fail
//! exactly as the model decides.
//!
//! This replaced an implementation that handed every request to `dav_server::memfs::MemFs` — a
//! real read/write filesystem inside the process — and dropped the `OllamaClient` on startup,
//! so the server instruction was read by nobody. See `src/server/webdav/CLAUDE.md` for why
//! implementing `dav_server::fs::DavFileSystem` against the model was rejected in favour of
//! answering the verbs directly.
//!
//! Three methods never reach the model, because they are handshakes with no content to decide:
//! `OPTIONS` (capability advertisement), `LOCK` and `UNLOCK` (a synthetic, never-enforced lock
//! token, so clients that refuse to write without one can proceed). Everything else raises
//! `webdav_request`.
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::WebDavProtocol;
use crate::state::app_state::AppState;
use actions::WEBDAV_REQUEST_EVENT;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

/// Methods a client may use against this server, advertised in `Allow` on `OPTIONS`.
const ALLOWED_METHODS: &str = "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, \
                               COPY, MOVE, LOCK, UNLOCK";

/// Largest request body this server will read, in bytes.
///
/// WebDAV has no authentication step, so anything a peer can reach it can also `PUT` to. The
/// body used to be read with a bare `.collect()`, which is unbounded: a single request decided
/// how much memory this process allocated, and the whole of it was then interpolated into the
/// model's prompt. 8 MiB is far past any real `PROPFIND`/`PROPPATCH` body and still bounded.
/// A larger body gets `413 Payload Too Large` before any of it reaches the model.
const MAX_REQUEST_BODY: usize = 8 * 1024 * 1024;

/// WebDAV server whose entire filesystem is supplied by the LLM
pub struct WebDavServer;

impl WebDavServer {
    /// Spawn WebDAV server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        info!("WebDAV server starting on {}", listen_addr);

        let protocol = Arc::new(WebDavProtocol::new());

        // Bind before spawning. Binding inside the accept task made every failure invisible:
        // the task logged and returned while spawn_with_llm_actions had already answered Ok,
        // so the server sat in Running with no socket. It also meant port 0 was reported back
        // to the caller verbatim instead of the port the kernel actually chose.
        let listener = crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr)
            .await
            .with_context(|| format!("Failed to bind WebDAV listener on {}", listen_addr))?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("WebDAV server listening on {}", local_addr));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        Log::new(Some(&status_tx)).debug(format!(
                            "WebDAV connection {} from {}",
                            connection_id, peer_addr
                        ));

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr: peer_addr,
                            local_addr,
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

                        let llm_clone = llm_client.clone();
                        let app_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let app_for_close = app_state.clone();
                        let status_for_close = status_tx.clone();

                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);

                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm = llm_clone.clone();
                                let state = app_clone.clone();
                                let status = status_clone.clone();
                                let proto = protocol_clone.clone();
                                handle_webdav_request(
                                    req,
                                    connection_id,
                                    server_id,
                                    llm,
                                    state,
                                    status,
                                    proto,
                                )
                            });

                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("WebDAV connection error: {:?}", err);
                            }

                            app_for_close
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_for_close))
                                .info(format!("WebDAV connection {} closed", connection_id));
                            let _ = status_for_close.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept WebDAV connection: {}", e));
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

/// Everything the server extracted from one request before deciding what to do with it.
struct WebDavRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Bytes,
    /// The peer sent (or declared) more body than `MAX_REQUEST_BODY`. The request is refused
    /// with 413 without a model call: there is no content decision to make about a body the
    /// server declined to read.
    body_too_large: bool,
}

async fn extract_request(req: Request<Incoming>) -> WebDavRequest {
    let method = req.method().to_string().to_ascii_uppercase();
    // Percent-decoded, query string dropped: a WebDAV path is a resource name, and the model
    // is asked to echo it back into `send_webdav_listing`, so it must be the human-readable
    // form rather than the wire form.
    let raw_path = req.uri().path().to_string();
    let path = actions::percent_decode(&raw_path);

    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(v) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), v.to_string());
        }
    }

    // Refuse on the declared length before reading a byte where the client gave one, then
    // bound the stream itself for the chunked case where it did not. Checking only the
    // remainder-after-headers, or only the declared value, is the mistake that let thirty
    // bytes on the wire buffer toward gigabytes elsewhere in this tree.
    let declared_too_large = headers
        .get("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|len| len > MAX_REQUEST_BODY as u64);

    if declared_too_large {
        return WebDavRequest {
            method,
            path,
            headers,
            body: Bytes::new(),
            body_too_large: true,
        };
    }

    let limited = http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BODY);
    let (body, body_too_large) = match limited.collect().await {
        Ok(collected) => (collected.to_bytes(), false),
        Err(e) => {
            // `Limited` reports the cap as an error indistinguishable from a transport
            // failure, and both end the request the same way here: nothing usable was read.
            warn!("WebDAV: request body rejected: {}", e);
            (Bytes::new(), true)
        }
    };

    WebDavRequest {
        method,
        path,
        headers,
        body,
        body_too_large,
    }
}

/// Build a response without ever panicking.
///
/// Status and headers here are model-influenced, so an out-of-range status becomes 500 and an
/// illegal header (notably one containing CR/LF, i.e. a response-splitting attempt) is dropped
/// individually rather than taken down the connection task with a panic.
fn build_safe_response(
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
) -> Response<Full<Bytes>> {
    let status_code = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!(
            "WebDAV: invalid HTTP status {} (must be 100-599), sending 500 instead",
            status
        );
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });

    // Advertised on every response: clients decide whether a server speaks WebDAV from this
    // header, not from the multistatus body.
    let mut builder = Response::builder()
        .status(status_code)
        .header("DAV", "1, 2")
        .header("MS-Author-Via", "DAV");

    for (name, value) in headers {
        match (
            hyper::header::HeaderName::from_bytes(name.as_bytes()),
            hyper::header::HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => builder = builder.header(n, v),
            _ => warn!(
                "WebDAV: dropping invalid response header {:?} (name or value is not legal HTTP)",
                name
            ),
        }
    }

    builder
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|e| {
            error!("WebDAV: failed to build response ({}), sending bare 500", e);
            let mut fallback =
                Response::new(Full::new(Bytes::from_static(b"Internal Server Error")));
            *fallback.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

/// Turn the model's action results into an HTTP response.
///
/// **Fails closed.** If nothing usable came back — the model emitted no WebDAV action, or the
/// executor rejected the one it did emit — the client gets `503 Service Unavailable`, not a
/// permissive default. A model that means to allow something says so with
/// `send_webdav_status`; silence is not consent, and 503 is deliberately distinct from any
/// status the model can choose, so "it refused" and "it never answered" are never confused in
/// a packet capture or an access log.
fn build_webdav_response(
    results: Vec<ActionResult>,
    method: &str,
    path: &str,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Response<Full<Bytes>> {
    fn find(results: &[ActionResult]) -> Option<serde_json::Value> {
        for result in results {
            match result {
                ActionResult::Output(bytes) => {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
                        if v.get("status").is_some() {
                            return Some(v);
                        }
                    }
                }
                ActionResult::Multiple(inner) => {
                    if let Some(v) = find(inner) {
                        return Some(v);
                    }
                }
                _ => {}
            }
        }
        None
    }

    let Some(payload) = find(&results) else {
        Log::new(Some(status_tx)).warn(format!(
            "WebDAV {} {} -> 503 (model produced no send_webdav_* action)",
            method, path
        ));
        return build_safe_response(
            503,
            vec![("Content-Type".to_string(), "text/plain".to_string())],
            "WebDAV server has no answer for this request".to_string(),
        );
    };

    let status = payload
        .get("status")
        .and_then(|v| v.as_u64())
        .unwrap_or(500)
        .min(599) as u16;
    let headers = payload
        .get("headers")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let body = payload
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Log::new(Some(status_tx)).debug(format!(
        "WebDAV {} {} -> {} ({} bytes)",
        method,
        path,
        status,
        body.len()
    ));

    build_safe_response(status, headers, body)
}

/// Handle one request, maintaining the per-connection counters on every exit path.
///
/// Semantics match the other hyper-based servers: one "packet" is one HTTP message, and the
/// byte counts are message bodies only — hyper has parsed the request line and headers away
/// before this sees them. `last_activity` matters beyond bookkeeping:
/// `ServerInstance::cleanup_old_connections` evicts connections idle for 10s, so a keep-alive
/// WebDAV session (clients pipeline PROPFIND/GET aggressively) would otherwise vanish from the
/// state map while still serving.
async fn handle_webdav_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<WebDavProtocol>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    app_state
        .update_connection_stats(server_id, connection_id, None, None, Some(1), None)
        .await;

    let response = handle_webdav_request_inner(
        req,
        connection_id,
        server_id,
        llm_client,
        app_state.clone(),
        status_tx,
        protocol,
    )
    .await;

    let bytes_sent = {
        use hyper::body::Body;
        response.body().size_hint().exact().unwrap_or(0)
    };
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            None,
            Some(bytes_sent),
            None,
            Some(1),
        )
        .await;

    Ok(response)
}

async fn handle_webdav_request_inner(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<WebDavProtocol>,
) -> Response<Full<Bytes>> {
    let request = extract_request(req).await;

    if request.body_too_large {
        Log::new(Some(&status_tx)).warn(format!(
            "WebDAV {} {} -> 413 (body over {} bytes, no LLM call)",
            request.method, request.path, MAX_REQUEST_BODY
        ));
        return build_safe_response(
            413,
            vec![("Content-Type".to_string(), "text/plain".to_string())],
            "WebDAV request body too large".to_string(),
        );
    }

    Log::new(Some(&status_tx)).debug(format!(
        "WebDAV request: {} {} ({} bytes)",
        request.method,
        request.path,
        request.body.len()
    ));
    for (name, value) in &request.headers {
        trace!("WebDAV header: {}: {}", name, value);
    }

    if !request.body.is_empty() {
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(request.body.len() as u64),
                None,
                None,
                None,
            )
            .await;
    }

    // Handshake methods: answered by the server, no model call. These carry no content
    // decision — OPTIONS advertises what this build can parse, and LOCK/UNLOCK exist so that
    // clients which refuse to write without a lock (macOS Finder, the Windows redirector) can
    // proceed. The lock is synthetic and never enforced; nothing consults it.
    match request.method.as_str() {
        "OPTIONS" => {
            Log::new(Some(&status_tx)).debug(format!(
                "WebDAV OPTIONS {} -> 200 (no LLM call)",
                request.path
            ));
            return build_safe_response(
                200,
                // Content-Length is left to hyper: the empty body already yields 0, and
                // setting it here as well risks a duplicated header.
                vec![("Allow".to_string(), ALLOWED_METHODS.to_string())],
                String::new(),
            );
        }
        "LOCK" => {
            let token = format!("opaquelocktoken:{}", uuid::Uuid::new_v4());
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
                 <D:prop xmlns:D=\"DAV:\"><D:lockdiscovery><D:activelock>\
                 <D:locktype><D:write/></D:locktype>\
                 <D:lockscope><D:exclusive/></D:lockscope>\
                 <D:depth>infinity</D:depth>\
                 <D:timeout>Second-3600</D:timeout>\
                 <D:locktoken><D:href>{}</D:href></D:locktoken>\
                 </D:activelock></D:lockdiscovery></D:prop>",
                token
            );
            Log::new(Some(&status_tx)).debug(format!(
                "WebDAV LOCK {} -> 200 (synthetic token, never enforced, no LLM call)",
                request.path
            ));
            return build_safe_response(
                200,
                vec![
                    (
                        "Content-Type".to_string(),
                        "application/xml; charset=utf-8".to_string(),
                    ),
                    ("Lock-Token".to_string(), format!("<{}>", token)),
                ],
                body,
            );
        }
        "UNLOCK" => {
            Log::new(Some(&status_tx)).debug(format!(
                "WebDAV UNLOCK {} -> 204 (no LLM call)",
                request.path
            ));
            return build_safe_response(204, Vec::new(), String::new());
        }
        _ => {}
    }

    // Everything else is a content decision, and belongs to the model.
    let body_is_binary = std::str::from_utf8(&request.body).is_err();
    let body_text = String::from_utf8_lossy(&request.body);
    let mut event_data = serde_json::json!({
        "method": request.method,
        "path": request.path,
        "headers": request.headers,
        "body": body_text.as_ref(),
        "body_bytes": request.body.len(),
    });
    if body_is_binary {
        event_data["body_is_binary"] = serde_json::Value::Bool(true);
    }
    if let Some(depth) = request.headers.get("depth") {
        event_data["depth"] = serde_json::Value::String(depth.clone());
    }
    if let Some(destination) = request.headers.get("destination") {
        event_data["destination"] =
            serde_json::Value::String(actions::percent_decode(&destination_path(destination)));
    }
    if let Some(overwrite) = request.headers.get("overwrite") {
        event_data["overwrite"] = serde_json::Value::String(overwrite.to_ascii_uppercase());
    }

    let event = Event::new(&WEBDAV_REQUEST_EVENT, event_data);

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
            for msg in execution_result.messages {
                let _ = status_tx.send(msg);
            }
            build_webdav_response(
                execution_result.protocol_results,
                &request.method,
                &request.path,
                &status_tx,
            )
        }
        Err(e) => {
            // An LLM failure is the server's fault, not the client's request being wrong, and
            // it is distinct from both a refusal (whatever status the model chose) and a
            // no-answer (503).
            Log::new(Some(&status_tx)).warn(format!(
                "WebDAV {} {} -> 500 (LLM error: {})",
                request.method, request.path, e
            ));
            build_safe_response(
                500,
                vec![("Content-Type".to_string(), "text/plain".to_string())],
                "WebDAV server error".to_string(),
            )
        }
    }
}

/// Reduce a `Destination` header to a path.
///
/// RFC 4918 requires an absolute URI (`http://host/path`), but clients send both forms, and
/// only the path is meaningful to a model that has no idea what host it is being addressed as.
fn destination_path(destination: &str) -> String {
    match destination.find("://") {
        Some(scheme_end) => match destination[scheme_end + 3..].find('/') {
            Some(path_start) => destination[scheme_end + 3 + path_start..].to_string(),
            None => "/".to_string(),
        },
        None => destination.to_string(),
    }
}
