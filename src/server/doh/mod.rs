//! DNS-over-HTTPS (DoH) server implementation
//!
//! Implements RFC 8484 DNS-over-HTTPS protocol using hickory-dns, hyper, and rustls.
//! The LLM controls DNS responses while NetGet handles the HTTPS transport layer.

pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
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
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error};

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

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    Log::new(Some(&status_tx))
                        .debug(format!("DoH TCP connection from {}", peer_addr));

                    let acceptor = acceptor.clone();
                    let llm_client = llm_client.clone();
                    let app_state = app_state.clone();
                    let status_tx = status_tx.clone();

                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(
                            stream, peer_addr, acceptor, llm_client, app_state, server_id,
                            status_tx,
                        )
                        .await
                        {
                            error!("DoH connection error from {}: {}", peer_addr, e);
                        }
                    });
                }
                Err(e) => {
                    Log::new(Some(&status_tx))
                        .warn(format!("Failed to accept DoH TCP connection: {}", e));
                }
            }
        }
    }

    /// Handle a single DoH connection
    async fn handle_connection(
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
        acceptor: TlsAcceptor,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        server_id: ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        // Perform TLS handshake
        let tls_stream = acceptor
            .accept(stream)
            .await
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
                Self::handle_request(req, peer_addr, server_id, llm_client, app_state, status_tx)
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
    async fn handle_request(
        req: Request<hyper::body::Incoming>,
        peer_addr: SocketAddr,
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
                    if content_type != "application/dns-message" {
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

                // Read request body
                let body = req.collect().await?.to_bytes();
                body.to_vec()
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
        let query_type = format!("{:?}", query.query_type());
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
            None,
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                Log::new(Some(&status_tx)).warn(format!("DoH LLM call failed: {}", e));
                return Ok(error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "LLM error",
                ));
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

/// Create an error response
fn error_response(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain")
        .body(Full::new(Bytes::from(message.to_string())))
        .expect("constant status and headers always build a valid response")
}
