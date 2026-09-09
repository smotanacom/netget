//! Mercurial HTTP server implementation
//!
//! Implements a subset of Mercurial's HTTP wire protocol (version 1) for serving virtual
//! repositories. The LLM controls capabilities, heads, branches and bookmarks.
//!
//! Protocol URLs:
//! - GET  /?cmd=capabilities       - Server capabilities
//! - GET  /?cmd=heads              - Repository heads
//! - GET  /?cmd=branchmap          - Branch mappings
//! - GET  /?cmd=listkeys           - List keys (bookmarks, tags, etc.)
//! - POST /?cmd=getbundle          - Changegroup retrieval (clone/pull)
//!
//! Read-only: there is no `unbundle` endpoint, so pushes are refused.
//!
//! Every command raises an [`Event`] through [`call_llm`], so script and static
//! `event_handlers` work here as they do for every other protocol.

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
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::mercurial::actions::{
    empty_bundle, MercurialProtocol, HG_BRANCHMAP_EVENT, HG_CAPABILITIES_EVENT, HG_GETBUNDLE_EVENT,
    HG_HEADS_EVENT, HG_LISTKEYS_EVENT,
};
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use crate::{console_error, console_info};

/// Null node ID: Mercurial's "no changeset" sentinel, and the correct `heads` answer for an
/// empty repository.
const NULL_NODE: &str = "0000000000000000000000000000000000000000";

/// Largest request body this server will buffer, in bytes.
///
/// Only `getbundle` has a body at all, and hg puts its arguments there only when they are too
/// long for a URL — kilobytes at most. An unbounded `collect()` lets one unauthenticated
/// client grow the process by whatever it cares to send, so the read is capped and an
/// oversized body is refused with 413.
const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// Shared per-request context.
struct RequestContext {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<MercurialProtocol>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    remote_addr: SocketAddr,
}

/// Mercurial HTTP server
pub struct MercurialServer;

impl MercurialServer {
    /// Spawn the Mercurial server with integrated LLM actions
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
        console_info!(status_tx, "Mercurial server listening on {}", local_addr);

        let protocol = Arc::new(MercurialProtocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        info!(
                            "Mercurial connection {} from {}",
                            connection_id, remote_addr
                        );
                        let _ = status_tx
                            .send(format!("[INFO] Mercurial connection from {}", remote_addr));

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

                            // Create a service that handles Mercurial HTTP requests with LLM
                            let service = service_fn(move |req: Request<Incoming>| {
                                let ctx = RequestContext {
                                    llm_client: llm_client_clone.clone(),
                                    app_state: app_state_for_service.clone(),
                                    status_tx: status_for_service.clone(),
                                    protocol: protocol_clone.clone(),
                                    connection_id,
                                    server_id,
                                    remote_addr,
                                };
                                handle_mercurial_request(req, ctx)
                            });

                            // Serve HTTP/1 on this connection
                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving Mercurial connection: {:?}", err);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_tx_clone.send(format!(
                                "[INFO] Mercurial connection {} closed",
                                connection_id
                            ));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept Mercurial connection: {}", e);
                        let _ = status_tx.send(format!(
                            "[ERROR] Failed to accept Mercurial connection: {}",
                            e
                        ));
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

