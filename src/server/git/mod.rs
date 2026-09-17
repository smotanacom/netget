//! Git Smart HTTP server implementation
//!
//! Serves virtual repositories over Git's Smart HTTP transport. The LLM (or a script/static
//! handler) supplies the repository *content*; this module compiles it into real Git objects
//! and frames them for the wire.
//!
//! Protocol URLs:
//! - GET  /<repo>/info/refs?service=git-upload-pack  - Reference discovery
//! - POST /<repo>/git-upload-pack                    - Object transfer
//!
//! Read-only (clone/fetch). There is no `git-receive-pack` endpoint, so pushes are refused.
//!
//! Both endpoints raise an [`Event`] through [`call_llm`], which means script and static
//! `event_handlers` work here exactly as they do for every other protocol - and a static
//! handler is the only way to guarantee the two requests of a clone see the same snapshot.

pub mod actions;
pub mod pack;
pub mod pktline;

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
use tracing::{debug, error};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::git::actions::{
    GitProtocol, DEFAULT_COMMIT_TIMESTAMP, GIT_INFO_REFS_EVENT, GIT_UPLOAD_PACK_EVENT,
};
use crate::server::git::pack::{build_repo, hex_id, write_pack, BuiltRepo, CommitMeta, RepoFile};
use crate::state::app_state::AppState;
use crate::utils::WireFailure;

/// Capabilities advertised on the first ref line.
///
/// Deliberately minimal. `side-band-64k` is *not* offered: the multiplexed form is only
/// correct if the server can read which capabilities the client selected, and git compresses
/// the `git-upload-pack` request body, which this server cannot always decompress. Refusing
/// the capability keeps every response in the one framing that is always right, at the cost of
/// no progress or error side-channel during transfer.
const BASE_CAPABILITIES: &str = "no-progress agent=netget";

/// Largest `POST /git-upload-pack` body this server will buffer, in bytes.
///
/// The body is a list of `want`/`have` lines and is tiny in practice - a clone of a
/// single-commit repository sends a few hundred bytes. An unbounded `collect()` lets one
/// unauthenticated client grow the process by whatever it cares to send, so the read is
/// capped and an oversized body is refused with 413.
pub const MAX_UPLOAD_PACK_BYTES: usize = 1024 * 1024;

/// How long to wait for a peer's first byte after it connects.
///
/// Smart HTTP is client-speaks-first — the server says nothing until it has a request line —
/// so a peer that has connected and sent nothing has begun no request at all, which is the
/// state an unauthenticated flood lives in. Enforced with `TcpStream::peek` *before* the socket
/// reaches hyper, so the request line is still there for hyper to parse afterwards.
const FIRST_BYTE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a connection may do nothing at all between requests.
///
/// **This bounds the silence, not the transfer.** A clone is two requests on one keep-alive
/// connection and the second one carries the whole pack; that pack is built into a
/// `Full<Bytes>` before the response is returned, so hyper writes an already-complete body and
/// a slow reader is draining bytes rather than idling. The watchdog reads `ConnectionActivity`,
/// which reports a connection with a request in flight as **not idle at all**, so building a
/// repository — including an LLM round-trip, or a `manual` rule parked for a human — can never
/// close the connection it is an answer for.
///
/// Fifteen minutes is the wait between one request finishing and the next one starting. git
/// itself sets no keep-alive interval and curl's default idle reuse is far shorter; a git
/// client that has taken fifteen minutes to decide on its next request has gone away.
const IDLE_BETWEEN_REQUESTS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Concurrent connections this server admits.
///
/// Each admitted connection may buffer a request body of up to [`MAX_UPLOAD_PACK_BYTES`] and
/// build a repository of whatever size the handler describes, so the cap is what turns that
/// per-connection bound into a total one.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] is told before the socket closes.
///
/// Smart HTTP *is* HTTP, so the protocol's own vocabulary for "come back later" is a `503` with
/// `Retry-After` — which `git` surfaces as `unable to access … The requested URL returned
/// error: 503` rather than as an unexplained connection reset.
const CONNECTION_CAP_REFUSAL: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    Content-Length: 0\r\nRetry-After: 5\r\nConnection: close\r\n\r\n";

/// Shared per-request context, so each handler does not take nine positional arguments.
struct RequestContext {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<GitProtocol>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    remote_addr: SocketAddr,
    default_branch: String,
}

/// Git Smart HTTP server
pub struct GitServer;

