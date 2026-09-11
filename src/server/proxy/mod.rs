//! HTTP/HTTPS Proxy server implementation using MITM with LLM control
//!
//! This module implements a sophisticated proxy server with:
//! - Full MITM capabilities with certificate generation/loading
//! - Pass-through mode for HTTPS (no decryption, allow/block only)
//! - LLM-controlled filtering and modification of requests/responses
//! - Regex-based filtering for selective interception

pub mod actions;
pub mod cert_cache;
pub mod filter;
pub mod tls_mitm;

use crate::server::connection::ConnectionId;
use anyhow::{Context, Result};
use cert_cache::CertificateCache;
use filter::{
    CertificateMode, FullRequestInfo, HttpsConnectionAction, HttpsConnectionInfo,
    ProxyFilterConfig, RequestAction,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::{ActionResult, Server};
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::ProxyProtocol;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use actions::{PROXY_HTTPS_CONNECT_EVENT, PROXY_HTTP_REQUEST_EVENT};

use rcgen::{Certificate, CertificateParams, KeyPair};
use regex::Regex;
use serde_json::json;

/// Largest upstream response `forward_http_request` will buffer before truncating.
const MAX_UPSTREAM_RESPONSE: usize = 8 * 1024 * 1024;

/// Largest client request head the proxy will read before giving up on it.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// How long a client has to send its request line and headers after connecting.
const REQUEST_READ_TIMEOUT_SECS: u64 = 30;

/// Re-assemble `host` and `port` into something `TcpStream::connect` accepts, re-bracketing an
/// IPv6 literal that the CONNECT parser stripped.
pub(crate) fn connect_authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Read an HTTP request head (through the blank line) plus whatever body bytes arrived with
/// it, bounded by `MAX_REQUEST_BYTES`.
///
/// Returns as soon as `\r\n\r\n` is seen; a peer that stops mid-head simply stops the future,
/// which the caller wraps in a timeout.
async fn read_request_head(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;

    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            // EOF: hand back whatever we have and let the caller decide.
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..n]);

        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() >= MAX_REQUEST_BYTES {
            anyhow::bail!("request head exceeded {MAX_REQUEST_BYTES} bytes without terminating");
        }
    }
}

/// Relay a CONNECT tunnel until both directions are finished, and report the byte counts.
///
/// This is `copy_bidirectional`, not two `tokio::io::copy` futures under `join!`, and the
/// difference is a connection leak rather than a style point. `tokio::io::copy` returns on EOF
/// without shutting down the write half it was feeding, so when the client half-closed its
/// side the destination never saw a FIN. A keep-alive upstream then held the socket open
/// indefinitely, the other `copy` never completed, `join!` never returned, and the connection
/// task -- plus its dashboard entry -- lived until the operating system gave up.
/// `copy_bidirectional` shuts down the opposite write half on EOF, which is what RFC 9110
/// tunnelling requires.
async fn tunnel_bidirectional(
    client_stream: &mut tokio::net::TcpStream,
    dest_stream: &mut tokio::net::TcpStream,
) -> (u64, u64) {
    match tokio::io::copy_bidirectional(client_stream, dest_stream).await {
        Ok((up, down)) => (up, down),
        Err(e) => {
            // A reset mid-tunnel is ordinary; the byte counts are lost with it.
            debug!("Proxy tunnel ended with an I/O error: {}", e);
            (0, 0)
        }
    }
}

/// HTTP/HTTPS Proxy server that intercepts and forwards requests via LLM
pub struct ProxyServer;

