//! JSON-RPC 2.0 client implementation
pub mod actions;

pub use actions::JsonRpcClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::client::jsonrpc::actions::JSONRPC_CLIENT_RESPONSE_RECEIVED_EVENT;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// What [`JsonRpcClient::apply_action`] did with one executed action. JSON-RPC rides on
/// HTTP request/response, so there is no persistent socket whose byte count could be
/// reported - every variant carries a description of what actually happened instead.
enum Applied {
    /// The action ran; the string describes the effect (request made, nothing to do, ...).
    Ran(String),
    /// The action asked to end the session.
    Disconnect,
}

/// Whether the response event is raised inline or from its own task.
#[derive(Clone, Copy)]
enum Dispatch {
    /// Raise it here and now. Used by the connected-event LLM path, which already runs in
    /// its own task and whose ordering the E2E tests rely on.
    Inline,
    /// Hand it to a registered task. Used by the injected-command loop so that a manual
    /// (human-answered) routing rule on `jsonrpc_response_received` cannot hold up the
    /// command's outcome, or the next injected command.
    Deferred,
}

/// One completed exchange waiting to be reported to the LLM.
enum Notification {
    Single((u16, Option<serde_json::Value>)),
    Batch((u16, Option<serde_json::Value>)),
}

/// JSON-RPC 2.0 client that makes RPC calls to remote servers
pub struct JsonRpcClient;

impl JsonRpcClient {
    /// Connect to a JSON-RPC server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // JSON-RPC is HTTP-based, so "connection" is logical
        // We'll create an HTTP client and store it in protocol_data

        // Every other client protocol takes a bare `host:port`, and that is what a model
        // asked to "connect to 127.0.0.1:8080" produces. reqwest requires an absolute URL,
        // so a bare authority reached the wire as `builder error` (relative URL without a
        // base) on every single request -- with nothing in the message naming the address.
        // An explicit scheme is left exactly as given, including https.
        let remote_addr = if remote_addr.contains("://") {
            remote_addr
        } else {
            format!("http://{remote_addr}")
        };

        info!(
            "JSON-RPC client {} initialized for {}",
            client_id, remote_addr
        );

        // Build reqwest client
        let _http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to build HTTP client for JSON-RPC")?;