impl GitServer {
    /// Spawn the Git server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        default_branch: String,
        server_id: crate::state::ServerId,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("Git server listening on {}", local_addr));

        let protocol = Arc::new(GitProtocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "Git",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((stream, remote_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        Log::new(Some(&status_tx))
                            .info(format!("Git connection from {}", remote_addr));

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
                        let default_branch_clone = default_branch.clone();

                        // Spawn a task to handle this connection
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Held for the life of the connection: dropping it early
                                // releases the slot while the peer is still here, which
                                // silently un-caps the server.
                                let _permit = permit;

                                // First-byte bound, before hyper sees the socket. hyper owns
                                // every read once `serve_connection` starts and keeps polling
                                // for frames *while a request is being answered*, so a deadline
                                // on reads would fire in the middle of an LLM round-trip.
                                // `peek` waits for data without consuming it, so the request
                                // line is still there for hyper afterwards.
                                match tokio::time::timeout(
                                    FIRST_BYTE_READ_TIMEOUT,
                                    stream.peek(&mut [0u8; 1]),
                                )
                                .await
                                {
                                    Ok(Ok(0)) | Ok(Err(_)) => {
                                        app_state_clone
                                            .close_connection_on_server(server_id, connection_id)
                                            .await;
                                        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                                        return;
                                    }
                                    Ok(Ok(_)) => {}
                                    Err(_) => {
                                        Log::new(Some(&status_tx_clone)).debug(format!(
                                            "Git peer {} sent nothing for {}s; closing before \
                                             the request line",
                                            remote_addr,
                                            FIRST_BYTE_READ_TIMEOUT.as_secs()
                                        ));
                                        app_state_clone
                                            .close_connection_on_server(server_id, connection_id)
                                            .await;
                                        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                                        return;
                                    }
                                }

                                let io = TokioIo::new(stream);

                                // Clone for service closure
                                let status_for_service = status_tx_clone.clone();
                                let app_state_for_service = app_state_clone.clone();

                                // Tracks whether this connection is answering anything. A
                                // request waiting on the model, or parked for a human at the
                                // dashboard, holds the count above zero, so the idle watchdog
                                // below cannot close the connection the answer belongs to
                                // however long it takes — only genuine silence counts.
                                let activity = Arc::new(
                                    crate::server::accept_bounded::ConnectionActivity::new(),
                                );
                                let activity_for_service = Arc::clone(&activity);

                                // Create a service that handles Git Smart HTTP requests with LLM
                                let service = service_fn(move |req: Request<Incoming>| {
                                    let ctx = RequestContext {
                                        llm_client: llm_client_clone.clone(),
                                        app_state: app_state_for_service.clone(),
                                        status_tx: status_for_service.clone(),
                                        protocol: protocol_clone.clone(),
                                        connection_id,
                                        server_id,
                                        remote_addr,
                                        default_branch: default_branch_clone.clone(),
                                    };
                                    let activity = Arc::clone(&activity_for_service);
                                    async move {
                                        let _busy = activity.busy();
                                        handle_git_request(req, ctx).await
                                    }
                                });

                                // Serve HTTP/1 on this connection
                                let conn = http1::Builder::new().serve_connection(io, service);
                                tokio::pin!(conn);
                                tokio::select! {
                                    result = &mut conn => {
                                        if let Err(err) = result {
                                            error!("Error serving Git connection: {:?}", err);
                                        }
                                    }
                                    _ = crate::server::accept_bounded::watch_idle(
                                        Arc::clone(&activity),
                                        IDLE_BETWEEN_REQUESTS_TIMEOUT,
                                    ) => {
                                        debug!(
                                            "Git connection {} idle for {}s; closing",
                                            connection_id,
                                            IDLE_BETWEEN_REQUESTS_TIMEOUT.as_secs()
                                        );
                                    }
                                }

                                // Mark connection as closed
                                app_state_clone
                                    .close_connection_on_server(server_id, connection_id)
                                    .await;
                                Log::new(Some(&status_tx_clone))
                                    .info(format!("Git connection {} closed", connection_id));
                                let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                            })
                            .await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept Git connection: {}", e));
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

/// Handle a Git Smart HTTP request
async fn handle_git_request(
    req: Request<Incoming>,
    ctx: RequestContext,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let user_agent = req
        .headers()
        .get(hyper::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    Log::new(Some(&ctx.status_tx)).debug(format!("Git {} {}", method, path));

    // Parse repository name from path
    // Format: /<repo>/info/refs or /<repo>/git-upload-pack
    let repo = parse_repo_name(&path);

    // Track repository access
    track_repo_access(&ctx.app_state, ctx.server_id, ctx.connection_id, &repo).await;

    // Route based on path
    match (&method, path.as_str()) {
        // Reference discovery: GET /info/refs?service=git-upload-pack
        (&Method::GET, p) if p.ends_with("/info/refs") => {
            let query = uri.query().unwrap_or("");
            if query.contains("service=git-upload-pack") {
                Ok(handle_info_refs(&ctx, &repo, &user_agent).await)
            } else if query.contains("service=git-receive-pack") {
                // Be explicit: this is a read-only server, not a broken one.
                Ok(build_error_response(
                    StatusCode::FORBIDDEN,
                    "Push is not supported: this server implements git-upload-pack only",
                ))
            } else {
                // Dumb HTTP protocol not supported
                Ok(build_error_response(
                    StatusCode::FORBIDDEN,
                    "Dumb HTTP protocol not supported, use Smart HTTP (git-upload-pack service)",
                ))
            }
        }

        // Object transfer: POST /git-upload-pack
        (&Method::POST, p) if p.ends_with("/git-upload-pack") => {
            let content_encoding = req
                .headers()
                .get(hyper::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            // Bounded read: `Limited` errors as soon as the cap is passed rather than after
            // buffering the whole thing, so an oversized body costs at most the cap.
            let limited = http_body_util::Limited::new(req.into_body(), MAX_UPLOAD_PACK_BYTES);
            let body_bytes = match limited.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(e) => {
                    Log::new(Some(&ctx.status_tx)).error(format!(
                        "Git refusing upload-pack body ({}); limit is {} bytes",
                        e, MAX_UPLOAD_PACK_BYTES
                    ));
                    return Ok(build_error_response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "git-upload-pack request body is too large",
                    ));
                }
            };

            Log::new(Some(&ctx.status_tx)).trace(format!(
                "Git upload-pack request body ({} bytes, encoding {:?})",
                body_bytes.len(),
                content_encoding
            ));

            Ok(handle_upload_pack(&ctx, &repo, &body_bytes, &content_encoding).await)
        }

        // Push endpoint: answer honestly rather than 404.
        (_, p) if p.ends_with("/git-receive-pack") => Ok(build_error_response(
            StatusCode::FORBIDDEN,
            "Push is not supported: this server implements git-upload-pack only",
        )),

        // Unsupported endpoint
        _ => Ok(build_error_response(
            StatusCode::NOT_FOUND,
            &format!("Endpoint not found: {} {}", method, path),
        )),
    }
}

/// Parse repository name from URL path
fn parse_repo_name(path: &str) -> String {
    // Path formats:
    // /repo-name/info/refs
    // /repo-name/git-upload-pack
    // /info/refs (root repository)
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();

    match parts.first() {
        None | Some(&"") | Some(&"info") | Some(&"git-upload-pack") | Some(&"git-receive-pack") => {
            "default".to_string()
        }
        Some(name) => (*name).to_string(),
    }
}

/// Handle GET /info/refs?service=git-upload-pack
async fn handle_info_refs(
    ctx: &RequestContext,
    repo: &str,
    user_agent: &str,
) -> Response<Full<Bytes>> {
    let event = Event::new(
        &GIT_INFO_REFS_EVENT,
        serde_json::json!({
            "repository": repo,
            "user_agent": user_agent,
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let outcome = match resolve_repository(ctx, &event).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    let repo_build = match outcome {
        Outcome::Repository(build) => build,
        Outcome::Error(response) => return response,
    };

    let body = build_refs_advertisement(&repo_build);
    Log::new(Some(&ctx.status_tx)).info(format!(
        "Git advertised {} at {} for '{}'",
        repo_build.branch,
        &repo_build.commit_hex()[..8],
        repo
    ));
    record_bytes_sent(ctx, body.len()).await;

    Response::builder()
        .status(StatusCode::OK)
        .header(
            "Content-Type",
            "application/x-git-upload-pack-advertisement",
        )
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(body)))
        .expect("static header values are valid")
}

/// Handle POST /git-upload-pack
async fn handle_upload_pack(
    ctx: &RequestContext,
    repo: &str,
    body: &[u8],
    content_encoding: &str,
) -> Response<Full<Bytes>> {
    record_bytes_received(ctx, body.len()).await;

    // git compresses small RPC bodies. Without a decompressor the wants cannot be read; the
    // request is still answered, because for a single-commit repository the answer does not
    // depend on them - but say so rather than silently reporting "no wants" to the model.
    let compressed = !content_encoding.is_empty() && content_encoding != "identity";
    if compressed {
        debug!(
            "Git upload-pack body uses Content-Encoding: {} - negotiation details unavailable",
            content_encoding
        );
    }

    let request = if compressed {
        pktline::UploadPackRequest::default()
    } else {
        pktline::parse_upload_pack_request(body)
    };

    // A negotiation round that sends `have` lines without `done` expects acknowledgements
    // only. Answering it with a pack would desynchronise the client.
    if !request.done && !request.haves.is_empty() {
        debug!(
            "Git upload-pack negotiation round ({} haves, no done) - replying NAK",
            request.haves.len()
        );
        let mut body = Vec::new();
        body.extend_from_slice(&pktline::encode(b"NAK\n"));
        return upload_pack_response(ctx, body).await;
    }

    let event = Event::new(
        &GIT_UPLOAD_PACK_EVENT,
        serde_json::json!({
            "repository": repo,
            "wants": request.wants,
            "haves": request.haves,
            "capabilities": request.capabilities,
            "client_ip": ctx.remote_addr.ip().to_string(),
        }),
    );

    let outcome = match resolve_repository(ctx, &event).await {
        Ok(outcome) => outcome,
        Err(response) => return response,
    };

    let repo_build = match outcome {
        Outcome::Repository(build) => build,
        Outcome::Error(response) => return response,
    };

    let commit_hex = repo_build.commit_hex();
    if !request.wants.is_empty() && !request.wants.contains(&commit_hex) {
        // The two halves of the clone disagreed. Say exactly why, because the client-side
        // error ("did not send all necessary objects") gives no hint.
        Log::new(Some(&ctx.status_tx)).error(format!(
            "Git '{}': advertised commit and packed commit differ ({} vs {}); clone will \
             fail. The git_info_refs and git_upload_pack events returned different repository \
             content. Pin the repository content with a static event handler.",
            repo,
            request.wants.first().map(|w| &w[..8]).unwrap_or("?"),
            &commit_hex[..8]
        ));
    }

    let pack = write_pack(&repo_build.objects);
    debug!(
        "Git pack for '{}': {} objects, {} bytes, commit {}",
        repo,
        repo_build.objects.len(),
        pack.len(),
        commit_hex
    );

    let mut body = Vec::with_capacity(pack.len() + 16);
    body.extend_from_slice(&pktline::encode(b"NAK\n"));
    body.extend_from_slice(&pack);

    Log::new(Some(&ctx.status_tx)).info(format!(
        "Git sent pack for '{}' ({} objects, {} bytes)",
        repo,
        repo_build.objects.len(),
        pack.len()
    ));

    upload_pack_response(ctx, body).await
}

async fn upload_pack_response(ctx: &RequestContext, body: Vec<u8>) -> Response<Full<Bytes>> {
    record_bytes_sent(ctx, body.len()).await;
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-git-upload-pack-result")
        .header("Cache-Control", "no-cache")
        .body(Full::new(Bytes::from(body)))
        .expect("static header values are valid")
}

/// What the model decided to do with a request.
enum Outcome {
    Repository(BuiltRepo),
    Error(Response<Full<Bytes>>),
}

/// Raise the event, then turn whatever the handler or model produced into a repository.
///
/// Three failure shapes are kept apart by a `decision=` tag, as in `src/server/radius/`:
/// the call itself failed (`fail_closed_overloaded` / `fail_closed_llm_error`), the model
/// answered with a refusal (`model_reject`), or the model answered nothing this endpoint
/// can use (`fail_closed_no_action`). The peer is told only a
/// [`WireFailure`] category — 503 + `Retry-After` when the backend is saturated so git
/// backs off, 500 otherwise — never the error text, which names the backend, the model and
/// our own retry machinery.
async fn resolve_repository(
    ctx: &RequestContext,
    event: &Event,
) -> Result<Outcome, Response<Full<Bytes>>> {
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
            let failure = WireFailure::classify(&e);
            let decision = if failure.is_overloaded() {
                "fail_closed_overloaded"
            } else {
                "fail_closed_llm_error"
            };
            Log::new(Some(&ctx.status_tx)).error(format!(
                "Git {} decision={} error: {}",
                event.id(),
                decision,
                e
            ));
            return Err(if failure.is_overloaded() {
                build_retryable_error_response(failure.text())
            } else {
                build_error_response(StatusCode::INTERNAL_SERVER_ERROR, failure.text())
            });
        }
    };

    for message in &execution_result.messages {
        Log::new(Some(&ctx.status_tx)).info(format!("{}", message));
    }

    for result in execution_result.protocol_results {
        match result {
            ActionResult::Custom { name, data } if name == "git_repository_response" => {
                match snapshot_to_repo(&data, &ctx.default_branch) {
                    Ok(build) => return Ok(Outcome::Repository(build)),
                    Err(e) => {
                        Log::new(Some(&ctx.status_tx))
                            .error(format!("Git: invalid repository description: {}", e));
                        return Err(build_error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("Invalid repository description: {}", e),
                        ));
                    }
                }
            }
            ActionResult::Custom { name, data } if name == "git_error_response" => {
                let message = data
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Error");
                let code = data.get("code").and_then(|v| v.as_u64()).unwrap_or(500) as u16;
                // The model answered, and its answer was a refusal — distinct in the log
                // from both "answered nothing" and "the call errored".
                Log::new(Some(&ctx.status_tx)).info(format!(
                    "Git {} decision=model_reject code={}",
                    event.id(),
                    code
                ));
                return Ok(Outcome::Error(build_error_response(
                    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    message,
                )));
            }
            _ => {}
        }
    }

    // The call succeeded and the answer contained neither a repository nor a refusal.
    Log::new(Some(&ctx.status_tx)).warn(format!(
        "Git {} decision=fail_closed_no_action (expected git_repository or git_error)",
        event.id()
    ));
    Err(build_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        WireFailure::Unavailable.text(),
    ))
}

