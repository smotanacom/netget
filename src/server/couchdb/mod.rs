//! CouchDB server implementation
//!
//! Implements a CouchDB-compatible HTTP/JSON REST API on port 5984.
//! The LLM controls all database operations, document management, views, changes feed,
//! and maintains "virtual" data through conversation context.

pub mod actions;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::server::connection::ConnectionId;
use crate::server::CouchDbProtocol;
use crate::state::app_state::AppState;
use crate::{console_error, console_info};

/// How much of a request body is read before the request is refused with 413.
///
/// Same value and same reasoning as `http_common::MAX_REQUEST_BODY_BYTES`, defined locally
/// because `server::http_common` is gated on `any(feature = "http", "http2", "oauth2", …)`
/// and `couchdb` is not in that list — the same exit `xmlrpc` takes. Adding `couchdb` to the
/// gate in `src/server/mod.rs` would let this share the constant.
///
/// The body is buffered whole and then embedded in an LLM prompt, so there is no legitimate
/// use for a large one: a model cannot read 8 MB, and every byte past a few kilobytes is cost
/// without benefit. Without a cap the buffer is whatever the peer chooses to send.
const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

/// CouchDB server that delegates all operations to LLM
pub struct CouchDbServer;

impl CouchDbServer {
    /// Spawn the CouchDB server with integrated LLM actions
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        enable_auth: bool,
        admin_username: String,
        admin_password: String,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        console_info!(
            status_tx,
            "CouchDB server listening on {} (auth: {})",
            local_addr,
            if enable_auth { "enabled" } else { "disabled" }
        );

        let protocol = Arc::new(CouchDbProtocol::new());
        let auth_config = Arc::new(AuthConfig {
            enabled: enable_auth,
            admin_username,
            admin_password,
        });

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        info!("CouchDB connection {} from {}", connection_id, remote_addr);
                        Log::new(Some(&status_tx))
                            .info(format!("CouchDB connection from {}", remote_addr));

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
                        let auth_config_clone = auth_config.clone();

                        // Spawn a task to handle this connection
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);

                            // Clone for service closure
                            let status_for_service = status_tx_clone.clone();
                            let app_state_for_service = app_state_clone.clone();

                            // Create a service that handles CouchDB requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                let auth_clone = auth_config_clone.clone();
                                handle_couchdb_request_with_llm(
                                    req,
                                    connection_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                    server_id,
                                    auth_clone,
                                )
                            });

                            // Serve HTTP/1 on this connection
                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving CouchDB connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("CouchDB connection {} closed", connection_id));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "Failed to accept CouchDB connection: {}", e);
                        break;
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

/// Authentication configuration
struct AuthConfig {
    enabled: bool,
    admin_username: String,
    admin_password: String,
}

