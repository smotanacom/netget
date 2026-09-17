//! Elasticsearch/OpenSearch server implementation
//!
//! Implements an Elasticsearch-compatible HTTP/JSON API on port 9200.
//! The LLM controls search queries, indexing, and maintains "virtual" data through conversation context.

pub mod actions;

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
use tracing::{debug, error, info, warn};

use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::server::connection::ConnectionId;
use crate::server::ElasticsearchProtocol;
use crate::state::app_state::AppState;
use crate::{console_error, console_info};

/// How much of a request body is read before the request is refused with 413.
///
/// Same value and same reasoning as `http_common::MAX_REQUEST_BODY_BYTES`, defined locally
/// because `server::http_common` is gated on `any(feature = "http", "http2", "oauth2", …)`
/// and `elasticsearch` is not in that list — the same exit `xmlrpc` takes. Adding
/// `elasticsearch` to the gate in `src/server/mod.rs` would let this share the constant.
pub const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;

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
/// nginx's `keepalive_timeout` default. Elasticsearch's official clients — the Java `RestClient`,
/// `elasticsearch-py` — hold a *pool* of connections for the life of the application, which looks
/// like an argument for minutes, and is not: a scroll, a `?wait_for_completion` request or a bulk
/// batch in progress is a request **in flight**, and the watchdog reads
/// [`ConnectionActivity`](crate::server::accept_bounded::ConnectionActivity), which reports a
/// connection with work in flight — a model round-trip, or an event a `manual` rule parked for a
/// human — as not idle at all. What this bound measures is a pooled connection with nothing
/// outstanding, and reopening one of those costs a loopback handshake.
const IDLE_BETWEEN_REQUESTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(75);

/// Concurrent connections this server admits.
///
/// Below the shared `DEFAULT_MAX_CONNECTIONS` of 256 on purpose: each admitted connection may
/// buffer one body of up to [`MAX_REQUEST_BODY_BYTES`] (8 MiB), which is the largest
/// per-connection cost in netget's HTTP family, and the cap is what turns that per-connection
/// bound into a total one. 128 holds the worst case to the same ~1 GiB ceiling the 4 MiB and
/// 64 KiB servers reach at 256, so the number that varies is the one that has to.
const MAX_CONNECTIONS: usize = 128;

/// What a peer over [`MAX_CONNECTIONS`] is told before the socket closes.
///
/// `503 Service Unavailable` with a `Retry-After`, written directly onto the socket because the
/// peer has not sent a request line for hyper to answer. Fixed bytes: nothing derived from an
/// error reaches the wire (see `crate::utils::wire_failure`).
const CONNECTION_CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    Content-Length: 0\r\nRetry-After: 5\r\nConnection: close\r\n\r\n";

/// Elasticsearch server that delegates search/index operations to LLM
pub struct ElasticsearchServer;

impl ElasticsearchServer {
    /// Spawn the Elasticsearch server with integrated LLM actions
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
        console_info!(
            status_tx,
            "Elasticsearch server listening on {}",
            local_addr
        );