/// Turn the `git_repository` action payload into Git objects.
fn snapshot_to_repo(data: &Value, default_branch: &str) -> anyhow::Result<BuiltRepo> {
    let branch = data
        .get("branch")
        .and_then(|v| v.as_str())
        .filter(|b| !b.is_empty())
        .unwrap_or(default_branch);

    let mut files = Vec::new();
    if let Some(entries) = data.get("files").and_then(|v| v.as_array()) {
        for entry in entries {
            let path = entry
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("A file entry has no 'path'"))?;
            let content = entry
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let executable = entry
                .get("executable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            files.push(RepoFile {
                path: path.to_string(),
                content: content.into_bytes(),
                executable,
            });
        }
    }

    let meta = CommitMeta {
        message: data
            .get("commit_message")
            .and_then(|v| v.as_str())
            .unwrap_or("Initial commit")
            .to_string(),
        author_name: data
            .get("author_name")
            .and_then(|v| v.as_str())
            .unwrap_or("NetGet")
            .to_string(),
        author_email: data
            .get("author_email")
            .and_then(|v| v.as_str())
            .unwrap_or("netget@localhost")
            .to_string(),
        timestamp: data
            .get("timestamp")
            .and_then(|v| v.as_i64())
            .unwrap_or(DEFAULT_COMMIT_TIMESTAMP),
    };

    build_repo(branch, &files, &meta)
}

