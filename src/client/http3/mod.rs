//! HTTP/3 client implementation using QUIC
pub mod actions;

pub use actions::Http3ClientProtocol;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::Request;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::client::http3::actions::HTTP3_CLIENT_RESPONSE_RECEIVED_EVENT;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::actions::protocol_trait::Protocol;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// How often the command loop re-checks that its client still exists. Each HTTP/3
/// request opens its own QUIC connection, so there is no long-lived socket to notice
/// a close on; this is what the old idle task was for.
const REMOVAL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// One completed HTTP/3 exchange.
///
/// Split out of [`Http3Client::make_request`] so the injected-command loop can await
/// the QUIC round-trip - and report a truthful outcome - without also awaiting the LLM
/// call the response event triggers.
pub struct Http3Exchange {
    pub status_code: u16,
    pub status_text: String,
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: String,
}

/// What one executed action did.
enum Applied {
    /// The action ran; `detail` says what it did.
    Executed(String),
    /// The action asked to end the session.
    Disconnect,
}

/// HTTP/3 client that makes requests to remote HTTP/3 servers over QUIC
pub struct Http3Client;

impl Http3Client {
    /// Connect to an HTTP/3 server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        info!(
            "HTTP/3 client {} initializing for {}",
            client_id, remote_addr
        );

        // Parse remote address
        let remote_sock_addr: SocketAddr = remote_addr
            .parse()
            .context("Invalid remote address format, expected host:port")?;

        // Store base URL and connection info in protocol_data
        let base_url = format!("https://{}", remote_addr);

        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field("base_url".to_string(), serde_json::json!(base_url));
                client
                    .set_protocol_field("remote_addr".to_string(), serde_json::json!(remote_addr));
                client.set_protocol_field("quic_initialized".to_string(), serde_json::json!(true));
            })
            .await;

        // Update status to connected
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;

        let _ = status_tx.send(format!(
            "[CLIENT] HTTP/3 client {} ready for {} (QUIC transport)",
            client_id, remote_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        info!("HTTP/3 client {} initialized successfully", client_id);

        // Injected commands (the dashboard's [ send ]). This client raises no
        // connected event, so there is no LLM call to register ahead of - but the
        // handle still has to exist before `connect()` returns, or the dashboard
        // greys out [ send ] on a client that is up.
        //
        // This task also replaces the old "poll get_client() every 5s" idle task -
        // that check is now one arm of the command loop's select!.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_state = app_state.clone();
        let cmd_llm = llm_client.clone();
        let cmd_tx = status_tx.clone();
        let cmd_task = tokio::spawn(async move {
            Self::command_loop(command_rx, client_id, cmd_state, cmd_llm, cmd_tx).await;
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Return the remote address
        Ok(remote_sock_addr)
    }

    /// Drain injected commands until the channel closes (client removed) or an
    /// injected `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client:
    /// there is no write half to hand it, and `send_http3_request` yields
    /// `ClientActionResult::Custom`. So the action goes through [`Self::apply_action`]
    /// and the outcome is recorded and replied the way the generic arm does it.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let protocol = Http3ClientProtocol::new();
        let mut removal_check = tokio::time::interval(REMOVAL_CHECK_INTERVAL);
        removal_check.tick().await; // the first tick completes immediately

        loop {
            tokio::select! {
                received = command_rx.recv() => {
                    let Some(command) = received else { break };
                    if Self::handle_command(
                        &protocol,
                        command,
                        client_id,
                        &app_state,
                        &llm_client,
                        &status_tx,
                    )
                    .await
                    {
                        break;
                    }
                }
                _ = removal_check.tick() => {
                    if app_state.get_client(client_id).await.is_none() {
                        info!("HTTP/3 client {} stopped", client_id);
                        break;
                    }
                }
            }
        }

        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Execute one injected action, record it, and reply. Returns `true` when the
    /// command loop should stop.
    async fn handle_command(
        protocol: &Http3ClientProtocol,
        command: ClientCommand,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> bool {
        let action = command.action.clone();
        let outcome = match protocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(action_result) => {
                match Self::apply_action(action_result, client_id, app_state, llm_client, status_tx)
                    .await
                {
                    // Never `Sent`: h3/quinn own the datagrams and report no wire byte
                    // count for the request, so a number here would be invented.
                    // `Executed` carries the response status instead.
                    Ok(Applied::Executed(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                    Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                    Err(e) => Err(e),
                }
            }
        };

        let outcome_json = match &outcome {
            Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({"error": e.to_string()}),
        };
        app_state
            .record_access_log(
                AccessLogOwner::Client(client_id.as_u32()),
                protocol.protocol_name(),
                None,
                "injected_action",
                action,
                vec![outcome_json],
            )
            .await;

        let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
        if let Err(e) = &outcome {
            error!("HTTP/3 client {} injected action failed: {}", client_id, e);
            let _ = status_tx.send(format!(
                "[WARN] Client {} injected action failed: {}",
                client_id, e
            ));
        }
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        crate::client::command_support::reply(command, outcome);

        if disconnect {
            app_state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
        }
        disconnect
    }

    /// Turn one executed action into an HTTP/3 request (or a session end).
    ///
    /// The exchange is awaited so the caller can report what the server actually
    /// answered; the response event is delivered from its own registered task, so a
    /// parked manual handler cannot wedge the command loop for a human's think time.
    async fn apply_action(
        action_result: ClientActionResult,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        match action_result {
            ClientActionResult::Custom { name, data } if name == "http3_request" => {
                let method = data["method"].as_str().unwrap_or("GET").to_string();
                let path = data["path"].as_str().unwrap_or("/").to_string();
                let headers = data["headers"].as_object().cloned();
                let body = data["body"].as_str().map(|s| s.to_string());
                let priority = data["priority"].as_u64().map(|p| p as u8);

                let exchange = Self::perform_request(
                    client_id,
                    method.clone(),
                    path.clone(),
                    headers,
                    body,
                    priority,
                    app_state,
                )
                .await?;
                let detail = format!(
                    "http3_request {} {} -> {} ({} byte body)",
                    method,
                    path,
                    exchange.status_code,
                    exchange.body.len()
                );

                let llm_clone = llm_client.clone();
                let state_clone = app_state.clone();
                let status_clone = status_tx.clone();
                let notify_handle = tokio::spawn(async move {
                    Http3Client::notify_response(
                        client_id,
                        exchange,
                        state_clone,
                        llm_clone,
                        status_clone,
                    )
                    .await;
                });
                app_state
                    .register_client_task(client_id, notify_handle)
                    .await;
                Ok(Applied::Executed(detail))
            }
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            ClientActionResult::WaitForMore => Ok(Applied::Executed("wait_for_more".to_string())),
            ClientActionResult::NoAction => Ok(Applied::Executed("no_action".to_string())),
            // Not swallowed: an action this client cannot carry out says so, rather
            // than looking identical to success.
            ClientActionResult::Custom { name, .. } => Ok(Applied::Executed(format!(
                "custom result '{name}' is not handled by the HTTP/3 client"
            ))),
            ClientActionResult::SendData(_) => Ok(Applied::Executed(
                "send_data has no meaning for a request/response HTTP/3 client".to_string(),
            )),
            ClientActionResult::Multiple(_) => Ok(Applied::Executed(
                "multiple results are not produced by the HTTP/3 client".to_string(),
            )),
        }
    }

    /// Make an HTTP/3 request over QUIC and hand the response to the LLM.
    #[allow(clippy::too_many_arguments)]
    pub async fn make_request(
        client_id: ClientId,
        method: String,
        path: String,
        headers: Option<serde_json::Map<String, serde_json::Value>>,
        body: Option<String>,
        priority: Option<u8>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let exchange =
            Self::perform_request(client_id, method, path, headers, body, priority, &app_state)
                .await?;
        Self::notify_response(client_id, exchange, app_state, llm_client, status_tx).await;
        Ok(())
    }

    /// Perform the QUIC round-trip only. No LLM involvement, so a caller can await
    /// this and know exactly what the server answered.
    ///
    /// The QUIC connection is closed before returning rather than after the LLM call,
    /// so a slow model does not hold an idle connection open.
    #[allow(clippy::too_many_arguments)]
    pub async fn perform_request(
        client_id: ClientId,
        method: String,
        path: String,
        headers: Option<serde_json::Map<String, serde_json::Value>>,
        body: Option<String>,
        priority: Option<u8>,
        app_state: &AppState,
    ) -> Result<Http3Exchange> {
        // Get connection info from client
        let (base_url, remote_addr) = app_state
            .with_client_mut(client_id, |client| {
                let base_url = client
                    .get_protocol_field("base_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let remote_addr = client
                    .get_protocol_field("remote_addr")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (base_url, remote_addr)
            })
            .await
            .context("Client not found")?;

        let base_url = base_url.context("No base URL found")?;
        let remote_addr_str = remote_addr.context("No remote address found")?;
        let remote_sock_addr: SocketAddr = remote_addr_str.parse()?;

        // Build full URL
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else {
            format!("{}{}", base_url, path)
        };

        info!(
            "HTTP/3 client {} making request: {} {} (priority: {:?})",
            client_id, method, url, priority
        );

        // Create QUIC endpoint
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;

        // Install a rustls CryptoProvider before building the config, or
        // `ClientConfig::builder()` panics instead of erroring. See the fuller note in
        // `src/client/tls/mod.rs`.
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Configure TLS (accept invalid certs for now - can be made configurable)
        let mut rustls_client_config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
            .with_no_client_auth();

        // Set ALPN to h3
        rustls_client_config.alpn_protocols = vec![b"h3".to_vec()];

        // Convert to quinn client config
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(rustls_client_config)?,
        ));

        endpoint.set_default_client_config(client_config);

        // Extract host from URL for SNI
        let url_parsed = url::Url::parse(&url)?;
        let host = url_parsed.host_str().context("No host in URL")?;

        info!(
            "HTTP/3 client {} connecting to {} via QUIC",
            client_id, remote_sock_addr
        );

        // Connect via QUIC
        let connection = endpoint
            .connect(remote_sock_addr, host)
            .context("Failed to create QUIC connection")?
            .await
            .context("Failed to establish QUIC connection")?;

        info!("HTTP/3 client {} established QUIC connection", client_id);

        // Create H3 connection
        let quinn_connection = h3_quinn::Connection::new(connection);
        let (mut h3_conn, mut send_request) = h3::client::new(quinn_connection)
            .await
            .context("Failed to create HTTP/3 connection")?;

        info!("HTTP/3 client {} created HTTP/3 session", client_id);

        // Build HTTP request
        let mut req_builder = Request::builder().uri(&url).method(method.as_str());

        // Add headers
        if let Some(hdrs) = headers {
            for (key, value) in hdrs {
                if let Some(val_str) = value.as_str() {
                    req_builder = req_builder.header(&key, val_str);
                }
            }
        }

        // Build request body
        let req_body = body.unwrap_or_default();
        let request = req_builder.body(()).context("Failed to build request")?;

        // Send request
        let mut stream = send_request
            .send_request(request)
            .await
            .context("Failed to send HTTP/3 request")?;

        // Send body if present
        if !req_body.is_empty() {
            stream
                .send_data(Bytes::from(req_body))
                .await
                .context("Failed to send request body")?;
        }

        stream
            .finish()
            .await
            .context("Failed to finish sending request")?;

        info!(
            "HTTP/3 client {} sent request, waiting for response",
            client_id
        );

        // Receive response
        let response = stream
            .recv_response()
            .await
            .context("Failed to receive response")?;

        let status = response.status();
        let status_code = status.as_u16();

        // Get headers
        let mut resp_headers = serde_json::Map::new();
        for (name, value) in response.headers() {
            if let Ok(val_str) = value.to_str() {
                resp_headers.insert(name.to_string(), serde_json::json!(val_str));
            }
        }

        // Read response body
        let mut body_bytes = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await? {
            use bytes::Buf;
            body_bytes.extend_from_slice(chunk.chunk());
            chunk.advance(chunk.remaining());
        }
        let body_text = String::from_utf8_lossy(&body_bytes).to_string();

        info!(
            "HTTP/3 client {} received response: {} ({})",
            client_id, status_code, status
        );

        // Close connection gracefully
        h3_conn.shutdown(0).await?;
        endpoint.close(0u32.into(), b"done");

        Ok(Http3Exchange {
            status_code,
            status_text: status.to_string(),
            headers: resp_headers,
            body: body_text,
        })
    }

    /// Hand a completed exchange to the LLM as an `http3_response_received` event.
    async fn notify_response(
        client_id: ClientId,
        exchange: Http3Exchange,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let protocol = Arc::new(crate::client::http3::actions::Http3ClientProtocol::new());
        let event = Event::new(
            &HTTP3_CLIENT_RESPONSE_RECEIVED_EVENT,
            serde_json::json!({
                "status_code": exchange.status_code,
                "status_text": exchange.status_text,
                "headers": exchange.headers,
                "body": exchange.body,
                "stream_id": 0u64, // TODO: Get actual stream ID if available
            }),
        );

        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        match call_llm_for_client(
            &llm_client,
            &app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            protocol.as_ref(),
            &status_tx,
        )
        .await
        {
            Ok(ClientLlmResult {
                actions,
                memory_updates,
            }) => {
                // Update memory
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }

                // Execute what the model asked for. These were discarded, so a model that
                // read a response and wanted to follow it with another request was
                // silently ignored -- the entire purpose of raising the event.
                //
                // They run through `perform_request`, which raises no event: that bounds
                // the loop, and avoids making notify -> apply -> make -> notify a
                // self-referential async chain that rustc cannot prove `Send`.
                use crate::llm::actions::client_trait::{Client, ClientActionResult};
                for action in actions {
                    let Ok(ClientActionResult::Custom { name, data }) =
                        protocol.execute_action(action.clone())
                    else {
                        continue;
                    };
                    if name != "http3_request" {
                        continue;
                    }
                    let result = Self::perform_request(
                        client_id,
                        data["method"].as_str().unwrap_or("GET").to_string(),
                        data["path"].as_str().unwrap_or("/").to_string(),
                        data["headers"].as_object().cloned(),
                        data["body"].as_str().map(|s| s.to_string()),
                        data["priority"].as_u64().map(|p| p as u8),
                        &app_state,
                    )
                    .await;
                    if let Err(e) = result {
                        error!(
                            "HTTP/3 client {} follow-up request failed: {}",
                            client_id, e
                        );
                    }
                }
            }
            Err(e) => {
                error!("LLM error for HTTP/3 client {}: {}", client_id, e);
            }
        }
    }
}

/// Skip server certificate verification (for testing)
/// TODO: Make this configurable
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer,
        _intermediates: &[rustls::pki_types::CertificateDer],
        _server_name: &rustls::pki_types::ServerName,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