        let protocol = Arc::new(ElasticsearchProtocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "Elasticsearch",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((stream, remote_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        info!(
                            "Elasticsearch connection {} from {}",
                            connection_id, remote_addr
                        );
                        Log::new(Some(&status_tx))
                            .info(format!("Elasticsearch connection from {}", remote_addr));

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
                        let status_tx_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        // Spawn a task to handle this connection
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
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

                                // Clone for service closure
                                let status_for_service = status_tx_clone.clone();
                                let app_state_for_service = app_state_clone.clone();

                                // Tracks whether this connection is answering anything. A
                                // request waiting on the model, or parked for a human by a
                                // `manual` rule, holds the count above zero, so the idle watchdog
                                // below cannot close the connection the answer belongs to however
                                // long it takes - only genuine silence counts.
                                let activity = std::sync::Arc::new(
                                    crate::server::accept_bounded::ConnectionActivity::new(),
                                );
                                let activity_for_service = std::sync::Arc::clone(&activity);

                                // Create a service that handles Elasticsearch requests with LLM
                                let service = service_fn(move |req: Request<Incoming>| {
                                    let llm_clone = llm_client_clone.clone();
                                    let state_clone = app_state_for_service.clone();
                                    let status_clone = status_for_service.clone();
                                    let protocol_clone = protocol_clone.clone();
                                    let activity = std::sync::Arc::clone(&activity_for_service);
                                    async move {
                                        let _busy = activity.busy();
                                        handle_elasticsearch_request_with_llm(
                                            req,
                                            connection_id,
                                            llm_clone,
                                            state_clone,
                                            status_clone,
                                            protocol_clone,
                                            server_id,
                                        )
                                        .await
                                    }
                                });

                                // Serve HTTP/1 on this connection, bounded at both ends.
                                if !spoke {
                                    Log::new(Some(&status_tx_clone)).debug(format!(
                                        "Elasticsearch peer {} sent nothing for {}s; closing before \
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
                                                    "Error serving Elasticsearch \
                                                     connection: {:?}",
                                                    err
                                                );
                                            }
                                        }
                                        _ = crate::server::accept_bounded::watch_idle(
                                            std::sync::Arc::clone(&activity),
                                            IDLE_BETWEEN_REQUESTS_TIMEOUT,
                                        ) => {
                                            Log::new(Some(&status_tx_clone)).debug(format!(
                                                "Elasticsearch connection {} idle for {}s; closing",
                                                connection_id,
                                                IDLE_BETWEEN_REQUESTS_TIMEOUT.as_secs()
                                            ));
                                        }
                                    }
                                }

                                // Mark connection as closed
                                app_state_clone
                                    .close_connection_on_server(server_id, connection_id)
                                    .await;
                                Log::new(Some(&status_tx_clone)).info(format!(
                                    "Elasticsearch connection {} closed",
                                    connection_id
                                ));
                                let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                            })
                            .await;
                    }
                    Err(e) => {
                        console_error!(
                            status_tx,
                            "Failed to accept Elasticsearch connection: {}",
                            e
                        );
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

/// Handle a single Elasticsearch request with LLM
async fn handle_elasticsearch_request_with_llm(
    req: Request<Incoming>,
    _connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<ElasticsearchProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    // Extract request details
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let path = req.uri().path().to_string();

    // Read the JSON body, bounded.
    //
    // `Incoming` has no default limit, so this used to buffer whatever an unauthenticated
    // peer chose to send — a single `POST /_bulk` was enough — and the body is then embedded
    // whole in an LLM prompt, so there is no legitimate large one either. `Limited` errors as
    // soon as the cap is passed rather than after buffering it. Elasticsearch's own answer
    // to an oversized request is 413 with a `circuit_breaking_exception`-shaped envelope.
    //
    // An unreadable body must **not** fall through as empty, which is what the old `Err` arm
    // did: the handler was then shown a request with no body and answered it as though the
    // client had sent none, so a truncated bulk index read as an empty one.
    let body_bytes = match http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(
                "Elasticsearch {} {}: refusing request body ({}); limit is {} bytes",
                method, path, e, MAX_REQUEST_BODY_BYTES
            );
            console_error!(
                status_tx,
                "Elasticsearch {} {} → 413 (request body over {} bytes)",
                method,
                path,
                MAX_REQUEST_BODY_BYTES
            );
            let reason = format!(
                "netget: request body exceeds {} bytes",
                MAX_REQUEST_BODY_BYTES
            );
            let body = serde_json::json!({
                "error": {
                    "root_cause": [{"type": "circuit_breaking_exception", "reason": reason}],
                    "type": "circuit_breaking_exception",
                    "reason": reason,
                },
                "status": 413,
            })
            .to_string();
            return Ok(build_es_response(413, body));
        }
    };

    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

    debug!(
        "Elasticsearch request: {} {} ({} bytes)",
        method,
        uri,
        body_bytes.len()
    );
    Log::new(Some(&status_tx)).debug(format!(
        "Elasticsearch {} {} ({} bytes)",
        method,
        path,
        body_bytes.len()
    ));

    // Detect operation type from path and method
    let (operation, index, doc_id) = detect_elasticsearch_operation(&method, &path);

    Log::new(Some(&status_tx)).trace(format!("Elasticsearch request body: {}", body_str));

    // Create Elasticsearch request event
    let event = crate::protocol::Event::new(
        &actions::ELASTICSEARCH_REQUEST_EVENT,
        serde_json::json!({
            "method": method,
            "path": path,
            "operation": operation,
            "index": index,
            "doc_id": doc_id,
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
            // Look for Elasticsearch-specific response actions
            for result in execution_result.protocol_results {
                match result {
                    ActionResult::Custom { name, data } => {
                        if name == "elasticsearch_response" {
                            let status =
                                data.get("status").and_then(|v| v.as_u64()).unwrap_or(200) as u16;
                            let body = data.get("body").and_then(|v| v.as_str()).unwrap_or("{}");

                            debug!("Elasticsearch response: status={}", status);
                            let log = Log::new(Some(&status_tx));
                            log.debug(format!("Elasticsearch → {} response", status));
                            log.trace(format!("Elasticsearch response body: {}", body));

                            return Ok(build_es_response(status, body.to_string()));
                        }
                    }
                    _ => {
                        // Other actions don't affect HTTP response
                    }
                }
            }

            // The handler ran but produced no Elasticsearch response — a model that refused, a
            // static handler with an empty action list, or an answer whose actions were all
            // unrecognised.
            //
            // This used to answer 200 `{"acknowledged": true}`. That is the OAuth2 shape: the
            // single most affirmative body in the Elasticsearch API, returned precisely when
            // nothing affirmed anything. A client issuing a create-index, a mapping update or a
            // delete-by-query reads `acknowledged: true` as "the cluster applied it", so a model
            // that declined the operation was reported as having performed it.
            //
            // Fail closed with the same envelope the backend-error arm below builds, so the two
            // are consistent and neither can be mistaken for a result.
            warn!(
                "Elasticsearch: no response action produced (decision=fail_closed_no_action); \
                 answering 500 rather than acknowledged:true"
            );
            console_error!(
                status_tx,
                "Elasticsearch answering 500 server_error: no response action produced \
                 (decision=fail_closed_no_action)"
            );
            let reason = crate::utils::WireFailure::Unavailable.prefixed_text();
            let error_response = serde_json::json!({
                "error": {
                    "root_cause": [{
                        "type": "server_error",
                        "reason": reason,
                    }],
                    "type": "server_error",
                    "reason": reason,
                },
                "status": 500,
            })
            .to_string();

            Ok(build_es_response(500, error_response))
        }
        Err(e) => {
            // Elasticsearch's error envelope is what every client parses, and `status` inside
            // the body must agree with the HTTP status or clients report the wrong thing.
            //
            // 503 with `type: "unavailable_shards_exception"` when the backend is merely
            // saturated: that is the type Elasticsearch itself uses for "come back later", and
            // clients retry it. 500 `server_error` otherwise. Neither can be read as a search
            // result - an empty `hits` array with a 200 would say the index contains nothing
            // matching, which is a statement about the data.
            let overloaded = crate::llm::is_overload_error(&e);
            let (status, kind) = if overloaded {
                (503u16, "unavailable_shards_exception")
            } else {
                (500u16, "server_error")
            };
            error!(
                "LLM error for Elasticsearch request (overload={}, status {}): {}",
                overloaded, status, e
            );
            console_error!(
                status_tx,
                "Elasticsearch answering {} {}: {}",
                status,
                kind,
                e
            );

            let reason = crate::utils::WireFailure::classify(&e).prefixed_text();
            let error_response = serde_json::json!({
                "error": {
                    "root_cause": [{
                        "type": kind,
                        "reason": reason
                    }],
                    "type": kind,
                    "reason": reason
                },
                "status": status
            })
            .to_string();

            Ok(build_es_response(status, error_response))
        }
    }
}

