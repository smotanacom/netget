//! HTTP/2 client implementation
pub mod actions;

pub use actions::Http2ClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::client::http2::actions::HTTP2_CLIENT_RESPONSE_RECEIVED_EVENT;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::actions::protocol_trait::Protocol;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// How often the command loop re-checks that its client still exists. HTTP/2 here is
/// request/response over reqwest with no read loop, so this is what the old idle task
/// was for.
const REMOVAL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// One completed HTTP/2 exchange.
///
/// Split out of [`Http2Client::make_request`] so the injected-command loop can await
/// the network round-trip - and report a truthful outcome - without also awaiting the
/// LLM call the response event triggers.
pub struct Http2Exchange {
    pub status_code: u16,
    pub status_text: String,
    pub http_version: String,
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

/// HTTP/2 client that makes requests to remote HTTP/2 servers
pub struct Http2Client;

impl Http2Client {
    /// Connect to an HTTP/2 server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // For HTTP/2, "connection" is logical, with persistent multiplexed streams
        // We'll create an HTTP/2 client and store it in protocol_data

        info!(
            "HTTP/2 client {} initialized for {}",
            client_id, remote_addr
        );

        // Build reqwest client with HTTP/2 enabled
        let _http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .http2_prior_knowledge() // Force HTTP/2 (without ALPN negotiation)
            .build()
            .context("Failed to build HTTP/2 client")?;

        // Store client in protocol_data
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field(
                    "http2_client".to_string(),
                    serde_json::json!("initialized"),
                );
                client.set_protocol_field("base_url".to_string(), serde_json::json!(remote_addr));
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] HTTP/2 client {} ready for {}",
            client_id, remote_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

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

        // Return a dummy local address (HTTP/2 is connectionless)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Drain injected commands until the channel closes (client removed) or an
    /// injected `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client:
    /// there is no write half, and `send_http2_request` yields
    /// `ClientActionResult::Custom`. So the action goes through [`Self::apply_action`]
    /// and the outcome is recorded and replied the way the generic arm does it.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let protocol = Http2ClientProtocol::new();
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
                        info!("HTTP/2 client {} stopped", client_id);
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
        protocol: &Http2ClientProtocol,
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
                    // Never `Sent`: reqwest owns the socket and does not report how
                    // many bytes the request serialised to, so a byte count here would
                    // be invented. `Executed` carries the response status instead.
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
            error!("HTTP/2 client {} injected action failed: {}", client_id, e);
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