/// Handle a single CouchDB request with LLM
#[allow(clippy::too_many_arguments)]
async fn handle_couchdb_request_with_llm(
    req: Request<Incoming>,
    _connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<CouchDbProtocol>,
    server_id: crate::state::ServerId,
    auth_config: Arc<AuthConfig>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    // Extract request details
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());

    // Extract authorization header
    let authorization = req
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Check authentication if enabled
    if auth_config.enabled {
        if let Some(auth_header) = &authorization {
            if !check_basic_auth(
                auth_header,
                &auth_config.admin_username,
                &auth_config.admin_password,
            ) {
                Log::new(Some(&status_tx)).debug("CouchDB authentication failed");
                return Ok(create_auth_required_response());
            }
        } else {
            // No auth header provided
            Log::new(Some(&status_tx)).debug("CouchDB authentication required");
            return Ok(create_auth_required_response());
        }
    }

    // Read the JSON body, bounded.
    //
    // `Incoming` has no default limit, so this used to buffer whatever an unauthenticated
    // peer chose to send — one `POST /db/_bulk_docs` was enough to exhaust the process. The
    // body is then embedded whole in an LLM prompt, so there is no legitimate large one
    // either. `Limited` errors as soon as the cap is passed rather than after buffering it,
    // and 413 says exactly what happened. This is the bound `http`, `http2` and `xmlrpc`
    // already had; CouchDB and Elasticsearch were the two HTTP servers still without it.
    //
    // An unreadable body must **not** fall through as empty, which is what the old `Err` arm
    // did: the handler was then shown a request with no body and answered it as though the
    // client had sent none, so a truncated bulk update read as an empty one.
    let limit = MAX_REQUEST_BODY_BYTES;
    let body_bytes = match http_body_util::Limited::new(req.into_body(), limit)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(
                "CouchDB {} {}: refusing request body ({}); limit is {} bytes",
                method, path, e, limit
            );
            console_error!(
                status_tx,
                "CouchDB {} {} → 413 (request body over {} bytes)",
                method,
                path,
                limit
            );
            let body = serde_json::json!({
                "error": "too_large",
                "reason": format!("netget: request body exceeds {} bytes", limit)
            })
            .to_string();
            return Ok(couchdb_response_builder(413)
                .body(Full::new(Bytes::from(body)))
                .unwrap_or_else(|_| {
                    couchdb_infallible_error(
                        hyper::StatusCode::PAYLOAD_TOO_LARGE,
                        r#"{"error":"too_large","reason":"netget: request body too large"}"#,
                    )
                }));
        }
    };

    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    debug!(
        "CouchDB request: {} {} ({} bytes)",
        method,
        uri,
        body_bytes.len()
    );
    Log::new(Some(&status_tx)).debug(format!(
        "CouchDB {} {} ({} bytes)",
        method,
        path,
        body_bytes.len()
    ));

    // Parse query parameters
    let query_params: HashMap<String, String> = query
        .as_ref()
        .map(|q| {
            form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    // Detect operation type from path and method
    let (operation, database, doc_id) = detect_couchdb_operation(&method, &path);

    if !body_str.is_empty() {
        Log::new(Some(&status_tx)).trace(format!("CouchDB request body: {}", body_str));
    }

    // Create CouchDB request event
    let event = crate::protocol::Event::new(
        &actions::COUCHDB_REQUEST_EVENT,
        serde_json::json!({
            "method": method,
            "path": path,
            "operation": operation,
            "database": database,
            "doc_id": doc_id,
            "query_params": query_params,
            "request_body": body_str,
            "authorization": authorization.map(|_| "***"),  // Don't log credentials
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
            // Look for CouchDB-specific response actions
            for result in execution_result.protocol_results {
                match result {
                    ActionResult::Custom { name, data } => {
                        if name == "couchdb_response" {
                            let status =
                                data.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                            let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("{}");
                            let etag = data.get("etag").and_then(|v| v.as_str());
                            let www_authenticate =
                                data.get("www_authenticate").and_then(|v| v.as_str());

                            debug!("CouchDB response: status={}", status);
                            let log = Log::new(Some(&status_tx));
                            log.debug(format!("CouchDB → {} response", status));
                            log.trace(format!("CouchDB response body: {}", body));

                            let mut builder = couchdb_response_builder(status);

                            if let Some(etag_value) = etag {
                                builder = header_or_skip(builder, "ETag", etag_value);
                            }

                            if let Some(www_auth) = www_authenticate {
                                builder = header_or_skip(builder, "WWW-Authenticate", www_auth);
                            }

                            return Ok(builder
                                .body(Full::new(Bytes::from(body.to_string())))
                                .unwrap_or_else(|_| {
                                    couchdb_infallible_error(
                                        hyper::StatusCode::INTERNAL_SERVER_ERROR,
                                        r#"{"error":"internal_server_error","reason":"netget: the response headers chosen for this reply could not be encoded"}"#,
                                    )
                                }));
                        }
                    }
                    _ => {
                        // Other actions don't affect HTTP response
                    }
                }
            }

            // The handler ran and produced no CouchDB response. This used to answer
            // 200 `{"ok": true}`, which is the exact claim the `Err` branch below refuses to
            // make: on a PUT it tells the client the document was written, on a `_session`
            // request it tells the client the login succeeded. Nothing here knows either.
            //
            // `no_response` is deliberately a different `error` string from the backend-failure
            // branch's `internal_server_error`, so a handler that stayed silent and a backend
            // that fell over are distinguishable in the client's error and in the log rather
            // than both reading as a generic 500.
            let kind = "no_response";
            Log::new(Some(&status_tx)).warn(
                "CouchDB handler produced no response for this request; answering 500 \
                 (decision=no_response) rather than a success it cannot vouch for"
                    .to_string(),
            );
            let error_response = serde_json::json!({
                "error": kind,
                "reason": "netget: the handler returned no CouchDB response for this request"
            })
            .to_string();

            Ok(couchdb_response_builder(500)
                .body(Full::new(Bytes::from(error_response)))
                .unwrap_or_else(|_| {
                    couchdb_infallible_error(
                        hyper::StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"error":"no_response","reason":"netget: the handler returned no CouchDB response for this request"}"#,
                    )
                }))
        }
        Err(e) => {
            // CouchDB reports errors as `{"error": ..., "reason": ...}` with a matching HTTP
            // status, and clients raise on it. What matters is that it is never a 2xx: a 200
            // with `{"rows": []}` means the view returned nothing, and a 201 with `{"ok":
            // true}` means the document was written - both statements about the database that
            // nothing here is in a position to make.
            let overloaded = crate::llm::is_overload_error(&e);
            let (status, kind) = if overloaded {
                (503u16, "unavailable")
            } else {
                (500u16, "internal_server_error")
            };
            error!(
                "LLM error for CouchDB request (overload={}, status {}): {}",
                overloaded, status, e
            );
            console_error!(status_tx, "CouchDB answering {} {}: {}", status, kind, e);

            let error_response = serde_json::json!({
                "error": kind,
                "reason": crate::utils::WireFailure::classify(&e).prefixed_text()
            })
            .to_string();

            Ok(couchdb_response_builder(status)
                .body(Full::new(Bytes::from(error_response)))
                .unwrap_or_else(|_| {
                    couchdb_infallible_error(
                        hyper::StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"error":"internal_server_error","reason":"netget: no response could be obtained"}"#,
                    )
                }))
        }
    }
}

