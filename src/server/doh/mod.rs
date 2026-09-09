//! DNS-over-HTTPS (DoH) server implementation
//!
//! Implements RFC 8484 DNS-over-HTTPS protocol using hickory-dns, hyper, and rustls.
//! The LLM controls DNS responses while NetGet handles the HTTPS transport layer.

pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::DohProtocol;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use actions::DOH_QUERY_EVENT;
use anyhow::{Context, Result};
use hickory_proto::op::Message as DnsMessage;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error};

/// Bound on the TLS handshake for one accepted connection.
///
/// Without it, a peer that completes the TCP handshake and then sends nothing holds
/// `acceptor.accept()` — and the task around it — open forever, at no cost to itself.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest DoH request body accepted, in bytes.
///
/// A DNS message is at most 65535 bytes by construction (its length is carried in 16 bits
/// wherever DNS is framed over a stream), so anything larger cannot be a DNS query. The body
/// was read with `req.collect()`, which is unbounded: over an established HTTP/2 connection a
/// peer could POST gigabytes to `/dns-query` and NetGet would buffer all of it in memory
/// before discovering it was not a DNS message. This is a hard cap with headroom, not a
/// tuning knob.
const MAX_DOH_BODY_BYTES: u64 = 65_535;

/// Pause after a failed `accept()` before trying again.
///
/// `accept` failing is usually transient (the peer went away between the SYN and the accept)
/// and retrying immediately is right. It is not always transient: at the file-descriptor
/// limit, `accept` returns `EMFILE` instantly and keeps doing so, and a bare `continue` then
/// spins the accept loop at full speed writing a warning per iteration onto an **unbounded**
/// status channel — turning "out of descriptors" into "out of memory". A short pause costs
/// nothing in the transient case and bounds the pathological one.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// DNS-over-HTTPS server
pub struct DohServer;

