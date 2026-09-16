//! IPP (Internet Printing Protocol) server implementation.
//!
//! IPP is HTTP POST with a binary body (RFC 8010/8011); hyper carries the HTTP and this module
//! parses just enough of the body to name the operation. The LLM supplies the answer as
//! structured attributes and `actions.rs` encodes it — no action carries bytes.
//!
//! There is no job queue and no printer state here: a `Print-Job` is not recorded anywhere, so
//! a following `Get-Job-Attributes` is answered by the model from its own memory, not from a
//! store this protocol keeps. That is deliberate.
//!
//! **Request parsing is shallow.** Only the 8-byte header is decoded (version, operation-id,
//! request-id). Attribute groups in the *request* are not parsed, so the model is told which
//! operation was asked for but not, for instance, which `printer-uri` or `document-format` the
//! client asked about, nor the document data of a Print-Job. Add attribute-group decoding here
//! if the model needs to see it.
//!
//! That shallowness is also why there is no recursive walk to bound here — the class of defect
//! that kills the whole process with a `SIGSEGV` no `tokio::spawn` can contain. If request
//! attribute decoding is ever added, it must stay iterative and must bound each value against
//! the bytes actually present rather than against the length the peer declared.
//!
//! **The body is bounded before it is buffered.** `Incoming` has no default limit, so an
//! unauthenticated `POST` used to be able to ask this server to buffer whatever it chose to
//! send. It is read frame by frame against `MAX_IPP_BODY_BYTES` and anything larger is refused
//! with a well-formed `client-error-request-entity-too-large` and no LLM call.
//!
//! **The refusal is then drained, boundedly, before it is sent.** Answering 413 and closing
//! while the peer is still writing sends `RST`, which discards the response bytes already
//! written — so the peer's `write` fails and it never reads the refusal at all. See
//! `LINGER_DRAIN_BYTES`.

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
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::server::connection::ConnectionId;
use crate::server::IppProtocol;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use crate::{console_error, console_info};

use actions::{build_ipp_response, ipp_status_code};

/// How much of a request body is read before the request is refused.
///
/// `Incoming` has no default limit, so without this an unauthenticated `POST` decides how much
/// memory this server allocates. A `Print-Job`'s document data is the only IPP body that is
/// legitimately large, and this server keeps no job store and never looks past the 8-byte
/// header, so nothing here needs more than the bound `http_common` uses.
///
/// Spelled out rather than borrowed from `http_common::handler::MAX_REQUEST_BODY_BYTES`:
/// that module is gated behind the `http`/`http2` features and `ipp` does not imply either,
/// so referencing it would make `--features ipp` alone fail to build.
const MAX_IPP_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How many further octets are read and **discarded** after the cap is exceeded, so the peer
/// can finish writing and then read the refusal.
///
/// A server that answers 413 and closes while the peer is still writing does not deliver its
/// answer: closing a socket with unread data in the receive queue sends `RST`, which discards
/// the response bytes already written along with it. The peer's `write` fails with
/// `ECONNRESET` and it never learns why — so the care taken to express the refusal twice, at
/// the HTTP layer *and* in the IPP status, is spent on a message nobody receives.
///
/// This is nginx's `lingering_close`, and it is bounded for the same reason: draining is
/// politeness, not an obligation. Nothing is buffered — the octets are counted and dropped —
/// so the cost is bandwidth and the deadline below, and a peer that keeps writing past either
/// gets the abrupt close it earned.
const LINGER_DRAIN_BYTES: usize = 8 * 1024 * 1024;

/// Wall-clock bound on that drain, so a peer trickling one octet at a time cannot hold the
/// connection open by staying under [`LINGER_DRAIN_BYTES`].
const LINGER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Outcome of reading a request body under [`MAX_IPP_BODY_BYTES`].
enum BodyRead {
    /// The whole body arrived inside the cap.
    Complete(Bytes),
    /// The cap was passed. Never carries the partial body: handing the model the first 8 MiB
    /// of a longer request is the truncated-body-looks-like-a-complete-one shape this cap
    /// exists to avoid.
    TooLarge { drained: usize, whole_body: bool },
}