/// A CouchDB error response that cannot itself fail to build.
///
/// Every `.body()` above can fail - it surfaces an invalid header value assembled earlier in
/// the builder - and every fallback for that used to be `Response::new(...)`, which is
/// **200 OK**. So a model answering `401` with a malformed `WWW-Authenticate` had its refusal
/// turned into a success, and the LLM-error branch answered 2xx in defiance of its own comment
/// that it never may. Setting the status on an already-constructed response leaves no builder
/// to fail, so a non-2xx here is guaranteed rather than hoped for.
fn couchdb_infallible_error(status: hyper::StatusCode, body: &str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = status;
    response
}

/// Start a CouchDB response with the standard headers and a validated status.
///
/// `status` originates in model output. `Response::builder().status()` rejects anything
/// outside 100-999 and the previous `.unwrap()` turned that into a panic, killing the
/// hyper connection task and leaving the client waiting on a socket that never answers.
/// `CouchDbProtocol::execute_action` already rejects out-of-range values with a message the
/// model sees; this is the belt-and-braces path.
fn couchdb_response_builder(status: u16) -> hyper::http::response::Builder {
    let status = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!(
            "Invalid CouchDB status code {}, sending 500 instead",
            status
        );
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("Server", "CouchDB/3.5.1 (NetGet LLM)")
}

/// Attach a header whose value came from model output.
///
/// `ETag` carries a document revision and `WWW-Authenticate` an auth realm, both chosen by
/// the model. `HeaderValue` rejects control characters and non-ASCII bytes, which
/// `Builder::body()` reports as an error - previously `.unwrap()`ed into a panic. Skip the
/// header instead and say so in the log.
fn header_or_skip(
    builder: hyper::http::response::Builder,
    name: &'static str,
    value: &str,
) -> hyper::http::response::Builder {
    match hyper::header::HeaderValue::from_str(value) {
        Ok(v) => builder.header(name, v),
        Err(_) => {
            error!(
                "Dropping CouchDB {} header: {:?} is not a valid HTTP header value",
                name, value
            );
            builder
        }
    }
}