impl DohServer {
    /// Spawn the DoH server.
    ///
    /// The listener is bound here, *before* the accept loop is spawned, so that
    /// a bind failure (port in use, permission denied) is returned to the
    /// caller instead of being swallowed by the background task - otherwise the
    /// server would be reported as `Running` while nothing is listening.
    /// Binding here also means the returned address carries the real port when
    /// the caller asked for port 0.
    pub async fn spawn(
        bind_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        server_id: ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<SocketAddr> {
        // Generate TLS configuration (default self-signed cert), advertising `h2`.
        //
        // RFC 8484 DoH runs over HTTP/2, and this server speaks nothing else. Without ALPN in
        // the handshake a real client has no way to learn that: it either negotiates nothing
        // and falls back to HTTP/1.1, which this server cannot answer, or refuses outright.
        // The E2E test papered over it by connecting with `http2_prior_knowledge()`, which
        // skips ALPN entirely — so the gap could not show up there.
        let tls_config =
            crate::server::tls_cert_manager::generate_default_tls_config_with_alpn(&["h2"])
                .context("Failed to generate TLS configuration")?;

        Log::new(Some(&status_tx)).info(format!("Starting DoH server on {}", bind_addr));

        let listener = TcpListener::bind(bind_addr)
            .await
            .context("Failed to bind DoH TCP listener")?;

        // Actual bound address (important for port 0 dynamic allocation)
        let local_addr = listener
            .local_addr()
            .context("Failed to get DoH listener local address")?;

        Log::new(Some(&status_tx)).info(format!("DoH server listening on {}", local_addr));

        let task_registrar = app_state.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = Self::run(
                listener, tls_config, llm_client, app_state, server_id, status_tx,
            )
            .await
            {
                error!("DoH server error: {}", e);
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar.register_server_task(server_id, handle).await;

        Ok(local_addr)
    }

    /// Run the DoH accept loop on an already-bound listener
    async fn run(
        listener: TcpListener,
        tls_config: Arc<rustls::ServerConfig>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        server_id: ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let acceptor = TlsAcceptor::from(tls_config);
        let local_addr = listener
            .local_addr()
            .context("Failed to get DoH listener local address")?;

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    Log::new(Some(&status_tx))
                        .debug(format!("DoH TCP connection from {}", peer_addr));

                    // Register the peer before the handshake. Nothing tracked DoH
                    // connections at all before this, so a DoH server drew an empty peer
                    // list however many resolvers were talking to it, `{client_ip}` in the
                    // `doh_query` log template rendered empty, and the rail's byte counters
                    // stayed at zero.
                    let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);
                    {
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ServerConnectionState {
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
                                },
                            )
                            .await;
                    }
                    let _ = status_tx.send("__UPDATE_UI__".to_string());

                    let acceptor = acceptor.clone();
                    let llm_client = llm_client.clone();
                    let conn_state = app_state.clone();
                    let app_state = app_state.clone();
                    let status_tx = status_tx.clone();

                    // Registered, not detached: `stop_server` aborts the tasks a server
                    // registered, and an unregistered per-connection task outlives it, so a
                    // stopped DoH server went on answering every connection it had already
                    // accepted.
                    let handle = tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(
                            stream,
                            peer_addr,
                            connection_id,
                            acceptor,
                            llm_client,
                            app_state,
                            server_id,
                            status_tx,
                        )
                        .await
                        {
                            // `{:#}`: the cause chain is where rustls says what actually
                            // went wrong. `{}` prints only "TLS handshake failed", which
                            // names the step and not the reason.
                            error!("DoH connection error from {}: {:#}", peer_addr, e);
                        }
                    });
                    conn_state.register_server_task(server_id, handle).await;
                }
                Err(e) => {
                    Log::new(Some(&status_tx))
                        .warn(format!("Failed to accept DoH TCP connection: {}", e));
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            }
        }
    }

    /// Handle a single DoH connection
    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        acceptor: TlsAcceptor,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        server_id: ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let closer = app_state.clone();
        let close_tx = status_tx.clone();
        let outcome = Self::serve_connection(
            stream,
            peer_addr,
            connection_id,
            acceptor,
            llm_client,
            app_state,
            server_id,
            status_tx,
        )
        .await;

        // Mark the peer closed however the session ended — a failed handshake included, so
        // a connection that never got past TLS does not sit in the rail as live.
        closer
            .close_connection_on_server(server_id, connection_id)
            .await;
        let _ = close_tx.send("__UPDATE_UI__".to_string());
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    async fn serve_connection(
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        acceptor: TlsAcceptor,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        server_id: ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // Perform TLS handshake, bounded. An unbounded `accept` is a task a peer can park
        // forever by connecting and saying nothing.
        let tls_stream = timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "TLS handshake with {peer_addr} did not complete within {:?}",
                    TLS_HANDSHAKE_TIMEOUT
                )
            })?
            .context("TLS handshake failed")?;

        Log::new(Some(&status_tx)).debug(format!("DoH TLS handshake complete with {}", peer_addr));

        // Wrap in TokioIo for hyper compatibility
        let io = TokioIo::new(tls_stream);

        // Create service closure
        let service = service_fn(move |req: Request<hyper::body::Incoming>| {
            let llm_client = llm_client.clone();
            let app_state = app_state.clone();
            let status_tx = status_tx.clone();

            async move {
                Self::handle_request(
                    req,
                    peer_addr,
                    connection_id,
                    server_id,
                    llm_client,
                    app_state,
                    status_tx,
                )
                .await
            }
        });

        // Serve HTTP/2
        let result = http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(io, service)
            .await;

        if let Err(e) = result {
            debug!("DoH HTTP/2 connection error: {}", e);
        }

        Ok(())
    }

    /// Handle a single DoH HTTP request
    #[allow(clippy::too_many_arguments)]
    async fn handle_request(
        req: Request<hyper::body::Incoming>,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: ServerId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();

        Log::new(Some(&status_tx)).debug(format!("DoH request: {} {}", method, uri));

        // Extract DNS query based on method
        let dns_bytes = match method {
            Method::GET => {
                // Extract DNS query from ?dns= parameter (base64url encoded)
                let query = uri.query().unwrap_or("");
                let mut dns_param = None;

                for param in query.split('&') {
                    if let Some(value) = param.strip_prefix("dns=") {
                        dns_param = Some(value);
                        break;
                    }
                }

                match dns_param {
                    Some(encoded) => {
                        // Decode base64url
                        match base64_url_decode(encoded) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                Log::new(Some(&status_tx))
                                    .warn(format!("Invalid base64url DNS query: {}", e));
                                return Ok(error_response(
                                    StatusCode::BAD_REQUEST,
                                    "Invalid DNS query encoding",
                                ));
                            }
                        }
                    }
                    None => {
                        Log::new(Some(&status_tx))
                            .warn("Missing dns= parameter in DoH GET request");
                        return Ok(error_response(
                            StatusCode::BAD_REQUEST,
                            "Missing dns parameter",
                        ));
                    }
                }
            }
            Method::POST => {
                // Check Content-Type
                if let Some(content_type) = req.headers().get("content-type") {
                    if !is_dns_message_content_type(content_type) {
                        Log::new(Some(&status_tx))
                            .warn(format!("Invalid DoH Content-Type: {:?}", content_type));
                        return Ok(error_response(
                            StatusCode::BAD_REQUEST,
                            "Invalid Content-Type",
                        ));
                    }
                } else {
                    Log::new(Some(&status_tx)).warn("Missing Content-Type in DoH POST");
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        "Missing Content-Type",
                    ));
                }

                // Read the body under a hard cap.
                //
                // `req.collect()` alone is unbounded — a peer that has completed TLS and the
                // HTTP/2 preface can stream a body of any size and NetGet buffers all of it
                // before deciding it is not DNS. `Limited` stops reading at the cap and
                // errors, so the memory a single request can claim is bounded by a constant.
                use http_body_util::Limited;
                let limited = Limited::new(req.into_body(), MAX_DOH_BODY_BYTES as usize);
                match limited.collect().await {
                    Ok(collected) => collected.to_bytes().to_vec(),
                    Err(e) => {
                        Log::new(Some(&status_tx)).warn(format!(
                            "DoH POST body from {} exceeded {} bytes or could not be read: {}",
                            peer_addr, MAX_DOH_BODY_BYTES, e
                        ));
                        return Ok(error_response(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "DNS message too large",
                        ));
                    }
                }
            }
            _ => {
                Log::new(Some(&status_tx)).warn(format!("Unsupported DoH method: {}", method));
                return Ok(error_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "Only GET and POST are supported",
                ));
            }
        };

        let log = Log::new(Some(&status_tx));
        log.debug(format!("DoH received {} bytes", dns_bytes.len()));
        log.trace(format!("DoH DNS query hex: {}", hex::encode(&dns_bytes)));

        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(dns_bytes.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;

        // Parse DNS query
        let dns_message = match DnsMessage::from_vec(&dns_bytes) {
            Ok(msg) => msg,
            Err(e) => {
                Log::new(Some(&status_tx)).warn(format!("Failed to parse DoH DNS message: {}", e));
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid DNS message",
                ));
            }
        };

        // Extract query information
        let queries = dns_message.queries();
        if queries.is_empty() {
            Log::new(Some(&status_tx)).warn("DoH DNS message has no queries");
            return Ok(error_response(StatusCode::BAD_REQUEST, "No DNS queries"));
        }

        let query = &queries[0];
        let domain = query.name().to_utf8();
        // Display, not Debug: the model reads this string and echoes it back into
        // `send_dns_nxdomain`'s `query_type`, which parses it with `RecordType::from_str`.
        let query_type = query.query_type().to_string();
        let query_id = dns_message.id();

        Log::new(Some(&status_tx)).info(format!(
            "DoH query: {} {} (ID: {})",
            domain, query_type, query_id
        ));

        // Create event for LLM
        let event = Event::new(
            &DOH_QUERY_EVENT,
            json!({
                "query_id": query_id,
                "domain": domain,
                "query_type": query_type,
                "peer_addr": peer_addr.to_string(),
                "method": method.to_string(),
            }),
        );

        // Get protocol actions
        let protocol = Arc::new(DohProtocol::new());

        Log::new(Some(&status_tx)).debug(format!("DoH calling LLM for query from {}", peer_addr));

        // Call LLM
        let execution_result = match call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                // Answer SERVFAIL in a 200, the way DNS and DoT answer SERVFAIL on the wire.
                //
                // RFC 8484 §4.2.1 makes 200 the status for "the HTTP transaction carried a
                // DNS message"; whether that message is an answer or a failure is DNS's
                // business, not HTTP's. A 5xx instead tells the client the *resolver
                // endpoint* is broken, which makes real DoH clients mark the server down and
                // fail over — a heavier reaction than the transient backend hiccup that
                // caused it. The transaction id and question are echoed by `build_servfail`,
                // without which the client discards the message.
                //
                // `decision=`, as `src/server/radius/` does it: SERVFAIL is the same bytes
                // whatever went wrong, so the log is the only place the distinction lives.
                let decision = if crate::llm::is_overload_error(&e) {
                    "fail_closed_llm_overload"
                } else {
                    "fail_closed_llm_error"
                };
                Log::new(Some(&status_tx)).warn(format!(
                    "DoH answering SERVFAIL to {} decision={}: {}",
                    peer_addr, decision, e
                ));

                return Ok(
                    match crate::server::dns::actions::build_servfail(&dns_message) {
                        Ok(packet) => dns_message_response(packet),
                        Err(build_err) => {
                            // Nothing DNS-shaped can be produced, so the failure has to be
                            // expressed in HTTP. The peer gets a category, never the error text.
                            Log::new(Some(&status_tx)).error(format!(
                                "DoH failed to build SERVFAIL for {}: {}",
                                peer_addr, build_err
                            ));
                            let failure = crate::utils::WireFailure::classify(&e);
                            match failure {
                                crate::utils::WireFailure::Overloaded => retry_later_response(),
                                crate::utils::WireFailure::Unavailable => error_response(
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    failure.prefixed_text(),
                                ),
                            }
                        }
                    },
                );
            }
        };

        // Display messages from LLM
        for message in &execution_result.messages {
            Log::new(Some(&status_tx)).info(format!("{}", message));
        }

        Log::new(Some(&status_tx)).debug(format!(
            "DoH got {} protocol results",
            execution_result.protocol_results.len()
        ));

        // Execute actions from LLM response
        for protocol_result in &execution_result.protocol_results {
            use crate::llm::actions::protocol_trait::ActionResult;
            match protocol_result {
                ActionResult::Output(bytes) => {
                    // DNS action returned binary response directly
                    let log = Log::new(Some(&status_tx));
                    log.debug(format!("DoH sending {} bytes", bytes.len()));
                    log.trace(format!("DoH response hex: {}", hex::encode(bytes)));

                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            None,
                            Some(bytes.len() as u64),
                            None,
                            Some(1),
                        )
                        .await;

                    // Return DNS response with correct Content-Type.
                    // Content-Length is left to hyper, which derives it from the
                    // Full<Bytes> body - keeping every builder input constant so
                    // this cannot panic on model-supplied data.
                    return Ok(dns_message_response(bytes.clone()));
                }
                ActionResult::Custom { data, .. } => {
                    if let Some(output_data) = data.get("output_data").and_then(|v| v.as_str()) {
                        // Decode hex DNS response
                        if let Ok(response_bytes) = hex::decode(output_data) {
                            let log = Log::new(Some(&status_tx));
                            log.debug(format!("DoH sending {} bytes", response_bytes.len()));
                            log.trace(format!("DoH response hex: {}", output_data));

                            app_state
                                .update_connection_stats(
                                    server_id,
                                    connection_id,
                                    None,
                                    Some(response_bytes.len() as u64),
                                    None,
                                    Some(1),
                                )
                                .await;

                            // Return DNS response with correct Content-Type
                            return Ok(dns_message_response(response_bytes));
                        }
                    }
                }
                ActionResult::NoAction => {
                    // Ignore query - return empty response
                    Log::new(Some(&status_tx)).debug("DoH query ignored by LLM");
                    return Ok(error_response(StatusCode::NOT_FOUND, "Query ignored"));
                }
                _ => {}
            }
        }

        // Default: no response sent
        Ok(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "No response generated",
        ))
    }
}