/// Build an Elasticsearch JSON response.
///
/// `status` originates in model output. `Response::builder().status()` rejects anything
/// outside 100-999 and the previous `.unwrap()` turned that into a panic, killing the
/// hyper connection task and leaving the client waiting on a socket that never answers.
/// `ElasticsearchProtocol::execute_action` already rejects out-of-range values with a
/// message the model sees; this is the belt-and-braces path.
fn build_es_response(status: u16, body: String) -> Response<Full<Bytes>> {
    let status = hyper::StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!(
            "Invalid Elasticsearch status code {}, sending 500 instead",
            status
        );
        hyper::StatusCode::INTERNAL_SERVER_ERROR
    });

    Response::builder()
        .status(status)
        .header("Content-Type", "application/json; charset=UTF-8")
        .header("X-elastic-product", "Elasticsearch")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from("{}"))))
}

/// Detect Elasticsearch operation from HTTP method and path
fn detect_elasticsearch_operation(
    method: &str,
    path: &str,
) -> (String, Option<String>, Option<String>) {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();

    match (method, parts.as_slice()) {
        // Root endpoint
        ("GET", [""]) => ("cluster_info".to_string(), None, None),

        // Search operations
        ("GET" | "POST", ["_search"]) => ("search".to_string(), None, None),
        ("GET" | "POST", [index, "_search"]) => {
            ("search".to_string(), Some(index.to_string()), None)
        }

        // Document operations
        ("POST", [index, "_doc"]) | ("PUT", [index, "_doc"]) => {
            ("index".to_string(), Some(index.to_string()), None)
        }
        ("POST" | "PUT", [index, "_doc", id]) | ("PUT", [index, "_create", id]) => (
            "index".to_string(),
            Some(index.to_string()),
            Some(id.to_string()),
        ),
        ("GET", [index, "_doc", id]) => (
            "get".to_string(),
            Some(index.to_string()),
            Some(id.to_string()),
        ),
        ("DELETE", [index, "_doc", id]) => (
            "delete".to_string(),
            Some(index.to_string()),
            Some(id.to_string()),
        ),

        // Bulk operations
        ("POST" | "PUT", ["_bulk"]) => ("bulk".to_string(), None, None),
        ("POST" | "PUT", [index, "_bulk"]) => ("bulk".to_string(), Some(index.to_string()), None),

        // Index management
        ("PUT", [index]) if !index.starts_with('_') => {
            ("create_index".to_string(), Some(index.to_string()), None)
        }
        ("DELETE", [index]) if !index.starts_with('_') => {
            ("delete_index".to_string(), Some(index.to_string()), None)
        }
        ("GET", [index]) if !index.starts_with('_') => {
            ("index_info".to_string(), Some(index.to_string()), None)
        }

        // Cluster operations
        ("GET", ["_cluster", "health"]) => ("cluster_health".to_string(), None, None),
        ("GET", ["_cluster", "stats"]) => ("cluster_stats".to_string(), None, None),
        ("GET", ["_cat", endpoint]) => (format!("cat_{}", endpoint), None, None),

        // Default
        _ => ("unknown".to_string(), None, None),
    }
}