/// Handle a Mercurial HTTP request
async fn handle_mercurial_request(
    req: Request<Incoming>,
    ctx: RequestContext,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    debug!("Mercurial request: {} {}?{}", method, path, query);
    let _ = ctx
        .status_tx
        .send(format!("[DEBUG] Mercurial {} {}?{}", method, path, query));

    // Parse query parameters
    let params: HashMap<String, String> = parse_query_params(&query);
    let cmd = params.get("cmd").map(|s| s.as_str()).unwrap_or("");

    // Parse repository name from path (e.g., /repo-name or /)
    let repo = parse_repo_name(&path);

    // Track repository access
    track_repo_access(&ctx.app_state, ctx.server_id, ctx.connection_id, &repo).await;

    match cmd {
        "capabilities" => Ok(handle_capabilities(&ctx, &repo).await),
        "heads" => Ok(handle_heads(&ctx, &repo).await),
        "branchmap" => Ok(handle_branchmap(&ctx, &repo).await),
        "listkeys" => {
            let namespace = params
                .get("namespace")
                .map(|s| s.as_str())
                .unwrap_or("bookmarks");
            Ok(handle_listkeys(&ctx, &repo, namespace).await)
        }
        "getbundle" => {
            // hg sends getbundle as a GET with the arguments in the query string, or as a
            // POST when they are too long for a URL. Accept both.
            // Bounded read: `Limited` errors as soon as the cap is passed rather than after
            // buffering the whole thing, so an oversized body costs at most the cap.
            let limited = http_body_util::Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES);
            let body_len = match limited.collect().await {
                Ok(collected) => collected.to_bytes().len(),
                Err(e) => {
                    console_error!(
                        ctx.status_tx,
                        "Mercurial refusing request body ({}); limit is {} bytes",
                        e,
                        MAX_REQUEST_BODY_BYTES
                    );
                    return Ok(build_error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "request body is too large",
                    ));
                }
            };
            if body_len > 0 {
                record_bytes_received(&ctx, body_len).await;
                trace!("Mercurial getbundle request body ({} bytes)", body_len);
            }
            Ok(handle_getbundle(&ctx, &repo, &params).await)
        }
        "unbundle" | "pushkey" => Ok(build_error_response(
            StatusCode::FORBIDDEN,
            "Push is not supported: this server serves clone/pull only",
        )),
        "" if method == Method::GET => Ok(build_text_response(
            StatusCode::OK,
            "Mercurial HTTP Server - NetGet\nSpecify ?cmd=capabilities to see server capabilities",
        )),
        other => {
            debug!("Mercurial: unimplemented command {:?}", other);
            Ok(build_error_response(
                StatusCode::NOT_FOUND,
                &format!("Unknown or unimplemented command: {}", other),
            ))
        }
    }
}

/// Parse repository name from URL path
fn parse_repo_name(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse query parameters from query string
fn parse_query_params(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            match (parts.next(), parts.next()) {
                (Some(key), Some(value)) => Some((
                    key.to_string(),
                    urlencoding::decode(value).ok()?.to_string(),
                )),
                _ => None,
            }
        })
        .collect()
}