/// Read a request body, refusing past the cap and then draining politely.
///
/// The read is frame-by-frame rather than `Limited::collect()`, which cannot distinguish
/// "stop" from "stop and let the peer finish". Once the cap is passed the buffer is dropped
/// immediately — the refusal is already decided, so holding 8 MiB while draining would double
/// what an oversized request costs.
async fn read_body_bounded(body: Incoming) -> BodyRead {
    let mut body = std::pin::pin!(body);
    let mut buffered: Vec<u8> = Vec::new();
    let mut over = false;
    let mut drained = 0usize;
    let deadline = tokio::time::Instant::now() + LINGER_DRAIN_TIMEOUT;

    loop {
        let next = if over {
            // Only the drain is deadlined. A body inside the cap is hyper's to time out, and
            // borrowing this deadline for it would cap how long a legitimate 8 MiB Print-Job
            // may take to arrive.
            match tokio::time::timeout_at(deadline, body.frame()).await {
                Ok(next) => next,
                Err(_) => {
                    return BodyRead::TooLarge {
                        drained,
                        whole_body: false,
                    }
                }
            }
        } else {
            body.frame().await
        };

        let Some(frame) = next else {
            // End of body. Reached while draining, the peer finished writing and can read the
            // refusal; reached inside the cap, this is the whole request.
            return if over {
                BodyRead::TooLarge {
                    drained,
                    whole_body: true,
                }
            } else {
                BodyRead::Complete(Bytes::from(buffered))
            };
        };

        let Ok(frame) = frame else {
            // A transport error mid-body. Answering with what arrived would be answering a
            // request nobody sent, so this is a refusal either way; `whole_body` is false
            // because the peer is not in a state to read anything.
            return BodyRead::TooLarge {
                drained,
                whole_body: false,
            };
        };

        let Ok(data) = frame.into_data() else {
            continue; // trailers carry no body octets
        };

        if over {
            drained = drained.saturating_add(data.len());
            if drained >= LINGER_DRAIN_BYTES {
                return BodyRead::TooLarge {
                    drained,
                    whole_body: false,
                };
            }
            continue;
        }

        if buffered.len() + data.len() > MAX_IPP_BODY_BYTES {
            over = true;
            // Free the partial body now rather than at the end of the drain.
            buffered = Vec::new();
            drained = data.len();
            continue;
        }
        buffered.extend_from_slice(&data);
    }
}

/// IPP server that delegates request handling to LLM
pub struct IppServer;

impl IppServer {
    /// Spawn the IPP server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        _send_first: bool,
        server_id: crate::state::ServerId,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        console_info!(status_tx, "IPP server listening on {}", local_addr);