        // Store client in protocol_data
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field(
                    "jsonrpc_client".to_string(),
                    serde_json::json!("initialized"),
                );
                client.set_protocol_field(
                    "endpoint".to_string(),
                    serde_json::json!(remote_addr.clone()),
                );
                client.set_protocol_field("next_id".to_string(), serde_json::json!(1));
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] JSON-RPC client {} ready for {}",
            client_id, remote_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Command channel for injected actions (the dashboard's [ send_jsonrpc_request ] /
        // [ disconnect ]). Registered BEFORE the connected-event LLM call, which a manual
        // `*` routing rule can park for minutes - the operator must be able to send a
        // request while it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            client_id,
            app_state.clone(),
            llm_client.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Call LLM with initial connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let protocol = Arc::new(crate::client::jsonrpc::actions::JsonRpcClientProtocol::new());
            let event = Event::new(
                &crate::client::jsonrpc::actions::JSONRPC_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "endpoint": remote_addr,
                }),
            );

            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            // Spawn task to process initial actions
            let app_state_clone = app_state.clone();
            let llm_client_clone = llm_client.clone();
            let status_tx_clone = status_tx.clone();
            // Registered with AppState so stop_client can abort this task —
            // dropping a JoinHandle only detaches it in Tokio.
            let task_registrar = app_state.clone();
            let task_handle = tokio::spawn(async move {
                match call_llm_for_client(
                    &llm_client_clone,
                    &app_state_clone,
                    client_id.to_string(),
                    &instruction,
                    &memory,
                    Some(&event),
                    protocol.as_ref(),
                    &status_tx_clone,
                )
                .await
                {
                    Ok(ClientLlmResult {
                        actions,
                        memory_updates,
                    }) => {
                        // Update memory
                        if let Some(mem) = memory_updates {
                            app_state_clone.set_memory_for_client(client_id, mem).await;
                        }

                        // Execute actions through the same path injected commands use.
                        for action in actions {
                            let result = match protocol.execute_action(action) {
                                Ok(result) => result,
                                Err(e) => {
                                    error!("JSON-RPC client {} rejected action: {}", client_id, e);
                                    continue;
                                }
                            };
                            match Self::apply_action(
                                result,
                                Dispatch::Inline,
                                client_id,
                                &app_state_clone,
                                &llm_client_clone,
                                &status_tx_clone,
                            )
                            .await
                            {
                                Ok(Applied::Ran(detail)) => {
                                    info!("JSON-RPC client {}: {}", client_id, detail);
                                }
                                Ok(Applied::Disconnect) => break,
                                Err(e) => {
                                    error!("JSON-RPC client {} action failed: {}", client_id, e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("LLM error for JSON-RPC client {}: {}", client_id, e);
                    }
                }
            });
            task_registrar
                .register_client_task(client_id, task_handle)
                .await;
        }

        // No idle-poll task: the command loop above is this client's only long-lived task
        // and it ends when the client is removed (`remove_client` drops the command
        // sender, so `recv()` returns `None`).

        // Return a dummy local address (JSON-RPC is connectionless over HTTP)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Run one executed action. Shared by the connected-event LLM path and injected
    /// commands so the request encoding exists exactly once.
    ///
    /// `dispatch` decides only *when the response event is raised*, never what goes on the
    /// wire: the LLM path keeps raising it inline, while an injected command awaits the
    /// HTTP round-trip and hands the event to its own task, so a manual routing rule
    /// parking on `jsonrpc_response_received` cannot wedge the command loop.
    async fn apply_action(
        result: ClientActionResult,
        dispatch: Dispatch,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        match result {
            ClientActionResult::Custom { name, data } if name == "jsonrpc_request" => {
                let method = data
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = data.get("params").cloned();
                let id = data.get("id").cloned();

                let exchange =
                    Self::perform_request(client_id, &method, params, id, app_state).await?;
                let detail = format!(
                    "jsonrpc_request '{}' sent; HTTP {} ({})",
                    method,
                    exchange.0,
                    if exchange.1.is_some() {
                        "JSON-RPC response received"
                    } else {
                        "body was not valid JSON"
                    }
                );
                Self::deliver_response(
                    Notification::Single(exchange),
                    dispatch,
                    client_id,
                    app_state,
                    llm_client,
                    status_tx,
                )
                .await;
                Ok(Applied::Ran(detail))
            }
            ClientActionResult::Custom { name, data } if name == "jsonrpc_batch" => {
                let requests = data
                    .get("requests")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let count = requests.len();

                let exchange = Self::perform_batch_request(client_id, requests, app_state).await?;
                let detail = format!(
                    "jsonrpc_batch of {} request(s) sent; HTTP {} ({})",
                    count,
                    exchange.0,
                    if exchange.1.is_some() {
                        "batch response received"
                    } else {
                        "body was not valid JSON"
                    }
                );
                Self::deliver_response(
                    Notification::Batch(exchange),
                    dispatch,
                    client_id,
                    app_state,
                    llm_client,
                    status_tx,
                )
                .await;
                Ok(Applied::Ran(detail))
            }
            ClientActionResult::Disconnect => {
                info!("JSON-RPC client {} disconnecting", client_id);
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                Ok(Applied::Disconnect)
            }
            ClientActionResult::Custom { name, .. } => Err(anyhow::anyhow!(
                "JSON-RPC client cannot execute custom result '{}'",
                name
            )),
            // WaitForMore / NoAction / SendData / nested Multiple: nothing to put on the
            // wire for a request/response protocol.
            _ => Ok(Applied::Ran(
                "no request made (action produced no JSON-RPC call)".to_string(),
            )),
        }
    }

    /// Raise the response event, inline or from a registered task.
    async fn deliver_response(
        notification: Notification,
        dispatch: Dispatch,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        match dispatch {
            Dispatch::Inline => match notification {
                Notification::Single(exchange) => {
                    Self::notify_response(client_id, exchange, app_state, llm_client, status_tx)
                        .await
                }
                Notification::Batch(exchange) => {
                    Self::notify_batch_response(
                        client_id, exchange, app_state, llm_client, status_tx,
                    )
                    .await
                }
            },
            Dispatch::Deferred => {
                let state = app_state.clone();
                let llm = llm_client.clone();
                let tx = status_tx.clone();
                let handle = tokio::spawn(async move {
                    match notification {
                        Notification::Single(exchange) => {
                            Self::notify_response(client_id, exchange, &state, &llm, &tx).await
                        }
                        Notification::Batch(exchange) => {
                            Self::notify_batch_response(client_id, exchange, &state, &llm, &tx)
                                .await
                        }
                    }
                });
                // Registered so stop_client aborts an in-flight LLM call for this event.
                app_state.register_client_task(client_id, handle).await;
            }
        }
    }

    /// Drain injected commands until the channel closes (the client was removed) or an
    /// injected `disconnect` ends the session. The request is awaited, so the reported
    /// [`ClientSendOutcome`] describes a round-trip that really happened.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;

        let protocol = crate::client::jsonrpc::actions::JsonRpcClientProtocol::new();

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => Self::apply_action(
                    result,
                    Dispatch::Deferred,
                    client_id,
                    &app_state,
                    &llm_client,
                    &status_tx,
                )
                .await
                .map(|applied| match applied {
                    Applied::Disconnect => ClientSendOutcome::Disconnected,
                    Applied::Ran(detail) => ClientSendOutcome::Executed { detail },
                }),
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
                error!(
                    "JSON-RPC client {} injected action failed: {}",
                    client_id, e
                );
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                break;
            }
        }

        // Nothing can be injected any more: stop the dashboard offering [ send ].
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        info!("JSON-RPC client {} command loop ended", client_id);
    }

    /// Make a JSON-RPC request and hand the response to the LLM.
    pub async fn make_request(
        client_id: ClientId,
        method: String,
        params: Option<serde_json::Value>,
        id: Option<serde_json::Value>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let exchange = Self::perform_request(client_id, &method, params, id, &app_state).await?;
        Self::notify_response(client_id, exchange, &app_state, &llm_client, &status_tx).await;
        Ok(())
    }

    /// POST one JSON-RPC request and return `(http_status, parsed_body)`. Split out of
    /// [`Self::make_request`] so the injected-command loop can await the round-trip - and
    /// report what really happened - without also awaiting the LLM call the response event
    /// triggers, which a manual routing rule can park for minutes.
    async fn perform_request(
        client_id: ClientId,
        method: &str,
        params: Option<serde_json::Value>,
        id: Option<serde_json::Value>,
        app_state: &Arc<AppState>,
    ) -> Result<(u16, Option<serde_json::Value>)> {
        // Get endpoint from client
        let endpoint = app_state
            .with_client_mut(client_id, |client| {
                client
                    .get_protocol_field("endpoint")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .await
            .flatten()
            .context("No endpoint found")?;

        info!("JSON-RPC client {} calling method: {}", client_id, method);

        // Build JSON-RPC request
        let mut request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });

        if let Some(p) = params {
            request["params"] = p;
        }

        if let Some(request_id) = id {
            request["id"] = request_id.clone();
        }

        // Build HTTP request
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let response = http_client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status_code = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();

        info!(
            "JSON-RPC client {} received response: {}",
            client_id, status_code
        );

        match serde_json::from_str::<serde_json::Value>(&body_text) {
            Ok(response_json) => Ok((status_code, Some(response_json))),
            Err(_) => {
                error!(
                    "JSON-RPC client {} received invalid JSON response",
                    client_id
                );
                Ok((status_code, None))
            }
        }
    }

    /// Raise `jsonrpc_response_received` for a completed exchange.
    /// Run the actions the model returned for a response event.
    ///
    /// They were discarded (`actions: _`) at both notify sites, so answering
    /// jsonrpc_response_received did nothing -- the whole point of raising it. Dispatch
    /// goes through the `perform_*` cores, which raise no event: that bounds the loop and
    /// avoids a notify -> perform -> notify chain rustc cannot prove `Send`.
    async fn run_follow_ups(
        client_id: ClientId,
        actions: Vec<serde_json::Value>,
        app_state: &Arc<AppState>,
    ) {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};
        let protocol = crate::client::jsonrpc::actions::JsonRpcClientProtocol::new();
        for action in actions {
            let Ok(ClientActionResult::Custom { name, data }) =
                protocol.execute_action(action.clone())
            else {
                continue;
            };
            let outcome = match name.as_str() {
                "jsonrpc_request" => Self::perform_request(
                    client_id,
                    data["method"].as_str().unwrap_or_default(),
                    data.get("params").cloned(),
                    data.get("id").cloned(),
                    app_state,
                )
                .await
                .map(|_| ()),
                "jsonrpc_batch" => Self::perform_batch_request(
                    client_id,
                    data["requests"].as_array().cloned().unwrap_or_default(),
                    app_state,
                )
                .await
                .map(|_| ()),
                other => {
                    info!(
                        "JSON-RPC client {} follow-up '{}' has no non-notifying path; skipped",
                        client_id, other
                    );
                    Ok(())
                }
            };
            if let Err(e) = outcome {
                error!(
                    "JSON-RPC client {} follow-up action failed: {}",
                    client_id, e
                );
            }
        }
    }

    async fn notify_response(
        client_id: ClientId,
        exchange: (u16, Option<serde_json::Value>),
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let Some(response_json) = exchange.1 else {
            return;
        };
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let protocol = Arc::new(crate::client::jsonrpc::actions::JsonRpcClientProtocol::new());
        let event = Event::new(&JSONRPC_CLIENT_RESPONSE_RECEIVED_EVENT, response_json);

        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        match call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            protocol.as_ref(),
            status_tx,
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
                Self::run_follow_ups(client_id, actions, &app_state).await;
            }
            Err(e) => {
                error!("LLM error for JSON-RPC client {}: {}", client_id, e);
            }
        }
    }

    /// Make a batch JSON-RPC request and hand the responses to the LLM.
    pub async fn make_batch_request(
        client_id: ClientId,
        requests: Vec<serde_json::Value>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let exchange = Self::perform_batch_request(client_id, requests, &app_state).await?;
        Self::notify_batch_response(client_id, exchange, &app_state, &llm_client, &status_tx).await;
        Ok(())
    }

    /// POST one JSON-RPC batch and return `(http_status, parsed_body)`. Same split (and
    /// same reason) as [`Self::perform_request`].
    async fn perform_batch_request(
        client_id: ClientId,
        requests: Vec<serde_json::Value>,
        app_state: &Arc<AppState>,
    ) -> Result<(u16, Option<serde_json::Value>)> {
        // Get endpoint from client
        let endpoint = app_state
            .with_client_mut(client_id, |client| {
                client
                    .get_protocol_field("endpoint")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .await
            .flatten()
            .context("No endpoint found")?;

        info!(
            "JSON-RPC client {} sending batch with {} requests",
            client_id,
            requests.len()
        );

        // Build batch request (array of JSON-RPC requests)
        let mut batch = Vec::new();
        for req in requests {
            let mut request = serde_json::json!({
                "jsonrpc": "2.0",
            });

            // Merge the request fields
            if let Some(method) = req.get("method") {
                request["method"] = method.clone();
            }
            if let Some(params) = req.get("params") {
                request["params"] = params.clone();
            }
            if let Some(id) = req.get("id") {
                request["id"] = id.clone();
            }

            batch.push(request);
        }

        // Build HTTP request
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let response = http_client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .json(&batch)
            .send()
            .await?;

        let status_code = response.status().as_u16();
        let body_text = response.text().await.unwrap_or_default();

        info!(
            "JSON-RPC client {} received batch response: {}",
            client_id, status_code
        );

        match serde_json::from_str::<serde_json::Value>(&body_text) {
            Ok(response_json) => Ok((status_code, Some(response_json))),
            Err(_) => {
                error!(
                    "JSON-RPC client {} received invalid JSON batch response",
                    client_id
                );
                Ok((status_code, None))
            }
        }
    }

    /// Raise `jsonrpc_response_received` for a completed batch exchange.
    async fn notify_batch_response(
        client_id: ClientId,
        exchange: (u16, Option<serde_json::Value>),
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let Some(response_json) = exchange.1 else {
            return;
        };
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let protocol = Arc::new(crate::client::jsonrpc::actions::JsonRpcClientProtocol::new());
        let event = Event::new(
            &JSONRPC_CLIENT_RESPONSE_RECEIVED_EVENT,
            serde_json::json!({
                "batch": true,
                "responses": response_json,
            }),
        );

        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        match call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            protocol.as_ref(),
            status_tx,
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
                Self::run_follow_ups(client_id, actions, &app_state).await;
            }
            Err(e) => {
                error!("LLM error for JSON-RPC client {}: {}", client_id, e);
            }
        }
    }
}
