//! HTTP client implementation
pub mod actions;

pub use actions::HttpClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::client::http::actions::{
    HTTP_CLIENT_CONNECTED_EVENT, HTTP_CLIENT_RESPONSE_RECEIVED_EVENT,
};
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

/// How often the command loop re-checks that its client still exists. HTTP has no
/// socket to notice a close on, so this is what the old idle task was for.
const REMOVAL_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// One completed HTTP exchange.
///
/// Split out of [`HttpClient::make_request`] so the injected-command loop can await
/// the network round-trip - and report a truthful outcome - without also awaiting the
/// LLM call the response event triggers.
pub struct HttpExchange {
    pub status_code: u16,
    pub status_text: String,
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: String,
}

/// What one executed action did. Shared vocabulary between the connected-event
/// handler and the injected-command loop.
enum Applied {
    /// The action ran; `detail` says what it did.
    Executed(String),
    /// The action asked to end the session.
    Disconnect,
}

/// How an `http_request` is issued.
#[derive(Clone, Copy)]
enum Dispatch {
    /// Spawn the request and return immediately. Used by the connected-event
    /// handler, which runs inline in `connect()` and must not block client creation
    /// on a request that can take the full 30s timeout.
    Spawn,
    /// Await the HTTP exchange so the caller can report what actually happened. Used
    /// by the injected-command loop. The response event is still delivered to the
    /// LLM, from its own registered task, so a parked manual handler cannot wedge the
    /// command loop for the length of a human's think time.
    Await,
}

/// HTTP client that makes requests to remote HTTP servers
pub struct HttpClient;

impl HttpClient {
    /// The process-wide HTTP client, built once.
    ///
    /// Two things this avoids, both of which cost real time on every request before:
    /// `reqwest::Client::builder().build()` sets up the rustls stack and loads the
    /// platform root store -- on macOS that reads the system keychain through
    /// Security.framework, which is synchronous and serialises across processes -- so it
    /// runs on `spawn_blocking` rather than parking a tokio worker. And it is kept, so
    /// requests after the first reuse the connection pool instead of paying for a fresh
    /// TLS stack and handshake each time.
    ///
    /// Protocol versions are negotiated via ALPN during the handshake, so one client
    /// serves both HTTP/1.1 and HTTP/2.
    async fn http_client() -> Result<reqwest::Client> {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        if let Some(client) = CLIENT.get() {
            return Ok(client.clone());
        }
        let built = tokio::task::spawn_blocking(|| {
            reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .use_rustls_tls()
                .build()
                .context("Failed to build HTTP client")
        })
        .await
        .context("HTTP client build task panicked")??;
        Ok(CLIENT.get_or_init(|| built).clone())
    }

    /// Connect to an HTTP server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // For HTTP, "connection" is logical, not a persistent TCP connection
        // We'll create an HTTP client and store it in protocol_data

        info!("HTTP client {} initialized for {}", client_id, remote_addr);

        // Warm the shared client here rather than building one and dropping it. This
        // used to bind to `_http_client`: a full rustls stack constructed at connect time
        // and immediately discarded, while every request built another one.
        Self::http_client().await?;