        let protocol = Arc::new(IppProtocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        info!("IPP connection {} from {}", connection_id, remote_addr);
                        Log::new(Some(&status_tx))
                            .info(format!("IPP connection from {}", remote_addr));

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
                                let io = TokioIo::new(stream);

                                // Clone for service closure
                                let status_for_service = status_tx_clone.clone();
                                let app_state_for_service = app_state_clone.clone();

                                // Create a service that handles IPP requests with LLM
                                let service = service_fn(move |req: Request<Incoming>| {
                                    let llm_clone = llm_client_clone.clone();
                                    let state_clone = app_state_for_service.clone();
                                    let status_clone = status_for_service.clone();
                                    let protocol_clone = protocol_clone.clone();
                                    handle_ipp_request_with_llm(
                                        req,
                                        connection_id,
                                        llm_clone,
                                        state_clone,
                                        status_clone,
                                        protocol_clone,
                                        server_id,
                                    )
                                });

                                // Serve HTTP/1 on this connection (IPP uses HTTP)
                                if let Err(err) =
                                    http1::Builder::new().serve_connection(io, service).await
                                {
                                    error!("Error serving IPP connection: {:?}", err);
                                }

                                // Mark connection as closed
                                app_state_clone
                                    .close_connection_on_server(server_id, connection_id)
                                    .await;
                                Log::new(Some(&status_tx_clone))
                                    .info(format!("IPP connection {} closed", connection_id));
                                let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                            })
                            .await;
                    }
                    Err(e) => {
                        console_error!(status_tx, "Failed to accept IPP connection: {}", e);
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

/// Handle a single IPP request with LLM
async fn handle_ipp_request_with_llm(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<IppProtocol>,
    server_id: crate::state::ServerId,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // Extract request details
    let method = req.method().to_string();
    let uri = req.uri().to_string();

    // Extract headers
    let mut headers = HashMap::new();
    for (name, value) in req.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(name.to_string(), value_str.to_string());
        }
    }

    // Read body (IPP operation data), bounded. `Limited` errors as soon as the cap is passed
    // rather than after buffering the whole thing, so an oversized request costs at most the
    // cap. Refusing here also means an oversized request never reaches the model.
    //
    // A read failure is NOT treated as an empty body: `Bytes::new()` would be handed to the
    // model as the operation `Empty`, so it would answer a request it never saw. IPP has a
    // status for exactly this case and it is used.
    let body_bytes = match read_body_bounded(req.into_body()).await {
        BodyRead::Complete(bytes) => bytes,
        BodyRead::TooLarge {
            drained,
            whole_body,
        } => {
            console_error!(
                status_tx,
                "IPP {} refusing request body; limit is {} bytes",
                connection_id,
                MAX_IPP_BODY_BYTES
            );
            Log::new(Some(&status_tx)).warn(format!(
                "IPP {} decision=fail_closed_body_too_large (limit {} bytes, drained {} more, \
                 peer finished writing: {})",
                connection_id, MAX_IPP_BODY_BYTES, drained, whole_body
            ));
            // 413 at the HTTP layer and the matching IPP status in the body: a client that
            // reads either one learns the same thing. request-id is unknown - the header was
            // never read - so it stays 0.
            let body = build_ipp_response(
                ipp_status_code("client-error-request-entity-too-large"),
                Some("request body exceeds this server's limit"),
                None,
            );
            return Ok(ipp_response_counted(&app_state, server_id, connection_id, 413, body).await);
        }
    };

    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(body_bytes.len() as u64),
            None,
            Some(1),
            None,
        )
        .await;

    Log::new(Some(&status_tx)).debug(format!(
        "IPP {} {} ({} bytes)",
        method,
        uri,
        body_bytes.len()
    ));

    // Parse the IPP header. The request-id must be echoed in the response or clients discard
    // it as unmatched, so it is parsed here and stamped in below - the model is never asked
    // for it and cannot get it wrong.
    let header = parse_ipp_header(&body_bytes);
    let operation_name = header
        .as_ref()
        .map(|h| h.operation.clone())
        .unwrap_or_else(|| {
            if body_bytes.is_empty() {
                "Empty".to_string()
            } else {
                "Malformed".to_string()
            }
        });
    let request_id = header.as_ref().map(|h| h.request_id).unwrap_or(0);
    let ipp_version = header
        .as_ref()
        .map(|h| format!("{}.{}", h.version_major, h.version_minor))
        .unwrap_or_else(|| "unknown".to_string());

    trace!(
        "IPP operation: {} (request-id {}, v{})",
        operation_name,
        request_id,
        ipp_version
    );

    // Create IPP request event
    let event = crate::protocol::Event::new(
        &actions::IPP_REQUEST_EVENT,
        serde_json::json!({
            "method": method,
            "uri": uri,
            "operation": operation_name,
            "request_id": request_id,
            "ipp_version": ipp_version,
        }),
    );

    let llm_result = crate::llm::action_helper::call_llm(
        &llm_client,
        &app_state,
        server_id,
        // Connection-scoped event handlers and connection-scoped scheduled tasks both key on
        // this. Passing `None` here (it was a TODO, and the id was in scope the whole time)
        // made both silently inapplicable to every IPP request.
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await;

    // Process action results to build HTTP response
    match llm_result {
        Ok(execution_result) => {
            // Look for IPP-specific response actions
            for result in execution_result.protocol_results {
                if let ActionResult::Custom { name, data } = result {
                    if name == "ipp_response" {
                        // `http_status` was range-checked in the executor before it was put
                        // here, so this is `as u16` on a value already proven to be 100..=599.
                        let status = data
                            .get("http_status")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(200) as u16;
                        let body_hex = data.get("body_hex").and_then(|v| v.as_str()).unwrap_or("");

                        // `unwrap_or_default()` here used to turn a decode failure into an
                        // EMPTY body, which is not a valid IPP message: the client reports a
                        // truncated response, exactly the failure the no-action branch below
                        // exists to avoid. The hex is written by `ipp_wire_response` and read
                        // here, so a failure means those two have drifted - loud, and answered
                        // with something parseable.
                        let Ok(mut body) = hex::decode(body_hex) else {
                            error!(
                                "IPP {} could not decode its own encoded response body \
                                 ({} chars); answering server-error-internal-error",
                                connection_id,
                                body_hex.len()
                            );
                            Log::new(Some(&status_tx)).warn(format!(
                                "IPP {} decision=fail_closed_encoding_bug",
                                connection_id
                            ));
                            return Ok(ipp_response_counted(
                                &app_state,
                                server_id,
                                connection_id,
                                200,
                                internal_error_body(header.as_ref(), request_id),
                            )
                            .await);
                        };

                        stamp_response_header(&mut body, header.as_ref(), request_id);

                        debug!(
                            "IPP {} decision=model_answer http={} request-id={} ({} bytes)",
                            connection_id,
                            status,
                            request_id,
                            body.len()
                        );
                        Log::new(Some(&status_tx)).debug(format!("IPP → {} response", status));

                        return Ok(ipp_response_counted(
                            &app_state,
                            server_id,
                            connection_id,
                            status,
                            body,
                        )
                        .await);
                    }
                }
                // Other actions don't affect HTTP response
            }

            // The LLM produced no IPP response action. An empty 200 is not a valid IPP message
            // and clients report it as a truncated response, so send a well-formed
            // server-error-internal-error instead of a body the client cannot parse.
            //
            // The `decision=` tag is the only place the distinction survives: on the wire this
            // is byte-identical to the backend-failure answer below, and to a model that chose
            // `ipp_status: "server-error-internal-error"` on purpose.
            let tag = if execution_result.failures.is_empty() {
                "model_silent"
            } else {
                "fail_closed_action_error"
            };
            debug!(
                "IPP {} decision={} - no ipp_* response action, \
                 answering server-error-internal-error",
                connection_id, tag
            );
            Log::new(Some(&status_tx)).warn(format!(
                "IPP {} decision={}: no ipp_* response action, sending \
                 server-error-internal-error",
                connection_id, tag
            ));
            Ok(ipp_response_counted(
                &app_state,
                server_id,
                connection_id,
                200,
                internal_error_body(header.as_ref(), request_id),
            )
            .await)
        }
        Err(e) => {
            // The peer gets a category, the log gets the error. IPP has no free-text field in
            // which a category string would be honest, so the category picks the *status*: a
            // saturated backend is transient and says so with `server-error-busy`, which is
            // what a client retries on, while anything else is `server-error-internal-error`.
            let failure = WireFailure::classify(&e);
            console_error!(
                status_tx,
                "IPP {} decision=fail_closed_llm_error: {}",
                connection_id,
                e
            );
            let (status_name, message) = match failure {
                WireFailure::Overloaded => (
                    "server-error-busy",
                    "netget: backend at capacity, retry later",
                ),
                WireFailure::Unavailable => (
                    "server-error-internal-error",
                    "netget: request could not be processed",
                ),
            };
            let mut body = build_ipp_response(ipp_status_code(status_name), Some(message), None);
            stamp_response_header(&mut body, header.as_ref(), request_id);
            // 200 with an IPP-level error, not an HTTP 500 with a text body: an IPP client
            // shown "Internal Server Error" reports a protocol error with no indication of
            // what went wrong.
            Ok(ipp_response_counted(&app_state, server_id, connection_id, 200, body).await)
        }
    }
}

