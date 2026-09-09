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

/// Wall-clock bound on one complete HTTP/3 exchange: QUIC handshake, h3 session setup,
/// request, and response.
///
/// There was no bound at all. `quinn` gives up on a peer that stops acknowledging, but a
/// server that completes the handshake and then simply never answers — trivially arranged,
/// and what a hung or wedged backend looks like — left `recv_response()` pending forever.
/// The task is registered, so `remove_client` could still abort it, but the dashboard's
/// `[ send ]` had no way to fail: `send_to_client` waited on an exchange that would never
/// resolve. 30 s matches the HTTP/1.1 and HTTP/2 clients' reqwest timeout.
const HTTP3_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a response body is read before the exchange is abandoned.
///
/// The body is buffered whole and handed to the model, so an unbounded one is memory
/// exhaustion with no upside — a model cannot read 8 MiB either. Same value and same
/// reasoning as the HTTP servers' `MAX_REQUEST_BODY_BYTES`.
const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How many exchanges deep this client keeps following the model's answers. Each response
/// can ask for another request, which produces another response; without a bound a model
/// that answers every response with a request never stops.
const MAX_FOLLOWUP_DEPTH: u8 = 4;

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
    /// The index of the QUIC stream this exchange used, from `h3`'s `RequestStream::id`.
    /// Only the index is public in `h3`; the wire stream id of a client bidirectional
    /// stream is `index << 2`.
    pub stream_index: u64,
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
        startup_params: Option<crate::protocol::StartupParams>,
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

        // `default_headers` is declared as "headers included in all requests" and nothing
        // read it, so setting it changed nothing. Stored here, merged in `perform_request`.
        let default_headers = match &startup_params {
            Some(params) => params.get_optional_object("default_headers")?.cloned(),
            None => None,
        };

        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field("base_url".to_string(), serde_json::json!(base_url));
                client
                    .set_protocol_field("remote_addr".to_string(), serde_json::json!(remote_addr));
                client.set_protocol_field("quic_initialized".to_string(), serde_json::json!(true));
                if let Some(headers) = &default_headers {
                    client.set_protocol_field(
                        "default_headers".to_string(),
                        serde_json::Value::Object(headers.clone()),
                    );
                }
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

        // Injected commands (the dashboard's [ send ]). Registered before the connected
        // event below, and before `connect()` returns: a dashboard-created client defaults
        // to a `*` manual rule, so that LLM call can park for minutes waiting for a human
        // and [ send ] has to work for the whole park. Without the handle the dashboard
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

        // Raise the connected event.
        //
        // It was declared and nothing ever raised it, and nothing else called this
        // client's request code either -- so a HTTP/3 client initialised itself and
        // then did nothing for the rest of its life. The command channel was, until now,
        // the only thing that could reach the wire at all.
        //
        // Raised from a registered task rather than inline: a dashboard-created client
        // defaults to a `*` -> manual rule, and awaiting a parked answer here would block
        // client creation itself. The exchange it produces IS reported back to the model
        // (it used to be dropped), bounded by MAX_FOLLOWUP_DEPTH so a connect instruction
        // cannot start an unbounded chain.
        let conn_state = app_state.clone();
        let conn_llm = llm_client.clone();
        let status_tx_c = status_tx.clone();
        let conn_task = tokio::spawn(async move {
            let Some(instruction) = conn_state.get_instruction_for_client(client_id).await else {
                return;
            };
            let protocol = crate::client::http3::actions::Http3ClientProtocol::new();
            let event = Event::new(
                &crate::client::http3::actions::HTTP3_CLIENT_CONNECTED_EVENT,
                serde_json::json!({ "base_url": base_url.clone() }),
            );
            match crate::client::llm_budget::call_llm_for_client(
                &conn_llm,
                &conn_state,
                client_id.to_string(),
                &instruction,
                "",
                Some(&event),
                &protocol,
                &status_tx_c,
            )
            .await
            {
                Ok(result) => {
                    if let Some(mem) = result.memory_updates {
                        conn_state.set_memory_for_client(client_id, mem).await;
                    }
                    use crate::llm::actions::client_trait::{Client, ClientActionResult};
                    for action in result.actions {
                        let Ok(ClientActionResult::Custom { name, data }) =
                            protocol.execute_action(action.clone())
                        else {
                            continue;
                        };
                        if name != "http3_request" {
                            continue;
                        }
                        match Self::perform_request(
                            client_id,
                            data["method"].as_str().unwrap_or("GET").to_string(),
                            data["path"].as_str().unwrap_or("/").to_string(),
                            data["headers"].as_object().cloned(),
                            data["body"].as_str().map(|s| s.to_string()),
                            data["priority"].as_u64().map(|p| p as u8),
                            &conn_state,
                        )
                        .await
                        {
                            // The exchange used to be dropped here, so the very first
                            // request — the one the connect instruction asks for — never
                            // raised `http3_response_received`. The model was told to make
                            // a request and then never told what came back. (The HTTP/2
                            // client had the same defect and it was fixed there first.)
                            Ok(exchange) => {
                                Self::notify_response(
                                    client_id,
                                    exchange,
                                    conn_state.clone(),
                                    conn_llm.clone(),
                                    status_tx_c.clone(),
                                    0,
                                )
                                .await;
                            }
                            Err(e) => error!(
                                "HTTP/3 client {} connect-time request failed: {}",
                                client_id, e
                            ),
                        }
                    }
                }
                Err(e) => error!(
                    "HTTP/3 client {} LLM error on connected event: {}",
                    client_id, e
                ),
            }
        });
        app_state.register_client_task(client_id, conn_task).await;

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
                        0,
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
        Self::notify_response(client_id, exchange, app_state, llm_client, status_tx, 0).await;
        Ok(())
    }

    /// Perform the QUIC round-trip only. No LLM involvement, so a caller can await
    /// this and know exactly what the server answered.
    ///
    /// The QUIC connection is closed before returning rather than after the LLM call,
    /// so a slow model does not hold an idle connection open.
    ///
    /// Bounded by [`HTTP3_REQUEST_TIMEOUT`]. Every step below can block indefinitely
    /// against a server that completes the handshake and then goes quiet, and none of them
    /// carried a deadline of its own; the endpoint is dropped on timeout, which closes the
    /// connection.
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
        match tokio::time::timeout(
            HTTP3_REQUEST_TIMEOUT,
            Self::perform_request_inner(
                client_id, method, path, headers, body, priority, app_state,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "HTTP/3 exchange did not complete within {}s",
                HTTP3_REQUEST_TIMEOUT.as_secs()
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn perform_request_inner(
        client_id: ClientId,
        method: String,
        path: String,
        headers: Option<serde_json::Map<String, serde_json::Value>>,
        body: Option<String>,
        priority: Option<u8>,
        app_state: &AppState,
    ) -> Result<Http3Exchange> {
        // Get connection info from client
        let (base_url, remote_addr, default_headers) = app_state
            .with_client_mut(client_id, |client| {
                let base_url = client
                    .get_protocol_field("base_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let remote_addr = client
                    .get_protocol_field("remote_addr")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let default_headers = client
                    .get_protocol_field("default_headers")
                    .and_then(|v| v.as_object().cloned());
                (base_url, remote_addr, default_headers)
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

        // Startup defaults merged *underneath* the request's own headers, keyed by the
        // lowercased name (HTTP/3 puts header names on the wire lowercased anyway). Merged
        // before anything is applied because `http::request::Builder::header` appends -
        // applying both sets in turn would send the same header twice.
        let mut merged = serde_json::Map::new();
        for (key, value) in default_headers.unwrap_or_default() {
            merged.insert(key.to_ascii_lowercase(), value);
        }
        if let Some(hdrs) = headers {
            for (key, value) in hdrs {
                merged.insert(key.to_ascii_lowercase(), value);
            }
        }
        for (key, value) in merged {
            if let Some(val_str) = value.as_str() {
                req_builder = req_builder.header(&key, val_str);
            }
        }

        // `priority` was threaded from the action through four call sites and read only by
        // the `info!` above — it changed nothing on the wire, exactly like the `enable_0rtt`
        // startup param that was removed for the same reason (see `actions.rs`). It is now
        // sent as the RFC 9218 `priority` header field, which is how HTTP/3 actually
        // expresses this: a request header, urgency `u=0..7`, that the *server* uses when
        // scheduling. Applied after the merge so it wins over a hand-written `priority`
        // header, and skipped entirely when the model does not ask for one, since RFC 9218
        // has a default (u=3) and sending it explicitly is not the same as leaving it out.
        //
        // Note the direction: RFC 9218 urgency is **lowest-is-most-urgent**. The action
        // description used to say "higher is more urgent", which is backwards, so a model
        // asking for high priority would have de-prioritised its request.
        if let Some(urgency) = priority {
            let urgency = urgency.min(7);
            match http::header::HeaderValue::from_str(&format!("u={urgency}")) {
                Ok(value) => {
                    req_builder = req_builder.header("priority", value);
                }
                Err(e) => tracing::warn!("HTTP/3 client {client_id}: bad priority header: {e}"),
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

        // The stream this request went out on. `http3_response_received` used to report a
        // hardcoded `0` under a `// TODO: Get actual stream ID` — a field the model was
        // told was the stream id and could never use to tell two responses apart. `h3`
        // does expose it; only the *index* is public (the raw id is `index << 2` for a
        // client bidirectional stream), so that is what is reported and what the event
        // parameter now describes.
        let stream_index = stream.id().index();

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

        // Read response body, bounded. It is buffered whole and handed to the model, so a
        // server streaming without end would otherwise grow this Vec until the process
        // died — and a model cannot read 8 MiB anyway.
        let mut body_bytes = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await? {
            use bytes::Buf;
            if body_bytes.len() + chunk.remaining() > MAX_RESPONSE_BODY_BYTES {
                return Err(anyhow::anyhow!(
                    "HTTP/3 response body exceeded {} bytes",
                    MAX_RESPONSE_BODY_BYTES
                ));
            }
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
            stream_index,
        })
    }

    /// Hand a completed exchange to the LLM as an `http3_response_received` event.
    ///
    /// Boxed with an explicit `+ Send` because the chain is genuinely self-referential
    /// (report → request → report) and the future is awaited inside a `tokio::spawn`,
    /// which inference will not give `Send` on its own. Root `CLAUDE.md` prescribes
    /// exactly this for the "client asks the model and throws the answer away" family:
    /// **a depth bound, not silence.**
    fn notify_response(
        client_id: ClientId,
        exchange: Http3Exchange,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
        depth: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
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
                    "stream_id": exchange.stream_index,
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

                    // Execute what the model asked for, and report each result back to it
                    // until the depth bound. Reporting used to stop after one hop: the
                    // follow-up ran but its response raised no event, so a model that read
                    // a 302 and asked for the redirect target never learned what it got.
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
                        match Self::perform_request(
                            client_id,
                            data["method"].as_str().unwrap_or("GET").to_string(),
                            data["path"].as_str().unwrap_or("/").to_string(),
                            data["headers"].as_object().cloned(),
                            data["body"].as_str().map(|s| s.to_string()),
                            data["priority"].as_u64().map(|p| p as u8),
                            &app_state,
                        )
                        .await
                        {
                            Ok(exchange) => {
                                if depth + 1 < MAX_FOLLOWUP_DEPTH {
                                    Self::notify_response(
                                        client_id,
                                        exchange,
                                        app_state.clone(),
                                        llm_client.clone(),
                                        status_tx.clone(),
                                        depth + 1,
                                    )
                                    .await;
                                } else {
                                    tracing::warn!(
                                        "HTTP/3 client {} reached the follow-up depth limit \
                                         ({}); not reporting the response to the model",
                                        client_id,
                                        MAX_FOLLOWUP_DEPTH
                                    );
                                }
                            }
                            Err(e) => error!(
                                "HTTP/3 client {} follow-up request failed: {}",
                                client_id, e
                            ),
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error for HTTP/3 client {}: {}", client_id, e);
                }
            }
        })
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