    /// Turn one executed action into an HTTP/2 request (or a session end).
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
            ClientActionResult::Custom { name, data } if name == "http2_request" => {
                let method = data["method"].as_str().unwrap_or("GET").to_string();
                let path = data["path"].as_str().unwrap_or("/").to_string();
                let headers = data["headers"].as_object().cloned();
                let body = data["body"].as_str().map(|s| s.to_string());

                let exchange = Self::perform_request(
                    client_id,
                    method.clone(),
                    path.clone(),
                    headers,
                    body,
                    app_state,
                    status_tx,
                )
                .await?;
                let detail = format!(
                    "http2_request {} {} -> {} ({} byte body)",
                    method,
                    path,
                    exchange.status_code,
                    exchange.body.len()
                );

                let llm_clone = llm_client.clone();
                let state_clone = app_state.clone();
                let status_clone = status_tx.clone();
                let notify_handle = tokio::spawn(async move {
                    Http2Client::notify_response(
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
                "custom result '{name}' is not handled by the HTTP/2 client"
            ))),
            ClientActionResult::SendData(_) => Ok(Applied::Executed(
                "send_data has no meaning for a request/response HTTP/2 client".to_string(),
            )),
            ClientActionResult::Multiple(_) => Ok(Applied::Executed(
                "multiple results are not produced by the HTTP/2 client".to_string(),
            )),
        }
    }

    /// Make an HTTP/2 request and hand the response to the LLM.
    #[allow(clippy::too_many_arguments)]
    pub async fn make_request(
        client_id: ClientId,
        method: String,
        path: String,
        headers: Option<serde_json::Map<String, serde_json::Value>>,
        body: Option<String>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let exchange = Self::perform_request(
            client_id, method, path, headers, body, &app_state, &status_tx,
        )
        .await?;
        Self::notify_response(client_id, exchange, app_state, llm_client, status_tx).await;
        Ok(())
    }

    /// Perform the HTTP/2 round-trip only. No LLM involvement, so a caller can await
    /// this and know exactly what the server answered.
    #[allow(clippy::too_many_arguments)]
    pub async fn perform_request(
        client_id: ClientId,
        method: String,
        path: String,
        headers: Option<serde_json::Map<String, serde_json::Value>>,
        body: Option<String>,
        app_state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Http2Exchange> {
        // Get base URL from client
        let base_url = app_state
            .with_client_mut(client_id, |client| {
                client
                    .get_protocol_field("base_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .await
            .flatten()
            .context("No base URL found")?;

        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else if base_url.starts_with("http://") || base_url.starts_with("https://") {
            format!("{}{}", base_url, path)
        } else {
            // `base_url` is whatever the client was opened on, often a bare host:port.
            // reqwest needs an absolute URL; `http2_prior_knowledge()` speaks cleartext
            // h2c, so http:// is the right scheme for that case.
            format!("http://{}{}", base_url, path)
        };

        info!(
            "HTTP/2 client {} making request: {} {}",
            client_id, method, url
        );

        // Build request with HTTP/2 enabled
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .http2_prior_knowledge() // Force HTTP/2
            .build()?;

        let mut request = match method.to_uppercase().as_str() {
            "GET" => http_client.get(&url),
            "POST" => http_client.post(&url),
            "PUT" => http_client.put(&url),
            "DELETE" => http_client.delete(&url),
            "HEAD" => http_client.head(&url),
            "PATCH" => http_client.patch(&url),
            _ => return Err(anyhow::anyhow!("Unsupported HTTP method: {}", method)),
        };

        // Add headers
        if let Some(hdrs) = headers {
            for (key, value) in hdrs {
                if let Some(val_str) = value.as_str() {
                    request = request.header(&key, val_str);
                }
            }
        }

        // Add body
        if let Some(body_str) = body {
            request = request.body(body_str);
        }

        // Make request
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let status_code = status.as_u16();
                let version = response.version();

                // Get headers
                let mut resp_headers = serde_json::Map::new();
                for (name, value) in response.headers() {
                    if let Ok(val_str) = value.to_str() {
                        resp_headers.insert(name.to_string(), serde_json::json!(val_str));
                    }
                }

                // Get body
                let body_text = response.text().await.unwrap_or_default();

                info!(
                    "HTTP/2 client {} received response: {} ({}) version: {:?}",
                    client_id, status_code, status, version
                );

                Ok(Http2Exchange {
                    status_code,
                    status_text: status.to_string(),
                    http_version: format!("{:?}", version),
                    headers: resp_headers,
                    body: body_text,
                })
            }
            Err(e) => {
                Log::new(Some(status_tx))
                    .error(format!("HTTP/2 client {} request failed: {}", client_id, e));
                Err(e.into())
            }
        }
    }

    /// Hand a completed exchange to the LLM as an `http2_response_received` event.
    async fn notify_response(
        client_id: ClientId,
        exchange: Http2Exchange,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let protocol = Arc::new(crate::client::http2::actions::Http2ClientProtocol::new());
        let event = Event::new(
            &HTTP2_CLIENT_RESPONSE_RECEIVED_EVENT,
            serde_json::json!({
                "status_code": exchange.status_code,
                "status_text": exchange.status_text,
                "http_version": exchange.http_version,
                "headers": exchange.headers,
                "body": exchange.body,
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
                    if name != "http2_request" {
                        continue;
                    }
                    let result = Self::perform_request(
                        client_id,
                        data["method"].as_str().unwrap_or("GET").to_string(),
                        data["path"].as_str().unwrap_or("/").to_string(),
                        data["headers"].as_object().cloned(),
                        data["body"].as_str().map(|s| s.to_string()),
                        &app_state,
                        &status_tx,
                    )
                    .await;
                    if let Err(e) = result {
                        error!(
                            "HTTP/2 client {} follow-up request failed: {}",
                            client_id, e
                        );
                    }
                }
            }
            Err(e) => {
                error!("LLM error for HTTP/2 client {}: {}", client_id, e);
            }
        }
    }
}