/// Build the HTTP envelope and record the response's bytes against the connection.
///
/// Nothing recorded them, so an IPP server's `up` counter in the dashboard rail stayed at zero
/// for its whole life however much it printed. Building and counting in one call is what keeps
/// a future exit path from forgetting one of the two.
async fn ipp_response_counted(
    app_state: &Arc<AppState>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    status: u16,
    body: Vec<u8>,
) -> Response<Full<Bytes>> {
    let sent = body.len() as u64;
    app_state
        .update_connection_stats(server_id, connection_id, None, Some(sent), None, Some(1))
        .await;
    ipp_http_response(status, body)
}

/// Build the HTTP envelope IPP requires.
fn ipp_http_response(status: u16, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/ipp")
        .body(Full::new(Bytes::from(body)))
        // Only fails on an invalid status code, and `status` comes from a u16 we control or
        // clamp; fall back to a bare 500 rather than panicking in a connection task.
        .unwrap_or_else(|_| {
            Response::builder()
                .status(500)
                .body(Full::new(Bytes::new()))
                .expect("500 with an empty body is always constructible")
        })
}

/// A minimal, well-formed `server-error-internal-error` message.
fn internal_error_body(header: Option<&IppHeader>, request_id: u32) -> Vec<u8> {
    let mut body = vec![
        0x02, 0x00, // version 2.0
        0x05, 0x00, // server-error-internal-error
        0x00, 0x00, 0x00, 0x00, // request-id, stamped below
        0x01, // operation-attributes-tag
        0x47, // charset
    ];
    body.extend_from_slice(&[0x00, 0x12]);
    body.extend_from_slice(b"attributes-charset");
    body.extend_from_slice(&[0x00, 0x05]);
    body.extend_from_slice(b"utf-8");
    body.push(0x48); // naturalLanguage
    body.extend_from_slice(&[0x00, 0x1b]);
    body.extend_from_slice(b"attributes-natural-language");
    body.extend_from_slice(&[0x00, 0x05]);
    body.extend_from_slice(b"en-us");
    body.push(0x03); // end-of-attributes-tag
    stamp_response_header(&mut body, header, request_id);
    body
}