impl ProxyServer {
    /// Spawn HTTP Proxy server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        Log::new(Some(&status_tx)).info(format!(
            "Proxy server (action-based) starting on {}",
            listen_addr
        ));

        // Get or initialize proxy filter configuration
        let mut config = app_state
            .get_proxy_filter_config(server_id)
            .await
            .unwrap_or_else(|| {
                info!("No proxy filter config found, using defaults (MITM with cert generation)");
                ProxyFilterConfig::default()
            });

        // Apply startup parameters if provided
        if let Some(ref params) = startup_params {
            Log::new(Some(&status_tx)).info("Applying proxy startup parameters");

            // Parse certificate_mode
            if let Some(cert_mode_str) = params.get_optional_string("certificate_mode")? {
                config.certificate_mode = match cert_mode_str.as_str() {
                    "generate" => CertificateMode::Generate,
                    "none" => CertificateMode::None,
                    "load_from_file" => {
                        let cert_path = params
                            .get_optional_string("cert_path")?
                            .context("Missing cert_path for load_from_file mode")?;
                        let key_path = params
                            .get_optional_string("key_path")?
                            .context("Missing key_path for load_from_file mode")?;
                        CertificateMode::LoadFromFile {
                            cert_path: cert_path.into(),
                            key_path: key_path.into(),
                        }
                    }
                    _ => {
                        warn!("Invalid certificate_mode: {}, using default", cert_mode_str);
                        config.certificate_mode
                    }
                };
                Log::new(Some(&status_tx))
                    .info(format!("Certificate mode: {:?}", config.certificate_mode));
            }

            // Parse filter modes
            if let Some(mode_str) = params.get_optional_string("request_filter_mode")? {
                if let Ok(mode) = serde_json::from_value(json!(mode_str)) {
                    Log::new(Some(&status_tx)).info(format!("Request filter mode: {mode:?}"));
                    config.request_filter_mode = mode;
                }
            }

            if let Some(mode_str) = params.get_optional_string("response_filter_mode")? {
                if let Ok(mode) = serde_json::from_value(json!(mode_str)) {
                    Log::new(Some(&status_tx)).info(format!("Response filter mode: {mode:?}"));
                    config.response_filter_mode = mode;
                }
            }

            if let Some(mode_str) = params.get_optional_string("https_connection_filter_mode")? {
                if let Ok(mode) = serde_json::from_value(json!(mode_str)) {
                    Log::new(Some(&status_tx))
                        .info(format!("HTTPS connection filter mode: {:?}", mode));
                    config.https_connection_filter_mode = mode;
                }
            }
        }

        // Generate or load certificate based on configuration
        let cert_cache: Option<Arc<CertificateCache>> = match &config.certificate_mode {
            CertificateMode::Generate => {
                Log::new(Some(&status_tx)).info("Generating self-signed CA certificate for MITM");
                let (ca_cert, ca_key, ca_params) = Self::generate_ca_certificate()?;
                Some(Arc::new(CertificateCache::new(ca_cert, ca_key, ca_params)))
            }
            CertificateMode::LoadFromFile {
                cert_path,
                key_path,
            } => {
                // Loading an operator-supplied CA is deliberately refused rather
                // than approximated. The previous implementation read the key,
                // ignored the certificate file entirely, and minted a *different*
                // self-signed CA from the operator's private key. Clients that
                // trusted the real CA rejected the result, so interception
                // silently failed while appearing configured -- and the operator's
                // CA key had been used to sign a certificate they never asked for.
                //
                // Using the real certificate requires reading its subject, which
                // needs rcgen's "x509-parser" feature (not currently enabled in
                // Cargo.toml).
                return Err(anyhow::anyhow!(
                    "certificate_mode 'load_from_file' is not implemented (cert_path={:?}, \
                     key_path={:?}). Use certificate_mode 'generate' and distribute the \
                     generated CA via the ca_export_path startup parameter, or 'none' for \
                     pass-through mode.",
                    cert_path,
                    key_path
                ));
            }
            CertificateMode::None => {
                Log::new(Some(&status_tx))
                    .info("Proxy running in pass-through mode (no MITM, origin certificates)");
                None
            }
        };

        // Export the CA certificate if the operator asked for it. Only the public
        // certificate is written; the private key never leaves memory.
        if let (Some(cache), Some(params)) = (cert_cache.as_ref(), startup_params.as_ref()) {
            if let Some(path) = params.get_optional_string("ca_export_path")? {
                std::fs::write(&path, cache.ca_cert_pem())
                    .with_context(|| format!("Failed to write CA certificate to {}", path))?;
                Log::new(Some(&status_tx)).info(format!(
                    "MITM CA certificate written to {} - clients must trust this file \
                     for interception to work",
                    path
                ));
            }
        }

        // Save the config back to state
        app_state
            .set_proxy_filter_config(server_id, config.clone())
            .await;

        let protocol = Arc::new(ProxyProtocol::new());

        // Start TCP listener for proxy connections
        let listener = tokio::net::TcpListener::bind(listen_addr)
            .await
            .context("Failed to bind proxy listener")?;

        let actual_addr = listener
            .local_addr()
            .context("Failed to get local address")?;

        Log::new(Some(&status_tx)).info(format!("Proxy server listening on {}", actual_addr));

        if cert_cache.is_some() {
            Log::new(Some(&status_tx))
                .info("MITM mode enabled - full HTTPS decryption and inspection");
        } else {
            Log::new(Some(&status_tx)).info("Pass-through mode - HTTPS allow/block only");
        }

        // Spawn cache cleanup task if MITM mode is enabled.
        //
        // Only ONE task handle can be registered per server, and that slot belongs
        // to the accept loop (registering a second one silently drops the first,
        // leaving the port held after stop_server). So this task holds a *weak*
        // reference and exits on its own once the accept loop is aborted and the
        // last strong reference to the cache goes away.
        if let Some(ref cache) = cert_cache {
            let cache_weak = Arc::downgrade(cache);
            let status_tx_clone = status_tx.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600)); // 1 hour
                loop {
                    interval.tick().await;
                    let Some(cache_clone) = cache_weak.upgrade() else {
                        debug!("Proxy server stopped, ending certificate cache cleanup task");
                        break;
                    };
                    debug!("Running periodic certificate cache cleanup");
                    cache_clone.cleanup_expired().await;
                    let stats = cache_clone.get_stats().await;
                    // Hourly cache stats are a summary: file-only DEBUG.
                    Log::new(Some(&status_tx_clone)).debug(format!(
                        "Certificate cache stats: {} total, {} valid, {} expired",
                        stats.total_certificates,
                        stats.valid_certificates,
                        stats.expired_certificates
                    ));
                }
            });
            Log::new(Some(&status_tx)).info("Certificate cache cleanup task started (hourly)");
        }

        // Spawn proxy handler task
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            Log::new(Some(&status_tx)).debug("Proxy accept loop started");
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        Log::new(Some(&status_tx)).info(format!(
                            "Proxy connection {} from {}",
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
                            local_addr: actual_addr,
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
                        let config_clone = config.clone();
                        let cert_cache_clone = cert_cache.clone();

                        // Handle each proxy connection in a separate task
                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_proxy_connection(
                                stream,
                                peer_addr,
                                connection_id,
                                server_id,
                                cert_cache_clone,
                                config_clone,
                                llm_clone,
                                app_clone.clone(),
                                status_clone.clone(),
                                protocol_clone,
                            )
                            .await
                            {
                                Log::new(Some(&status_clone)).error(format!(
                                    "Proxy connection {} error: {}",
                                    connection_id, e
                                ));
                            }

                            // Mark connection as closed
                            app_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_clone))
                                .info(format!("Proxy connection {} closed", connection_id));
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept proxy connection: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(actual_addr)
    }

    /// Handle a single proxy connection
    async fn handle_proxy_connection(
        mut stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: ServerId,
        cert_cache: Option<Arc<CertificateCache>>,
        config: ProxyFilterConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<ProxyProtocol>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        Log::new(Some(&status_tx)).info(format!(
            "Proxy: handling connection {} from {}",
            connection_id, peer_addr
        ));

        // Read the initial HTTP request, bounded in both size and time.
        //
        // A single `read()` with no deadline was two problems at once. A peer that connected
        // and then said nothing parked this task forever -- unauthenticated, so any number of
        // them -- and a request head split across TCP segments (any client that writes the
        // request line and the headers separately, and any head over the MTU) was parsed from
        // whatever the first segment happened to contain. Read until the head is complete,
        // stop at `MAX_REQUEST_BYTES`, and give up after `REQUEST_READ_TIMEOUT_SECS`.
        let buffer = match tokio::time::timeout(
            std::time::Duration::from_secs(REQUEST_READ_TIMEOUT_SECS),
            read_request_head(&mut stream),
        )
        .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => return Err(e).context("Failed to read initial request"),
            Err(_) => {
                debug!(
                    "Proxy connection {} timed out waiting for a request head",
                    connection_id
                );
                let _ = stream
                    .write_all(b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return Ok(());
            }
        };
        let n = buffer.len();

        Log::new(Some(&status_tx)).debug(format!(
            "Proxy connection {} received {} bytes",
            connection_id, n
        ));

        if n == 0 {
            debug!("Client closed connection before sending data");
            return Ok(()); // Client closed connection
        }

        let request_data = &buffer[..n];
        let request_str = String::from_utf8_lossy(request_data);

        debug!(
            "Proxy {} received request:\n{}",
            connection_id,
            if request_str.len() > 200 {
                format!(
                    "{}... ({} bytes total)",
                    truncate_str(&request_str, 200),
                    request_str.len()
                )
            } else {
                request_str.to_string()
            }
        );
        Log::new(Some(&status_tx)).debug(format!("Proxy {} parsing request", connection_id));

        // Parse the request line
        let first_line = request_str.lines().next().context("Empty request")?;

        Log::new(Some(&status_tx)).debug(format!("Request line: {}", first_line));

        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 3 {
            error!("Invalid HTTP request line: {}", first_line);
            return Err(anyhow::anyhow!("Invalid HTTP request line"));
        }

        let method = parts[0];
        let uri = parts[1];

        Log::new(Some(&status_tx)).debug(format!("Parsed: method={}, uri={}", method, uri));

        // Check if this is an HTTPS CONNECT request
        if method == "CONNECT" {
            // HTTPS tunneling request
            return Self::handle_https_connect(
                stream,
                uri,
                peer_addr,
                connection_id,
                server_id,
                cert_cache,
                config,
                llm_client,
                app_state,
                status_tx,
                protocol,
            )
            .await;
        } else {
            // Regular HTTP request
            return Self::handle_http_request(
                stream,
                request_data,
                method,
                uri,
                peer_addr,
                connection_id,
                server_id,
                config,
                llm_client,
                app_state,
                status_tx,
                protocol,
            )
            .await;
        }
    }

    /// Handle HTTPS CONNECT request (tunneling)
    async fn handle_https_connect(
        mut client_stream: tokio::net::TcpStream,
        uri: &str,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: ServerId,
        cert_cache: Option<Arc<CertificateCache>>,
        config: ProxyFilterConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<ProxyProtocol>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let start_time = std::time::Instant::now();

        // Parse host:port from CONNECT uri.
        //
        // Split on the *last* colon and strip the brackets an IPv6 literal is required to
        // carry (RFC 9110 authority-form, RFC 3986 section 3.2.2). Splitting on every colon
        // and demanding exactly two pieces rejected `[::1]:443` outright, so no client could
        // CONNECT to an IPv6 destination through this proxy.
        let (dest_host, port_str) = uri
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("Invalid CONNECT uri (no port): {}", uri))?;
        let dest_host = dest_host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(dest_host);
        if dest_host.is_empty() {
            return Err(anyhow::anyhow!("Invalid CONNECT uri (empty host): {}", uri));
        }
        let dest_port: u16 = port_str
            .parse()
            .context("Invalid port in CONNECT request")?;

        // TRACE: Log full connection details (metadata only, no content in pass-through)
        trace!(
            "HTTPS CONNECT from {} to {}:{}",
            peer_addr,
            dest_host,
            dest_port
        );
        trace!("  SNI: {} (from CONNECT)", dest_host);
        trace!(
            "  Certificate mode: {:?}",
            if cert_cache.is_some() {
                "MITM"
            } else {
                "Pass-through"
            }
        );
        Log::new(Some(&status_tx)).trace(format!(
            "HTTPS CONNECT {} -> {}:{} ({})",
            peer_addr,
            dest_host,
            dest_port,
            if cert_cache.is_some() {
                "MITM"
            } else {
                "pass-through"
            }
        ));

        if let Some(cache) = cert_cache {
            // MITM mode - full decryption and inspection
            info!(
                "MITM mode: will decrypt and inspect HTTPS traffic for {}:{}",
                dest_host, dest_port
            );

            // Call MITM implementation
            return tls_mitm::perform_mitm(
                client_stream,
                dest_host,
                dest_port,
                peer_addr,
                connection_id,
                server_id,
                cache,
                config,
                llm_client,
                app_state,
                status_tx,
                protocol,
            )
            .await;
        }

        // Pass-through mode or fallback - no decryption
        // Note: SNI could be extracted from TLS handshake, but we use dest_host for now
        let client_addr_str = peer_addr.to_string();
        if config.should_intercept_https_connection(
            dest_host,
            dest_port,
            Some(dest_host), // SNI - could be extracted from TLS handshake
            &client_addr_str,
        ) {
            // Consult LLM about whether to allow this HTTPS connection
            let conn_info = HttpsConnectionInfo {
                destination_host: dest_host.to_string(),
                destination_port: dest_port,
                sni: Some(dest_host.to_string()),
                client_addr: client_addr_str,
            };

            info!(
                "Consulting LLM about HTTPS connection to {}:{}",
                dest_host, dest_port
            );

            // Consult LLM
            let action = Self::consult_llm_https_connection(
                &conn_info,
                server_id,
                &llm_client,
                &app_state,
                &protocol,
                &status_tx,
            )
            .await
            .unwrap_or_else(|e| {
                // Blocking on failure was already right here; the HTTP path above is the one
                // that fell open. Non-fatal (the client gets a 403), so WARN not ERROR.
                Log::new(Some(&status_tx)).warn(format!(
                    "Proxy blocking CONNECT {}:{}: {} - LLM consultation failed",
                    dest_host, dest_port, e
                ));
                HttpsConnectionAction::Block {
                    reason: Some(
                        "netget proxy: no filtering decision could be obtained".to_string(),
                    ),
                }
            });

            match action {
                HttpsConnectionAction::Allow => {
                    Log::new(Some(&status_tx))
                        .info(format!("Allowed HTTPS to {}:{}", dest_host, dest_port));

                    // Establish connection to destination
                    let dest_addr = connect_authority(dest_host, dest_port);
                    let mut dest_stream = tokio::net::TcpStream::connect(&dest_addr)
                        .await
                        .context("Failed to connect to destination")?;

                    // Send 200 Connection Established to client
                    client_stream
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await?;

                    // Bidirectional copy between client and destination
                    let (up_bytes, down_bytes) =
                        tunnel_bidirectional(&mut client_stream, &mut dest_stream).await;
                    let total_bytes = up_bytes + down_bytes;
                    // Without this the rail's ↓/↑ counters stay at zero for a tunnel that may
                    // have carried gigabytes.
                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            Some(up_bytes),
                            Some(down_bytes),
                            None,
                            None,
                        )
                        .await;

                    let duration = start_time.elapsed();

                    // DEBUG: Access log (pass-through - no HTTP status)
                    Log::new(Some(&status_tx)).debug(format!(
                        "[ACCESS] {} CONNECT {}:{} -> TUNNEL {} bytes ({} up, {} down) in {:?}",
                        peer_addr,
                        dest_host,
                        dest_port,
                        total_bytes,
                        up_bytes,
                        down_bytes,
                        duration
                    ));

                    trace!("HTTPS tunnel closed: {} bytes transferred", total_bytes);

                    Ok(())
                }
                HttpsConnectionAction::Block { reason } => {
                    let duration = start_time.elapsed();
                    let reason_str = reason.clone().unwrap_or_default();

                    // DEBUG: Access log
                    Log::new(Some(&status_tx)).debug(format!(
                        "[ACCESS] {} CONNECT {}:{} -> 403 {} in {:?}",
                        peer_addr,
                        dest_host,
                        dest_port,
                        reason_str.len(),
                        duration
                    ));

                    // Send 403 Forbidden to client
                    let response = format!(
                        "HTTP/1.1 403 Forbidden\r\n\
                         Content-Type: text/plain\r\n\
                         Content-Length: {}\r\n\
                         \r\n\
                         {}",
                        reason_str.len(),
                        reason_str
                    );
                    client_stream.write_all(response.as_bytes()).await?;

                    Ok(())
                }
            }
        } else {
            // Filter mode is "none" or doesn't match - pass through without LLM
            trace!(
                "Pass-through HTTPS connection to {}:{} (no LLM consultation)",
                dest_host,
                dest_port
            );

            // Establish connection to destination
            let dest_addr = connect_authority(dest_host, dest_port);
            let mut dest_stream = tokio::net::TcpStream::connect(&dest_addr)
                .await
                .context("Failed to connect to destination")?;

            // Send 200 Connection Established to client
            client_stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;

            // Bidirectional copy
            let (up_bytes, down_bytes) =
                tunnel_bidirectional(&mut client_stream, &mut dest_stream).await;
            let total_bytes = up_bytes + down_bytes;
            app_state
                .update_connection_stats(
                    server_id,
                    connection_id,
                    Some(up_bytes),
                    Some(down_bytes),
                    None,
                    None,
                )
                .await;

            let duration = start_time.elapsed();

            // DEBUG: Access log
            Log::new(Some(&status_tx)).debug(format!(
                "[ACCESS] {} CONNECT {}:{} -> TUNNEL {} bytes ({} up, {} down) in {:?}",
                peer_addr, dest_host, dest_port, total_bytes, up_bytes, down_bytes, duration
            ));

            Ok(())
        }
    }

    /// Handle regular HTTP request (no TLS)
    async fn handle_http_request(
        mut client_stream: tokio::net::TcpStream,
        request_data: &[u8],
        method: &str,
        uri: &str,
        peer_addr: SocketAddr,
        _connection_id: ConnectionId,
        server_id: ServerId,
        config: ProxyFilterConfig,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<ProxyProtocol>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let start_time = std::time::Instant::now();

        // Parse HTTP request
        let request_str = String::from_utf8_lossy(request_data);
        let mut headers = HashMap::new();
        let mut body_start = 0;

        // Parse headers
        for (i, line) in request_str.lines().enumerate() {
            if i == 0 {
                continue; // Skip request line
            }
            if line.is_empty() {
                // End of headers
                body_start =
                    request_str[..request_str.find("\r\n\r\n").unwrap_or(request_str.len())].len()
                        + 4;
                break;
            }
            if let Some(colon_pos) = line.find(':') {
                let name = line[..colon_pos].trim().to_string();
                let value = line[colon_pos + 1..].trim().to_string();
                headers.insert(name, value);
            }
        }

        let body = if body_start < request_data.len() {
            &request_data[body_start..]
        } else {
            &[]
        };

        // Extract host from headers or URI
        let host = headers
            .get("Host")
            .map(|s| s.as_str())
            .or_else(|| {
                // Try to extract from absolute URI
                if uri.starts_with("http://") {
                    uri.strip_prefix("http://")
                        .and_then(|s| s.split('/').next())
                } else {
                    None
                }
            })
            .unwrap_or("unknown");

        let path = if uri.starts_with("http://") {
            // Absolute URI - extract path
            uri.find('/').map(|pos| &uri[pos..]).unwrap_or("/")
        } else {
            uri
        };

        // TRACE: Log full request details
        trace!("Proxy HTTP request: {} {} from {}", method, uri, peer_addr);
        trace!("  Headers: {:#?}", headers);
        if !body.is_empty() {
            if let Ok(body_str) = std::str::from_utf8(body) {
                trace!("  Body: {}", body_str);
            } else {
                trace!("  Body: {} bytes (binary)", body.len());
            }
        }
        Log::new(Some(&status_tx)).trace(format!(
            "Proxy request: {} {} from {} ({} bytes body)",
            method,
            uri,
            peer_addr,
            body.len()
        ));

        // Check if we should intercept this request
        if config.should_intercept_request(host, path, method, &headers, body) {
            Log::new(Some(&status_tx)).debug("Request matched filters, consulting LLM");

            // Build request info for LLM
            let request_info = FullRequestInfo {
                method: method.to_string(),
                url: uri.to_string(),
                path: path.to_string(),
                host: host.to_string(),
                headers: headers.clone(),
                body: body.to_vec(),
                client_addr: peer_addr.to_string(),
            };

            // Consult LLM
            let action = Self::consult_llm_http_request(
                &request_info,
                server_id,
                &llm_client,
                &app_state,
                &protocol,
                &status_tx,
            )
            .await
            .unwrap_or_else(|e| {
                // Fail closed. This used to default to `Pass`, which forwarded the request to
                // its destination unfiltered - so a proxy whose entire purpose is to let the
                // model decide what may leave the network became an open relay for exactly as
                // long as the backend was down, and the access log recorded each request as
                // having been passed on purpose. The HTTPS half of this same handler has
                // always defaulted to Block; only this one fell open.
                //
                // 502 is the proxy's own "I could not complete this on your behalf"; 503 when
                // the backend is merely saturated, which a client retries.
                let overloaded = crate::llm::is_overload_error(&e);
                let status = if overloaded { 503 } else { 502 };
                // Non-fatal: the client gets a 502/503, so WARN not ERROR.
                Log::new(Some(&status_tx)).warn(format!(
                    "Proxy blocking {} {} with {} (overload={}): {}",
                    request_info.method, request_info.url, status, overloaded, e
                ));
                RequestAction::Block {
                    status,
                    body: "netget proxy: no filtering decision could be obtained, request refused"
                        .to_string(),
                }
            });

            match action {
                RequestAction::Pass => {
                    info!("LLM passed request through");
                    // Forward request to destination
                    Self::forward_http_request(
                        client_stream,
                        request_data,
                        host,
                        method,
                        uri,
                        peer_addr,
                        start_time,
                        status_tx,
                    )
                    .await
                }
                RequestAction::Block { status, body } => {
                    let duration = start_time.elapsed();
                    let body_len = body.len();

                    // DEBUG: Access log
                    Log::new(Some(&status_tx)).debug(format!(
                        "[ACCESS] {} {} {} -> {} {} bytes in {:?}",
                        peer_addr, method, uri, status, body_len, duration
                    ));

                    // TRACE: Full response details
                    trace!(
                        "Blocking response: status={}, body_len={}",
                        status,
                        body_len
                    );
                    trace!("  Response body: {}", body);

                    let response = format!(
                        "HTTP/1.1 {} Blocked\r\n\
                         Content-Type: text/plain\r\n\
                         Content-Length: {}\r\n\
                         \r\n\
                         {}",
                        status, body_len, body
                    );
                    client_stream.write_all(response.as_bytes()).await?;
                    Ok(())
                }
                ref modify_action @ RequestAction::Modify { .. } => {
                    Log::new(Some(&status_tx)).debug("LLM requested modifications, applying");

                    // Apply modifications
                    let modified_request =
                        Self::apply_request_modifications(request_data, modify_action)
                            .unwrap_or_else(|e| {
                                // Non-fatal: falls back to forwarding the request unmodified.
                                Log::new(Some(&status_tx)).warn(format!(
                                    "Proxy modification error: {} - forwarding unmodified",
                                    e
                                ));
                                request_data.to_vec()
                            });

                    // Forward modified request
                    Self::forward_http_request(
                        client_stream,
                        &modified_request,
                        host,
                        method,
                        uri,
                        peer_addr,
                        start_time,
                        status_tx,
                    )
                    .await
                }
            }
        } else {
            // Pass through without LLM consultation
            info!("Request doesn't match filters, passing through");
            Self::forward_http_request(
                client_stream,
                request_data,
                host,
                method,
                uri,
                peer_addr,
                start_time,
                status_tx,
            )
            .await
        }
    }

    /// Forward HTTP request to destination and return response to client
    async fn forward_http_request(
        mut client_stream: tokio::net::TcpStream,
        request_data: &[u8],
        host: &str,
        method: &str,
        uri: &str,
        peer_addr: SocketAddr,
        start_time: std::time::Instant,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Parse host:port
        let (dest_host, dest_port) = if let Some(colon_pos) = host.find(':') {
            (
                &host[..colon_pos],
                host[colon_pos + 1..].parse().unwrap_or(80),
            )
        } else {
            (host, 80)
        };

        info!("Forwarding to {}:{}", dest_host, dest_port);

        // Rewrite the request-target from proxy absolute-form to origin-form.
        //
        // A client talking to a proxy sends "GET http://host/path HTTP/1.1"
        // (RFC 9112 3.2.2 absolute-form). Origin servers expect origin-form
        // ("GET /path HTTP/1.1") -- RFC 9112 3.2.1 requires a proxy to convert
        // before forwarding. Forwarding verbatim made every plain-HTTP request
        // through this proxy fail: python's http.server answers 404 because it
        // treats the whole absolute URI as a filename.
        let forwarded = Self::to_origin_form(request_data);

        // Connect to destination
        let dest_addr = format!("{}:{}", dest_host, dest_port);
        let mut dest_stream = tokio::net::TcpStream::connect(&dest_addr)
            .await
            .context(format!("Failed to connect to {}", dest_addr))?;

        // Send request to destination
        dest_stream.write_all(&forwarded).await?;
        trace!("Sent {} bytes to upstream {}", forwarded.len(), dest_addr);

        // Read response from destination, bounded. The whole response is buffered here before
        // any of it reaches the client, so without a cap a destination -- which the *peer*
        // chose, not the operator -- decides how much memory this process allocates. On
        // loopback that is gigabytes inside the 5s read window.
        let mut response_buffer = Vec::new();
        let mut temp_buffer = [0u8; 8192];
        let mut content_length: Option<usize> = None;
        let mut headers_complete = false;
        let mut headers_end = 0;

        loop {
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                dest_stream.read(&mut temp_buffer),
            )
            .await
            {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(n)) => {
                    response_buffer.extend_from_slice(&temp_buffer[..n]);

                    if response_buffer.len() > MAX_UPSTREAM_RESPONSE {
                        Log::new(Some(&status_tx)).warn(format!(
                            "Proxy upstream {} sent more than {} bytes; truncating the response",
                            dest_addr, MAX_UPSTREAM_RESPONSE
                        ));
                        response_buffer.truncate(MAX_UPSTREAM_RESPONSE);
                        break;
                    }

                    // Parse Content-Length if we haven't yet
                    if !headers_complete && response_buffer.len() > 4 {
                        let response_str = String::from_utf8_lossy(&response_buffer);
                        if let Some(header_end) = response_str.find("\r\n\r\n") {
                            headers_complete = true;
                            headers_end = header_end + 4;

                            // Extract Content-Length
                            for line in response_str[..header_end].lines() {
                                if line.to_lowercase().starts_with("content-length:") {
                                    if let Some(len_str) = line.split(':').nth(1) {
                                        content_length = len_str.trim().parse().ok();
                                    }
                                }
                            }
                        }
                    }

                    // Check if we have full response
                    if headers_complete {
                        if let Some(len) = content_length {
                            if response_buffer.len() >= headers_end + len {
                                break; // Have complete response
                            }
                        } else {
                            // No Content-Length, wait a bit more
                            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                            break;
                        }
                    }
                }
                Ok(Err(e)) => {
                    error!("Error reading from destination: {}", e);
                    break;
                }
                Err(_) => {
                    debug!("Timeout reading from destination, proceeding with what we have");
                    break;
                }
            }
        }

        // Parse response status for access log
        let status =
            if let Some(first_line) = String::from_utf8_lossy(&response_buffer).lines().next() {
                first_line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            } else {
                0
            };

        let duration = start_time.elapsed();

        // DEBUG: Access log
        Log::new(Some(&status_tx)).debug(format!(
            "[ACCESS] {} {} {} -> {} {} bytes in {:?}",
            peer_addr,
            method,
            uri,
            status,
            response_buffer.len(),
            duration
        ));

        // TRACE: Full response details
        if response_buffer.len() > 0 {
            let response_str = String::from_utf8_lossy(&response_buffer);
            let lines: Vec<&str> = response_str.lines().collect();
            if !lines.is_empty() {
                trace!("Response status line: {}", lines[0]);
                trace!("Response headers:");
                for line in &lines[1..] {
                    if line.is_empty() {
                        break;
                    }
                    trace!("  {}", line);
                }
            }
        }

        // Send response back to client
        client_stream.write_all(&response_buffer).await?;
        trace!("Forwarded {} bytes to client", response_buffer.len());

        Ok(())
    }

    /// Consult LLM about an HTTP request
    async fn consult_llm_http_request(
        request_info: &FullRequestInfo,
        server_id: ServerId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        protocol: &Arc<ProxyProtocol>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<RequestAction> {
        Log::new(Some(&status_tx)).debug("Consulting LLM about HTTP request...");

        // Create HTTP request event
        let event = Event::new(
            &PROXY_HTTP_REQUEST_EVENT,
            json!({
                "method": request_info.method,
                "url": request_info.url,
                "host": request_info.host,
                "path": request_info.path,
            }),
        );

        let execution_result = call_llm(
            llm_client,
            app_state,
            server_id,
            None, // TODO: Add connection_id for proxy requests
            &event,
            protocol.as_ref() as &dyn Server,
        )
        .await
        .context("LLM request failed")?;

        // Extract request action from protocol results
        for result in execution_result.protocol_results {
            if let ActionResult::Output(bytes) = result {
                // Deserialize the RequestAction
                let action: RequestAction = serde_json::from_slice(&bytes)
                    .context("Failed to deserialize RequestAction")?;
                return Ok(action);
            }
        }

        // Fail closed, for the same reason the `Err` branch at the call site does: a request
        // this proxy forwards is a request the model was asked to rule on and did not. The
        // caller cannot tell the two apart from the action alone, so the distinction is in
        // the body and the log - `no_decision` here, backend failure there - exactly so a
        // silent model and a dead backend are never diagnosed as each other.
        Log::new(Some(&status_tx)).warn(format!(
            "Proxy blocking {} {}: handler produced no filtering decision (decision=no_decision)",
            request_info.method, request_info.url
        ));
        Ok(RequestAction::Block {
            status: 502,
            body: "netget proxy: the filtering handler returned no decision for this request, \
                   so it was refused"
                .to_string(),
        })
    }

    /// Consult LLM about an HTTPS connection (pass-through mode)
    async fn consult_llm_https_connection(
        conn_info: &HttpsConnectionInfo,
        server_id: ServerId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        protocol: &Arc<ProxyProtocol>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<HttpsConnectionAction> {
        Log::new(Some(&status_tx)).debug("Consulting LLM about HTTPS connection...");

        // Create HTTPS CONNECT event
        let event = Event::new(
            &PROXY_HTTPS_CONNECT_EVENT,
            json!({
                "destination_host": conn_info.destination_host,
                "destination_port": conn_info.destination_port,
                "sni": conn_info.sni.as_ref().unwrap_or(&String::new()),
            }),
        );

        let execution_result = call_llm(
            llm_client,
            app_state,
            server_id,
            None, // TODO: Add connection_id for proxy responses
            &event,
            protocol.as_ref() as &dyn Server,
        )
        .await
        .context("LLM request failed")?;

        // Extract HTTPS connection action from protocol results
        for result in execution_result.protocol_results {
            if let ActionResult::Output(bytes) = result {
                // Deserialize the HttpsConnectionAction
                let action: HttpsConnectionAction = serde_json::from_slice(&bytes)
                    .context("Failed to deserialize HttpsConnectionAction")?;
                return Ok(action);
            }
        }

        // Fail closed. An `Allow` here opens a bidirectional tunnel to an arbitrary host on
        // the strength of a decision nobody made, and the access log then records it as a
        // deliberate pass. See the sibling comment in `consult_llm_http_request`.
        Log::new(Some(&status_tx)).warn(format!(
            "Proxy blocking CONNECT {}:{}: handler produced no filtering decision \
             (decision=no_decision)",
            conn_info.destination_host, conn_info.destination_port
        ));
        Ok(HttpsConnectionAction::Block {
            reason: Some(
                "netget proxy: the filtering handler returned no decision for this connection, \
                 so it was refused"
                    .to_string(),
            ),
        })
    }

    /// Convert a proxy-style request (absolute-form request-target, plus
    /// proxy-only hop-by-hop headers) into what an origin server expects.
    ///
    /// Only the request line and the `Proxy-Connection` header are touched; the
    /// rest of the message, including the body, is passed through byte-for-byte.
    fn to_origin_form(request_data: &[u8]) -> Vec<u8> {
        // Split headers from body without decoding the body.
        let sep = b"\r\n\r\n";
        let headers_end = request_data
            .windows(sep.len())
            .position(|w| w == sep)
            .map(|p| p + sep.len())
            .unwrap_or(request_data.len());

        let (head, body) = request_data.split_at(headers_end);
        let Ok(head_str) = std::str::from_utf8(head) else {
            return request_data.to_vec(); // Not text: leave untouched
        };

        let mut out = String::with_capacity(head_str.len());
        for (i, line) in head_str.split("\r\n").enumerate() {
            if i == 0 {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    let target = parts[1];
                    let origin_form = strip_absolute_form(target);
                    out.push_str(&format!("{} {} {}", parts[0], origin_form, parts[2]));
                } else {
                    out.push_str(line);
                }
            } else if line
                .split_once(':')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("proxy-connection"))
            {
                // Hop-by-hop header that must not be forwarded upstream.
                continue;
            } else {
                out.push_str(line);
            }
            out.push_str("\r\n");
        }

        // The split above turns the trailing "\r\n\r\n" into two empty
        // segments, so the reassembled head already ends with a blank line.
        // Drop the one extra CRLF that the final empty segment added.
        if out.ends_with("\r\n\r\n\r\n") {
            out.truncate(out.len() - 2);
        }

        let mut result = out.into_bytes();
        result.extend_from_slice(body);
        result
    }

    /// Apply modifications to HTTP request
    pub(crate) fn apply_request_modifications(
        request_data: &[u8],
        modifications: &RequestAction,
    ) -> Result<Vec<u8>> {
        if let RequestAction::Modify {
            headers,
            remove_headers,
            new_path,
            query_params,
            new_body,
            body_replacements,
        } = modifications
        {
            // Find the \r\n\r\n separator between headers and body
            let separator = b"\r\n\r\n";
            let separator_pos = request_data
                .windows(separator.len())
                .position(|window| window == separator);

            if separator_pos.is_none() {
                return Ok(request_data.to_vec());
            }

            let headers_end = separator_pos.unwrap();
            let body_start = headers_end + 4; // After \r\n\r\n

            // Extract headers section as string
            let headers_bytes = &request_data[..headers_end];
            let headers_str = String::from_utf8_lossy(headers_bytes);
            let header_lines: Vec<&str> = headers_str.lines().collect();

            if header_lines.is_empty() {
                return Ok(request_data.to_vec());
            }

            // Rebuild the request line, applying new_path and query_params.
            let mut request_line = header_lines[0].to_string();
            let original_parts: Vec<&str> = header_lines[0].split_whitespace().collect();
            if (new_path.is_some() || query_params.is_some()) && original_parts.len() >= 3 {
                let method = original_parts[0];
                let version = original_parts[2];
                let target = new_path.as_deref().unwrap_or(original_parts[1]);
                let target = apply_query_params(target, query_params.as_ref());
                request_line = format!("{} {} {}", method, target, version);
            }

            // Build headers map.
            //
            // HTTP field names are case-insensitive (RFC 9110 §5.1) and hyper/reqwest put them
            // on the wire lowercased, while a model writes `User-Agent` / `Content-Type`. Keying
            // this map on the raw name therefore made `remove_headers: ["User-Agent"]` a no-op
            // and made `headers: {"Host": ...}` *append* a second Host header instead of
            // replacing the existing `host`. Key on the lowercased name and keep the original
            // spelling only for output. The MITM response path (`tls_mitm.rs`) already did this;
            // the plain-HTTP request path did not.
            let mut headers_map: HashMap<String, (String, String)> = HashMap::new();
            for line in &header_lines[1..] {
                if let Some(colon_pos) = line.find(':') {
                    let name = line[..colon_pos].trim().to_string();
                    let value = line[colon_pos + 1..].trim().to_string();
                    headers_map.insert(name.to_lowercase(), (name, value));
                }
            }

            // Remove headers
            if let Some(remove) = remove_headers {
                for header_name in remove {
                    headers_map.remove(&header_name.to_lowercase());
                }
            }

            // Add/modify headers
            if let Some(add_headers) = headers {
                for (name, value) in add_headers {
                    headers_map.insert(name.to_lowercase(), (name.clone(), value.clone()));
                }
            }

            // Get body as bytes, then convert to string for modification
            let original_body = if body_start < request_data.len() {
                &request_data[body_start..]
            } else {
                &[]
            };

            let mut body = String::from_utf8_lossy(original_body).to_string();

            // Apply body modifications
            if let Some(new_body_text) = new_body {
                body = new_body_text.clone();
            }

            if let Some(replacements) = body_replacements {
                for replacement in replacements {
                    match Regex::new(&replacement.pattern) {
                        Ok(re) => {
                            body = re
                                .replace_all(&body, replacement.replacement.as_str())
                                .to_string();
                        }
                        Err(e) => warn!(
                            "Invalid body_replacements pattern {:?}: {} (skipped)",
                            replacement.pattern, e
                        ),
                    }
                }
            }

            // Update Content-Length to match new body size
            if !body.is_empty() {
                headers_map.insert(
                    "content-length".to_string(),
                    ("Content-Length".to_string(), body.len().to_string()),
                );
            } else if new_body.is_some() || body_replacements.is_some() {
                // Body was explicitly modified to empty
                headers_map.insert(
                    "content-length".to_string(),
                    ("Content-Length".to_string(), "0".to_string()),
                );
            }

            // Reconstruct request with proper \r\n line endings
            let mut result = Vec::new();
            result.extend_from_slice(request_line.as_bytes());
            result.extend_from_slice(b"\r\n");

            for (_key, (name, value)) in headers_map {
                result.extend_from_slice(name.as_bytes());
                result.extend_from_slice(b": ");
                result.extend_from_slice(value.as_bytes());
                result.extend_from_slice(b"\r\n");
            }

            result.extend_from_slice(b"\r\n");
            if !body.is_empty() {
                result.extend_from_slice(body.as_bytes());
            }

            Ok(result)
        } else {
            Ok(request_data.to_vec())
        }
    }

    /// Generate a self-signed CA certificate for MITM proxy.
    ///
    /// A fresh key is generated per server start; there is no fixed key and
    /// nothing is persisted. The params are returned alongside so that leaf
    /// certificates can be issued under this CA's real distinguished name.
    fn generate_ca_certificate() -> Result<(Certificate, KeyPair, CertificateParams)> {
        let mut params = CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "NetGet MITM Proxy CA");

        let key_pair = KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;

        Ok((cert, key_pair, params))
    }
}