        // Store client in protocol_data
        // Ensure base_url has http:// scheme
        let base_url = if remote_addr.starts_with("http://") || remote_addr.starts_with("https://")
        {
            remote_addr.clone()
        } else {
            format!("http://{}", remote_addr)
        };

        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field(
                    "http_client".to_string(),
                    serde_json::json!("initialized"),
                );
                client.set_protocol_field("base_url".to_string(), serde_json::json!(base_url));
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] HTTP client {} ready for {}",
            client_id, remote_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Injected commands (the dashboard's [ send ]). Registered BEFORE the
        // connected-event LLM call below: a dashboard-created client defaults to a
        // `*` manual rule, so that call can park for minutes waiting for a human and
        // [ send ] has to work for the whole park.
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

        // Call LLM with http_connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &HTTP_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "base_url": base_url.clone(),
                }),
            );

            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &String::new(), // No memory yet for initial connection
                Some(&event),
                &crate::client::http::actions::HttpClientProtocol,
                &status_tx,
            )
            .await
            {
                Ok(result) => {
                    // Execute actions from LLM response
                    let protocol = crate::client::http::actions::HttpClientProtocol;

                    for action in result.actions {
                        match protocol.execute_action(action.clone()) {
                            Ok(action_result) => {
                                match Self::apply_action(
                                    action_result,
                                    Dispatch::Spawn,
                                    client_id,
                                    &app_state,
                                    &llm_client,
                                    &status_tx,
                                )
                                .await
                                {
                                    Ok(Applied::Disconnect) => {
                                        info!("LLM requested disconnect after connect");
                                        // Drop the command handle so the dashboard stops
                                        // offering [ send ]; that also ends the command loop.
                                        app_state.remove_client_handle(client_id).await;
                                        app_state
                                            .update_client_status(
                                                client_id,
                                                ClientStatus::Disconnected,
                                            )
                                            .await;
                                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                                        return Ok("0.0.0.0:0".parse().unwrap());
                                    }
                                    Ok(Applied::Executed(detail)) => {
                                        debug!(
                                            "HTTP client {} after connect: {}",
                                            client_id, detail
                                        );
                                    }
                                    Err(e) => {
                                        error!(
                                            "HTTP client {} request after connect failed: {}",
                                            client_id, e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Action execution error: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error on http_connected event: {}", e);
                }
            }
        }

        // Return a dummy local address (HTTP is connectionless)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Drain injected commands until the channel closes (client removed) or an
    /// injected `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client:
    /// there is no write half, and `send_http_request` yields
    /// `ClientActionResult::Custom`. So the action goes through [`Self::apply_action`]
    /// - the same function the connected-event path uses - and the outcome is
    /// recorded and replied exactly the way the generic arm does it.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let protocol = HttpClientProtocol;
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
                        info!("HTTP client {} stopped", client_id);
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
        protocol: &HttpClientProtocol,
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
            Ok(action_result) => match Self::apply_action(
                action_result,
                Dispatch::Await,
                client_id,
                app_state,
                llm_client,
                status_tx,
            )
            .await
            {
                // Never `Sent`: reqwest owns the socket and does not report how many
                // bytes the request serialised to, so a byte count here would be
                // invented. `Executed` carries the response status instead, which is
                // both true and more useful.
                Ok(Applied::Executed(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                Err(e) => Err(e),
            },
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
            error!("HTTP client {} injected action failed: {}", client_id, e);
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

    /// Turn one executed action into an HTTP request (or a session end).
    ///
    /// Shared by the connected-event handler and the injected-command loop so the
    /// `http_request` decoding exists exactly once.
    async fn apply_action(
        action_result: ClientActionResult,
        dispatch: Dispatch,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        match action_result {
            ClientActionResult::Custom { name, data } if name == "http_request" => {
                let method = data["method"].as_str().unwrap_or("GET").to_string();
                let path = data["path"].as_str().unwrap_or("/").to_string();
                let headers = data["headers"].as_object().cloned();
                let body = data["body"].as_str().map(|s| s.to_string());

                match dispatch {
                    Dispatch::Spawn => {
                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let (log_method, log_path) = (method.clone(), path.clone());

                        let request_handle = tokio::spawn(async move {
                            if let Err(e) = HttpClient::make_request(
                                client_id,
                                method,
                                path,
                                headers,
                                body,
                                state_clone,
                                llm_clone,
                                status_clone,
                            )
                            .await
                            {
                                error!("HTTP request failed: {}", e);
                            }
                        });
                        // Registered so an in-flight request (and the LLM call it makes
                        // on completion) is aborted when the client is stopped.
                        app_state
                            .register_client_task(client_id, request_handle)
                            .await;
                        Ok(Applied::Executed(format!(
                            "http_request {} {} dispatched",
                            log_method, log_path
                        )))
                    }
                    Dispatch::Await => {
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
                            "http_request {} {} -> {} ({} byte body)",
                            method,
                            path,
                            exchange.status_code,
                            exchange.body.len()
                        );

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let notify_handle = tokio::spawn(async move {
                            HttpClient::notify_response(
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
                }
            }
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            ClientActionResult::WaitForMore => Ok(Applied::Executed("wait_for_more".to_string())),
            ClientActionResult::NoAction => Ok(Applied::Executed("no_action".to_string())),
            // Not swallowed: an action this client cannot carry out says so, rather
            // than looking identical to success.
            ClientActionResult::Custom { name, .. } => Ok(Applied::Executed(format!(
                "custom result '{name}' is not handled by the HTTP client"
            ))),
            ClientActionResult::SendData(_) => Ok(Applied::Executed(
                "send_data has no meaning for a request/response HTTP client".to_string(),
            )),
            ClientActionResult::Multiple(_) => Ok(Applied::Executed(
                "multiple results are not produced by the HTTP client".to_string(),
            )),
        }
    }

    /// Make an HTTP request and hand the response to the LLM.
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

    /// Perform the HTTP round-trip only. No LLM involvement, so a caller can await
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
    ) -> Result<HttpExchange> {
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
        } else {
            format!("{}{}", base_url, path)
        };

        info!(
            "HTTP client {} making request: {} {}",
            client_id, method, url
        );

        let http_client = Self::http_client().await?;

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
                    "HTTP client {} received response: {} ({})",
                    client_id, status_code, status
                );

                Ok(HttpExchange {
                    status_code,
                    status_text: status.to_string(),
                    headers: resp_headers,
                    body: body_text,
                })
            }
            Err(e) => {
                Log::new(Some(status_tx))
                    .error(format!("HTTP client {} request failed: {}", client_id, e));
                Err(e.into())
            }
        }
    }

    /// Hand a completed exchange to the LLM as an `http_response_received` event.
    async fn notify_response(
        client_id: ClientId,
        exchange: HttpExchange,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let protocol = Arc::new(crate::client::http::actions::HttpClientProtocol::new());
        let event = Event::new(
            &HTTP_CLIENT_RESPONSE_RECEIVED_EVENT,
            serde_json::json!({
                "status_code": exchange.status_code,
                "status_text": exchange.status_text,
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
                // They run through `perform_request`, which issues the exchange and raises
                // no event. That bounds the loop (a follow-up cannot trigger another
                // response event and drive the model in circles) and is also the only
                // shape that compiles: routing them back through `make_request` would make
                // notify -> apply -> make -> notify a self-referential async chain, which
                // rustc cannot prove `Send` and `tokio::spawn` therefore rejects.
                use crate::llm::actions::client_trait::{Client, ClientActionResult};
                for action in actions {
                    let Ok(ClientActionResult::Custom { name, data }) =
                        protocol.execute_action(action.clone())
                    else {
                        continue;
                    };
                    if name != "http_request" {
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
                        error!("HTTP client {} follow-up request failed: {}", client_id, e);
                    }
                }
            }
            Err(e) => {
                error!("LLM error for HTTP client {}: {}", client_id, e);
            }
        }
    }
}