/// Decode base64url (URL-safe base64 without padding)
fn base64_url_decode(encoded: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    URL_SAFE_NO_PAD
        .decode(encoded)
        .context("Failed to decode base64url")
}

/// Build a `200 application/dns-message` response carrying a DNS wire-format body.
///
/// Every header passed to the builder is a constant, so the `expect` below is
/// unreachable regardless of what the model or the network produced.
fn dns_message_response(body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/dns-message")
        .body(Full::new(Bytes::from(body)))
        .expect("constant status and headers always build a valid response")
}

/// Is this `Content-Type` `application/dns-message`?
///
/// RFC 9110 §8.3 makes the media type case-insensitive and allows parameters after a `;`, so
/// `Application/DNS-Message` and `application/dns-message; charset=utf-8` are both the same
/// type. The check was a byte-for-byte `!=` against the canonical spelling, which rejected
/// both as "Invalid Content-Type" — a conformant client turned away for being conformant.
fn is_dns_message_content_type(value: &hyper::header::HeaderValue) -> bool {
    let Ok(text) = value.to_str() else {
        return false;
    };
    let media_type = text.split(';').next().unwrap_or("").trim();
    media_type.eq_ignore_ascii_case("application/dns-message")
}

/// A `503` telling the client to come back, for a transient backend saturation.
///
/// Distinct from the 500 path on purpose: a client that sees 503 + `Retry-After` backs off,
/// where a 500 is recorded as a permanent fault. This mirrors what `src/server/http` does with
/// the same two `WireFailure` categories.
fn retry_later_response() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("Content-Type", "text/plain")
        .header("Retry-After", "1")
        .body(Full::new(Bytes::from(
            crate::utils::WireFailure::Overloaded.prefixed_text(),
        )))
        .expect("constant status and headers always build a valid response")
}

/// Create an error response
fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(message.to_string())))
        .expect("constant status and headers always build a valid response")
}