/// Reduce an absolute-form request-target ("http://host/path?q") to origin-form
/// ("/path?q"). Targets that are already origin-form, or the asterisk-form "*",
/// are returned unchanged.
pub(crate) fn strip_absolute_form(target: &str) -> &str {
    let rest = match target.split_once("://") {
        Some((scheme, rest)) if !scheme.is_empty() && !scheme.contains('/') => rest,
        _ => return target,
    };
    match rest.find('/') {
        Some(pos) => &rest[pos..],
        None => "/", // "http://host" with no path
    }
}

/// Merge `query_params` into a request target, replacing same-named parameters
/// and appending the rest. Returns the target unchanged when there is nothing to
/// merge.
pub(crate) fn apply_query_params(
    target: &str,
    query_params: Option<&HashMap<String, String>>,
) -> String {
    let Some(params) = query_params.filter(|p| !p.is_empty()) else {
        return target.to_string();
    };

    let (path, existing_query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    };

    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(query) = existing_query {
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            pairs.push((k.to_string(), v.to_string()));
        }
    }

    for (name, value) in params {
        if let Some(existing) = pairs.iter_mut().find(|(k, _)| k == name) {
            existing.1 = value.clone();
        } else {
            pairs.push((name.clone(), value.clone()));
        }
    }

    let query = pairs
        .into_iter()
        .map(|(k, v)| if v.is_empty() { k } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join("&");

    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

/// Truncate a string to at most `max_bytes`, never splitting a UTF-8 character.
///
/// `&s[..n]` panics when `n` lands inside a multi-byte character, which any
/// client can arrange by sending a request whose 200th byte is mid-character.
pub(crate) fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