/// Handle ?cmd=capabilities
async fn handle_capabilities(ctx: &RequestContext, repo: &str) -> Response<Full<Bytes>> {
    let event = Event::new(
        &HG_CAPABILITIES_EVENT,
        serde_json::json!({
            "repository": repo,
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let data = match resolve(ctx, &event, "hg_capabilities_response").await {
        Ok(data) => data,
        Err(response) => return response,
    };

    let requested: Vec<String> = data
        .get("capabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let capabilities = actions::sanitize_capabilities(&requested);
    let body = capabilities.join("\n");
    let _ = ctx.status_tx.send(format!(
        "→ hg capabilities for '{}': {}",
        repo,
        capabilities.join(" ")
    ));
    text_response(ctx, &body).await
}

/// Handle ?cmd=heads
async fn handle_heads(ctx: &RequestContext, repo: &str) -> Response<Full<Bytes>> {
    let event = Event::new(
        &HG_HEADS_EVENT,
        serde_json::json!({
            "repository": repo,
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let data = match resolve(ctx, &event, "hg_heads_response").await {
        Ok(data) => data,
        Err(response) => return response,
    };

    let heads = node_list(data.get("heads"));
    let body = if heads.is_empty() {
        NULL_NODE.to_string()
    } else {
        heads.join(" ")
    };

    let _ = ctx
        .status_tx
        .send(format!("→ hg heads for '{}': {}", repo, body));
    text_response(ctx, &body).await
}

/// Handle ?cmd=branchmap
async fn handle_branchmap(ctx: &RequestContext, repo: &str) -> Response<Full<Bytes>> {
    let event = Event::new(
        &HG_BRANCHMAP_EVENT,
        serde_json::json!({
            "repository": repo,
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let data = match resolve(ctx, &event, "hg_branchmap_response").await {
        Ok(data) => data,
        Err(response) => return response,
    };

    // Format: one line per branch, "<branch> <node> <node> ...".
    let mut body = String::new();
    if let Some(branches) = data.get("branches").and_then(|v| v.as_object()) {
        for (branch, nodes) in branches {
            let nodes = node_list(Some(nodes));
            if nodes.is_empty() {
                continue;
            }
            body.push_str(&format!("{} {}\n", branch, nodes.join(" ")));
        }
    }

    text_response(ctx, &body).await
}

/// Handle ?cmd=listkeys&namespace=...
async fn handle_listkeys(
    ctx: &RequestContext,
    repo: &str,
    namespace: &str,
) -> Response<Full<Bytes>> {
    let event = Event::new(
        &HG_LISTKEYS_EVENT,
        serde_json::json!({
            "repository": repo,
            "namespace": namespace,
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let data = match resolve(ctx, &event, "hg_listkeys_response").await {
        Ok(data) => data,
        Err(response) => return response,
    };

    // Format: "<key>\t<value>\n" per entry.
    let mut body = String::new();
    if let Some(keys) = data.get("keys").and_then(|v| v.as_object()) {
        for (key, value) in keys {
            let value = match value.as_str() {
                Some(v) => v.to_string(),
                None => value.to_string(),
            };
            body.push_str(&format!("{}\t{}\n", key, value));
        }
    }

    text_response(ctx, &body).await
}

/// Handle ?cmd=getbundle
async fn handle_getbundle(
    ctx: &RequestContext,
    repo: &str,
    params: &HashMap<String, String>,
) -> Response<Full<Bytes>> {
    let event = Event::new(
        &HG_GETBUNDLE_EVENT,
        serde_json::json!({
            "repository": repo,
            "heads": params.get("heads").cloned().unwrap_or_default(),
            "common": params.get("common").cloned().unwrap_or_default(),
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let data = match resolve(ctx, &event, "hg_bundle_response").await {
        Ok(data) => data,
        Err(response) => return response,
    };

    let bundle_type = data
        .get("bundle_type")
        .and_then(|v| v.as_str())
        .unwrap_or("HG10UN");
    let bundle = empty_bundle(bundle_type);

    debug!(
        "Mercurial getbundle for '{}': sending empty {} changegroup ({} bytes)",
        repo,
        bundle_type,
        bundle.len()
    );
    let _ = ctx.status_tx.send(format!(
        "→ hg bundle for '{}': empty changegroup ({} bytes)",
        repo,
        bundle.len()
    ));

    record_bytes_sent(ctx, bundle.len()).await;
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/mercurial-0.1")
        .body(Full::new(Bytes::from(bundle)))
        .expect("static header values are valid")
}

/// Raise the event and pick out the one protocol result this command needs.
///
/// `hg_error` is honoured for every command; anything else (no action, or an action for a
/// different command) becomes a 500 with a log line naming the event.
///
/// Three outcomes are kept distinguishable in the log by a `decision=` tag: the model
/// refused (`model_reject`), the model answered nothing usable (`fail_closed_no_action`),
/// or the call itself failed (`fail_closed_llm_error` / `fail_closed_overloaded`). The peer
/// is told only a category — 503 + `Retry-After` when the backend is saturated, 500
/// otherwise — never the error text.
async fn resolve(
    ctx: &RequestContext,
    event: &Event,
    expected: &str,
) -> Result<Value, Response<Full<Bytes>>> {
    let execution_result = match call_llm(
        &ctx.llm_client,
        &ctx.app_state,
        ctx.server_id,
        Some(ctx.connection_id),
        event,
        ctx.protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            // The peer gets a category, the log gets the error. Nothing derived from `e`
            // reaches the socket: `WireFailure::text` is a `&'static str` by design.
            let failure = WireFailure::classify(&e);
            let decision = if failure.is_overloaded() {
                "fail_closed_overloaded"
            } else {
                "fail_closed_llm_error"
            };
            error!(
                "Mercurial {} decision={} error={:#}",
                event.id(),
                decision,
                e
            );
            let _ = ctx.status_tx.send(format!(
                "✗ Mercurial {} decision={}: {}",
                event.id(),
                decision,
                e
            ));
            // Overload is transient; 503 + Retry-After tells hg to back off rather than
            // record a permanent fault, which a bare 500 would.
            return Err(if failure.is_overloaded() {
                build_retryable_error_response(failure.text())
            } else {
                build_error_response(StatusCode::INTERNAL_SERVER_ERROR, failure.text())
            });
        }
    };

    for message in &execution_result.messages {
        console_info!(ctx.status_tx, "{}", message);
    }

    for result in execution_result.protocol_results {
        match result {
            ActionResult::Custom { name, data } if name == expected => return Ok(data),
            ActionResult::Custom { name, data } if name == "hg_error_response" => {
                let message = data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Error");
                let code = data.get("code").and_then(|v| v.as_u64()).unwrap_or(500) as u16;
                // The model answered, and its answer was a refusal. Distinct in the log from
                // both "answered nothing" and "the call errored".
                warn!(
                    "Mercurial {} decision=model_reject code={}",
                    event.id(),
                    code
                );
                return Err(build_error_response(
                    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    message,
                ));
            }
            _ => {}
        }
    }

    // The call succeeded and the model said nothing usable — a third case, kept separate
    // from `decision=model_reject` and `decision=fail_closed_llm_error`.
    warn!(
        "Mercurial {} decision=fail_closed_no_action expected={}",
        event.id(),
        expected
    );
    let _ = ctx.status_tx.send(format!(
        "✗ Mercurial {} decision=fail_closed_no_action (expected {})",
        event.id(),
        expected
    ));
    Err(build_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        WireFailure::Unavailable.text(),
    ))
}

/// Extract a list of node IDs from an action field, accepting an array or a
/// whitespace-separated string, and dropping anything that is not a 40-character hex node.
fn node_list(value: Option<&Value>) -> Vec<String> {
    let mut nodes = Vec::new();
    match value {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(node) = item.as_str() {
                    push_node(&mut nodes, node);
                }
            }
        }
        Some(Value::String(text)) => {
            for node in text.split_whitespace() {
                push_node(&mut nodes, node);
            }
        }
        _ => {}
    }
    nodes
}

fn push_node(nodes: &mut Vec<String>, candidate: &str) {
    let candidate = candidate.trim();
    if candidate.len() == 40 && candidate.bytes().all(|b| b.is_ascii_hexdigit()) {
        nodes.push(candidate.to_ascii_lowercase());
    } else {
        // A model that answers "abc123..." would otherwise put an unparseable node on the
        // wire and the client would fail with an opaque error.
        warn!(
            "Mercurial: dropping {:?}, which is not a 40-character hex node ID",
            candidate
        );
    }
}

async fn text_response(ctx: &RequestContext, body: &str) -> Response<Full<Bytes>> {
    record_bytes_sent(ctx, body.len()).await;
    build_text_response(StatusCode::OK, body)
}

/// Track repository access in connection state
async fn track_repo_access(
    app_state: &Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    repo_name: &str,
) {
    app_state
        .with_server_mut(server_id, |server| {
            if let Some(conn) = server.connections.get_mut(&connection_id) {
                if let Some(obj) = conn.protocol_info.data.as_object_mut() {
                    let mut recent_repos: Vec<String> = obj
                        .get("recent_repos")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    if !recent_repos.contains(&repo_name.to_string()) {
                        recent_repos.push(repo_name.to_string());
                        // Keep only last 10 repos
                        if recent_repos.len() > 10 {
                            recent_repos.remove(0);
                        }
                    }
                    obj.insert(
                        "recent_repos".to_string(),
                        serde_json::to_value(&recent_repos).unwrap_or(serde_json::json!([])),
                    );
                }
            }
        })
        .await;
}

async fn record_bytes_sent(ctx: &RequestContext, len: usize) {
    ctx.app_state
        .update_connection_stats(
            ctx.server_id,
            ctx.connection_id,
            None,
            Some(len as u64),
            None,
            Some(1),
        )
        .await;
}

async fn record_bytes_received(ctx: &RequestContext, len: usize) {
    ctx.app_state
        .update_connection_stats(
            ctx.server_id,
            ctx.connection_id,
            Some(len as u64),
            None,
            Some(1),
            None,
        )
        .await;
}

/// Build an HTTP error response
fn build_error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(format!("Error: {}\n", message))))
        .expect("static header values are valid")
}

/// A retryable failure: 503 with `Retry-After`, so hg backs off instead of treating the
/// server as permanently broken. Same shape as `src/server/http_common/handler.rs`.
fn build_retryable_error_response(message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("Content-Type", "text/plain")
        .header(hyper::header::RETRY_AFTER, "1")
        .body(Full::new(Bytes::from(format!("Error: {}\n", message))))
        .expect("static header values are valid")
}

/// Build a text response
fn build_text_response(status: StatusCode, text: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/mercurial-0.1")
        .body(Full::new(Bytes::from(text.to_string())))
        .expect("static header values are valid")
}