/// Write the request's own version and id into an encoded response.
///
/// Both are echoed by the server rather than asked of the model, so correctness cannot depend
/// on it repeating a number back — the same class of bug that made DNS and NTP responses go
/// unmatched by their clients.
///
/// - **request-id**: RFC 8011 requires the response's to equal the request's.
/// - **version**: RFC 8011 §4.1.8 requires the response to carry the version the client sent.
///   The encoders write 2.0; `ipptool`, which speaks 1.1 by default, failed every response
///   with "Bad version 2.0 in response - expected 1.1" until this echoed it back.
fn stamp_response_header(body: &mut [u8], header: Option<&IppHeader>, request_id: u32) {
    use actions::REQUEST_ID_OFFSET;

    if let (Some(header), true) = (header, body.len() >= 2) {
        body[0] = header.version_major;
        body[1] = header.version_minor;
    }
    if body.len() >= REQUEST_ID_OFFSET + 4 {
        body[REQUEST_ID_OFFSET..REQUEST_ID_OFFSET + 4].copy_from_slice(&request_id.to_be_bytes());
    }
}

/// The fixed 8-byte IPP message header.
struct IppHeader {
    version_major: u8,
    version_minor: u8,
    operation: String,
    request_id: u32,
}

/// Parse the IPP header: version(2) + operation-id(2) + request-id(4).
///
/// Every index below is covered by the length check; there is no slicing past it.
fn parse_ipp_header(body: &[u8]) -> Option<IppHeader> {
    if body.len() < 8 {
        return None;
    }

    let operation_id = u16::from_be_bytes([body[2], body[3]]);
    let request_id = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);

    Some(IppHeader {
        version_major: body[0],
        version_minor: body[1],
        operation: ipp_operation_name(operation_id),
        request_id,
    })
}

/// Map an IPP operation id to its name.
fn ipp_operation_name(operation_id: u16) -> String {
    let name = match operation_id {
        0x0002 => "Print-Job",
        0x0003 => "Print-URI",
        0x0004 => "Validate-Job",
        0x0005 => "Create-Job",
        0x0006 => "Send-Document",
        0x0007 => "Send-URI",
        0x0008 => "Cancel-Job",
        0x0009 => "Get-Job-Attributes",
        0x000A => "Get-Jobs",
        0x000B => "Get-Printer-Attributes",
        0x000C => "Hold-Job",
        0x000D => "Release-Job",
        0x000E => "Restart-Job",
        0x000F => "Pause-Printer",
        0x0010 => "Resume-Printer",
        0x0011 => "Purge-Jobs",
        0x0012 => "Set-Printer-Attributes",
        0x0013 => "Set-Job-Attributes",
        0x003B => "Close-Job",
        0x003C => "Identify-Printer",
        0x003D => "Validate-Document",
        _ => return format!("Operation-0x{:04X}", operation_id),
    };

    name.to_string()
}