/// Detect CouchDB operation from HTTP method and path
fn detect_couchdb_operation(method: &str, path: &str) -> (String, Option<String>, Option<String>) {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    match (method, parts.as_slice()) {
        // Root endpoint - server info
        ("GET", [""]) => ("server_info".to_string(), None, None),

        // Special endpoints
        ("GET", ["_all_dbs"]) => ("all_dbs".to_string(), None, None),
        ("GET", ["_active_tasks"]) => ("active_tasks".to_string(), None, None),
        ("GET", ["_uuids"]) => ("uuids".to_string(), None, None),
        ("POST", ["_replicate"]) => ("replicate".to_string(), None, None),
        ("GET", ["_session"]) => ("session".to_string(), None, None),

        // Database operations
        ("PUT", [db]) if !db.starts_with('_') => {
            ("db_create".to_string(), Some(db.to_string()), None)
        }
        ("DELETE", [db]) if !db.starts_with('_') => {
            ("db_delete".to_string(), Some(db.to_string()), None)
        }
        ("GET", [db]) if !db.starts_with('_') => {
            ("db_info".to_string(), Some(db.to_string()), None)
        }
        ("POST", [db]) if !db.starts_with('_') => {
            ("doc_create".to_string(), Some(db.to_string()), None)
        }

        // Database special endpoints
        ("GET", [db, "_all_docs"]) => ("all_docs".to_string(), Some(db.to_string()), None),
        ("POST", [db, "_all_docs"]) => ("all_docs".to_string(), Some(db.to_string()), None),
        ("POST", [db, "_bulk_docs"]) => ("bulk_docs".to_string(), Some(db.to_string()), None),
        ("GET", [db, "_changes"]) => ("changes".to_string(), Some(db.to_string()), None),
        ("POST", [db, "_ensure_full_commit"]) => {
            ("ensure_full_commit".to_string(), Some(db.to_string()), None)
        }
        ("POST", [db, "_compact"]) => ("compact".to_string(), Some(db.to_string()), None),
        ("POST", [db, "_purge"]) => ("purge".to_string(), Some(db.to_string()), None),

        // Design document operations (views)
        ("GET", [db, "_design", ddoc]) => (
            "design_get".to_string(),
            Some(db.to_string()),
            Some(format!("_design/{}", ddoc)),
        ),
        ("PUT", [db, "_design", ddoc]) => (
            "design_put".to_string(),
            Some(db.to_string()),
            Some(format!("_design/{}", ddoc)),
        ),
        ("DELETE", [db, "_design", ddoc]) => (
            "design_delete".to_string(),
            Some(db.to_string()),
            Some(format!("_design/{}", ddoc)),
        ),

        // View query
        ("GET", [db, "_design", ddoc, "_view", view]) => (
            "view_query".to_string(),
            Some(db.to_string()),
            Some(format!("_design/{}/{}", ddoc, view)),
        ),
        ("POST", [db, "_design", ddoc, "_view", view]) => (
            "view_query".to_string(),
            Some(db.to_string()),
            Some(format!("_design/{}/{}", ddoc, view)),
        ),

        // Document operations
        ("GET", [db, doc_id]) if !doc_id.starts_with('_') => (
            "doc_get".to_string(),
            Some(db.to_string()),
            Some(doc_id.to_string()),
        ),
        ("PUT", [db, doc_id]) if !doc_id.starts_with('_') => (
            "doc_put".to_string(),
            Some(db.to_string()),
            Some(doc_id.to_string()),
        ),
        ("DELETE", [db, doc_id]) if !doc_id.starts_with('_') => (
            "doc_delete".to_string(),
            Some(db.to_string()),
            Some(doc_id.to_string()),
        ),
        ("HEAD", [db, doc_id]) if !doc_id.starts_with('_') => (
            "doc_head".to_string(),
            Some(db.to_string()),
            Some(doc_id.to_string()),
        ),

        // Attachment operations
        ("GET", [db, doc_id, attachment]) if !doc_id.starts_with('_') => (
            "attachment_get".to_string(),
            Some(db.to_string()),
            Some(format!("{}/{}", doc_id, attachment)),
        ),
        ("PUT", [db, doc_id, attachment]) if !doc_id.starts_with('_') => (
            "attachment_put".to_string(),
            Some(db.to_string()),
            Some(format!("{}/{}", doc_id, attachment)),
        ),
        ("DELETE", [db, doc_id, attachment]) if !doc_id.starts_with('_') => (
            "attachment_delete".to_string(),
            Some(db.to_string()),
            Some(format!("{}/{}", doc_id, attachment)),
        ),

        // Replication endpoints
        ("GET", [db, "_local", doc_id]) => (
            "local_doc_get".to_string(),
            Some(db.to_string()),
            Some(format!("_local/{}", doc_id)),
        ),
        ("PUT", [db, "_local", doc_id]) => (
            "local_doc_put".to_string(),
            Some(db.to_string()),
            Some(format!("_local/{}", doc_id)),
        ),
        ("POST", [db, "_revs_diff"]) => ("revs_diff".to_string(), Some(db.to_string()), None),
        ("POST", [db, "_bulk_get"]) => ("bulk_get".to_string(), Some(db.to_string()), None),

        // Default
        _ => ("unknown".to_string(), None, None),
    }
}

/// Check HTTP Basic Authentication
fn check_basic_auth(auth_header: &str, expected_username: &str, expected_password: &str) -> bool {
    // Format: "Basic base64(username:password)"
    if !auth_header.starts_with("Basic ") {
        return false;
    }

    let encoded = &auth_header[6..]; // Skip "Basic "

    // Decode base64
    use base64::Engine;
    let decoded = match base64::engine::general_purpose::STANDARD.decode(encoded) {
        Ok(d) => d,
        Err(_) => return false,
    };

    let credentials = match String::from_utf8(decoded) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Split username:password
    let parts: Vec<&str> = credentials.splitn(2, ':').collect();
    if parts.len() != 2 {
        return false;
    }

    parts[0] == expected_username && parts[1] == expected_password
}

/// Create 401 Unauthorized response
fn create_auth_required_response() -> Response<Full<Bytes>> {
    let error_response = serde_json::json!({
        "error": "unauthorized",
        "reason": "Authentication required"
    })
    .to_string();

    Response::builder()
        .status(401)
        .header("Content-Type", "application/json")
        .header("Server", "CouchDB/3.5.1 (NetGet LLM)")
        .header("WWW-Authenticate", "Basic realm=\"CouchDB\"")
        .body(Full::new(Bytes::from(error_response)))
        .unwrap()
}