/// Build the pkt-line reference advertisement for `GET /info/refs`.
fn build_refs_advertisement(repo: &BuiltRepo) -> Vec<u8> {
    let sha = hex_id(&repo.commit_id);
    let branch_ref = format!("refs/heads/{}", repo.branch);

    let mut out = Vec::new();
    out.extend_from_slice(&pktline::encode(b"# service=git-upload-pack\n"));
    out.extend_from_slice(pktline::FLUSH);

    // The first ref line carries the capability list after a NUL. symref tells the client
    // which branch HEAD points at, without which it warns and checks out nothing.
    let capabilities = format!("{BASE_CAPABILITIES} symref=HEAD:{branch_ref}");
    out.extend_from_slice(&pktline::encode(
        format!("{sha} HEAD\0{capabilities}\n").as_bytes(),
    ));
    out.extend_from_slice(&pktline::encode(format!("{sha} {branch_ref}\n").as_bytes()));
    out.extend_from_slice(pktline::FLUSH);
    out
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

/// A retryable failure: 503 with `Retry-After`, so git reports "server unavailable, retry"
/// rather than recording a permanent fault as a bare 500 would. Same shape as
/// `src/server/http_common/handler.rs` and `src/server/mercurial/mod.rs`.
fn build_retryable_error_response(message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("Content-Type", "text/plain")
        .header(hyper::header::RETRY_AFTER, "1")
        .body(Full::new(Bytes::from(format!("Error: {}\n", message))))
        .expect("static header values are valid")
}
