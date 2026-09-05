//! OpenAI client implementation
pub mod actions;

pub use actions::OpenAiClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::openai::actions::{
    OPENAI_CLIENT_CONNECTED_EVENT, OPENAI_CLIENT_RESPONSE_RECEIVED_EVENT,
};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::actions::protocol_trait::Protocol;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, StartupParams};
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// OpenAI client that connects to the OpenAI API
pub struct OpenAiClient;

impl OpenAiClient {
    /// Connect to OpenAI API with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract API key from startup params
        let api_key = startup_params
            .as_ref()
            .map(|p| p.get_string("api_key"))
            .transpose()?
            .context("OpenAI API key is required")?;

        let default_model = startup_params
            .as_ref()
            .map(|p| p.get_optional_string("default_model"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| "gpt-3.5-turbo".to_string());

        let organization = startup_params
            .as_ref()
            .map(|p| p.get_optional_string("organization"))
            .transpose()?
            .flatten();

        info!(
            "OpenAI client {} initializing with API endpoint: {}",
            client_id, remote_addr
        );

        // Store configuration in protocol_data
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field("api_key".to_string(), serde_json::json!(api_key));
                client.set_protocol_field(
                    "default_model".to_string(),
                    serde_json::json!(default_model),
                );
                client
                    .set_protocol_field("api_endpoint".to_string(), serde_json::json!(remote_addr));
                if let Some(org) = organization {
                    client.set_protocol_field("organization".to_string(), serde_json::json!(org));
                }
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        Log::new(Some(&status_tx)).info(format!(
            "OpenAI client {} ready (endpoint: {})",
            client_id, remote_addr
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Command channel for injected actions (the dashboard's [ send ] row).
        // Registered - and already being drained by its own task - BEFORE the
        // connected-event LLM call, which a manual `*` rule can park for minutes: the
        // operator must be able to reach the client while it waits. This task also
        // replaces the old 5s "is the client gone yet" poll: the channel closes when the
        // client is removed, which ends the loop immediately.
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

        // Call LLM with openai_connected event
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &OPENAI_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "api_endpoint": remote_addr.clone(),
                }),
            );

            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &String::new(),
                Some(&event),
                &crate::client::openai::actions::OpenAiClientProtocol,
                &status_tx,
            )
            .await
            {
                Ok(result) => {
                    info!("OpenAI client ready after connect event");
                    let protocol = crate::client::openai::actions::OpenAiClientProtocol::new();
                    for action in result.actions {
                        match protocol.execute_action(action.clone()) {
                            Ok(ClientActionResult::Disconnect) => {
                                info!(
                                    "OpenAI client {} disconnecting on connect-event action",
                                    client_id
                                );
                                app_state
                                    .update_client_status(client_id, ClientStatus::Disconnected)
                                    .await;
                                // Every exit path drops the handle so the dashboard stops
                                // offering [ send ] into a dead client.
                                app_state.remove_client_handle(client_id).await;
                                let _ = status_tx.send("__UPDATE_UI__".to_string());
                                return Ok("0.0.0.0:0".parse().unwrap());
                            }
                            Ok(result) => {
                                // Dispatch::Spawn: a 30s API call must not hold up
                                // `connect()`. The injected path awaits the same
                                // `apply_action` so its ClientSendOutcome is truthful.
                                if let Err(e) = Self::apply_action(
                                    result,
                                    Dispatch::Spawn,
                                    client_id,
                                    &app_state,
                                    &llm_client,
                                    &status_tx,
                                )
                                .await
                                {
                                    error!("OpenAI request failed: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("OpenAI action execution error: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error on openai_connected event: {}", e);
                }
            }
        }

        // Return a dummy local address (OpenAI is a remote API, not a local connection)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Drain injected commands until the channel closes (client removed) or an injected
    /// `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client: it writes
    /// `SendData` to a socket, and every OpenAI verb yields `ClientActionResult::Custom`
    /// that has to go out through `async-openai`. So the action is executed through the
    /// protocol's own `execute_action` and applied by [`Self::apply_action`] - the same
    /// function the connected-event path uses - and the outcome is recorded and replied
    /// exactly the way the generic arm does it.
    async fn command_loop(
        mut command_rx: tokio::sync::mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let protocol = crate::client::openai::actions::OpenAiClientProtocol::new();

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                // Dispatch::Await: the network exchange is awaited, so the reported
                // outcome describes a request that has actually completed. The
                // openai_response_received event is delivered from its own registered
                // task, so a manual handler parked for a human's think time cannot wedge
                // this loop or time out the dashboard's [ send ].
                Ok(result) => Self::apply_action(
                    result,
                    Dispatch::Await,
                    client_id,
                    &app_state,
                    &llm_client,
                    &status_tx,
                )
                .await
                .map(|applied| match applied {
                    Applied::Disconnect => ClientSendOutcome::Disconnected,
                    Applied::Executed(detail) => ClientSendOutcome::Executed { detail },
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
                error!("OpenAI client {} injected action failed: {}", client_id, e);
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
                break;
            }
        }

        info!("OpenAI client {} command loop stopped", client_id);
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Apply one already-executed action result. The single place an OpenAI API call is
    /// dispatched from, so an injected action behaves exactly like an LLM-produced one.
    async fn apply_action(
        result: ClientActionResult,
        dispatch: Dispatch,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        match result {
            ClientActionResult::Custom { name, data } if name == "openai_chat_completion" => {
                let messages = data
                    .get("messages")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let model = data
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let temperature = data.get("temperature").and_then(|v| v.as_f64());
                let max_tokens = data
                    .get("max_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let functions = data.get("functions").cloned().filter(|v| !v.is_null());
                let model_label = model.clone().unwrap_or_else(|| "<default>".to_string());

                match dispatch {
                    Dispatch::Spawn => {
                        let state_clone = app_state.clone();
                        let llm_clone = llm_client.clone();
                        let status_clone = status_tx.clone();
                        let request_handle = tokio::spawn(async move {
                            if let Err(e) = Self::make_chat_completion(
                                client_id,
                                messages,
                                model,
                                temperature,
                                max_tokens,
                                functions,
                                state_clone,
                                llm_clone,
                                status_clone,
                            )
                            .await
                            {
                                error!("OpenAI chat completion failed: {}", e);
                            }
                        });
                        app_state
                            .register_client_task(client_id, request_handle)
                            .await;
                        Ok(Applied::Executed(format!(
                            "send_chat_completion dispatched (model={model_label})"
                        )))
                    }
                    Dispatch::Await => {
                        let exchange = Self::finish_exchange(
                            Self::perform_chat_completion(
                                client_id,
                                messages,
                                model,
                                temperature,
                                max_tokens,
                                functions,
                                app_state,
                                status_tx,
                            )
                            .await,
                            client_id,
                            app_state,
                            llm_client,
                            status_tx,
                        )
                        .await?;
                        Ok(Applied::Executed(format!(
                            "send_chat_completion completed (model={model_label}, {})",
                            exchange.summary
                        )))
                    }
                }
            }
            ClientActionResult::Custom { name, data } if name == "openai_embedding" => {
                let input = data
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let model = data
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let model_label = model
                    .clone()
                    .unwrap_or_else(|| "text-embedding-ada-002".to_string());

                match dispatch {
                    Dispatch::Spawn => {
                        let state_clone = app_state.clone();
                        let llm_clone = llm_client.clone();
                        let status_clone = status_tx.clone();
                        let request_handle = tokio::spawn(async move {
                            if let Err(e) = Self::make_embedding_request(
                                client_id,
                                input,
                                model,
                                state_clone,
                                llm_clone,
                                status_clone,
                            )
                            .await
                            {
                                error!("OpenAI embedding request failed: {}", e);
                            }
                        });
                        app_state
                            .register_client_task(client_id, request_handle)
                            .await;
                        Ok(Applied::Executed(format!(
                            "send_embedding_request dispatched (model={model_label})"
                        )))
                    }
                    Dispatch::Await => {
                        let exchange = Self::finish_exchange(
                            Self::perform_embedding_request(
                                client_id, input, model, app_state, status_tx,
                            )
                            .await,
                            client_id,
                            app_state,
                            llm_client,
                            status_tx,
                        )
                        .await?;
                        Ok(Applied::Executed(format!(
                            "send_embedding_request completed (model={model_label}, {})",
                            exchange.summary
                        )))
                    }
                }
            }
            ClientActionResult::Custom { name, .. } => Ok(Applied::Executed(format!(
                "custom result '{name}' has no OpenAI executor"
            ))),
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            ClientActionResult::WaitForMore => Ok(Applied::Executed("wait_for_more".to_string())),
            ClientActionResult::NoAction => Ok(Applied::Executed("no_action".to_string())),
            ClientActionResult::SendData(_) => Ok(Applied::Executed(
                "raw send_data has no meaning for the OpenAI HTTPS client".to_string(),
            )),
            // OpenAiClientProtocol::execute_action never produces Multiple.
            ClientActionResult::Multiple(_) => Ok(Applied::Executed(
                "multiple results are not produced by the OpenAI client".to_string(),
            )),
        }
    }

    /// Make a chat completion request and hand the result to the LLM.
    #[allow(clippy::too_many_arguments)]
    pub async fn make_chat_completion(
        client_id: ClientId,
        messages: serde_json::Value,
        model: Option<String>,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
        functions: Option<serde_json::Value>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let outcome = Self::perform_chat_completion(
            client_id,
            messages,
            model,
            temperature,
            max_tokens,
            functions,
            &app_state,
            &status_tx,
        )
        .await;
        Self::notify_outcome(outcome, client_id, &app_state, &llm_client, &status_tx)
            .await
            .map(|_| ())
    }

    /// Make an embedding request and hand the result to the LLM.
    pub async fn make_embedding_request(
        client_id: ClientId,
        input: serde_json::Value,
        model: Option<String>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let outcome =
            Self::perform_embedding_request(client_id, input, model, &app_state, &status_tx).await;
        Self::notify_outcome(outcome, client_id, &app_state, &llm_client, &status_tx)
            .await
            .map(|_| ())
    }

    /// Deliver an exchange's `openai_response_received` event **inline** and wait for it.
    /// Used by the LLM-driven path, where nobody is waiting on a reply.
    async fn notify_outcome(
        outcome: Result<OpenAiExchange>,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<OpenAiExchange> {
        let event_data = match &outcome {
            Ok(exchange) => exchange.event_data.clone(),
            Err(e) => error_event_data(e),
        };
        Self::notify_response(
            client_id,
            event_data,
            app_state.clone(),
            llm_client.clone(),
            status_tx.clone(),
        )
        .await;
        outcome
    }

    /// Deliver an exchange's `openai_response_received` event from **its own registered
    /// task** and return the exchange immediately.
    ///
    /// This is the point of the perform/notify split. The injected-command loop already
    /// holds the truthful network result and must reply to the operator before that
    /// event's handler runs: a dashboard-created client defaults to a `*` -> manual rule,
    /// so the handler can park for a human's think time (300s by default), far longer
    /// than the composer's 30s send timeout. What the model decides to do with the
    /// response is a different question from whether the injected action reached the wire.
    async fn finish_exchange(
        outcome: Result<OpenAiExchange>,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<OpenAiExchange> {
        let event_data = match &outcome {
            Ok(exchange) => exchange.event_data.clone(),
            Err(e) => error_event_data(e),
        };
        let state_clone = app_state.clone();
        let llm_clone = llm_client.clone();
        let status_clone = status_tx.clone();
        let notify_handle = tokio::spawn(async move {
            OpenAiClient::notify_response(
                client_id,
                event_data,
                state_clone,
                llm_clone,
                status_clone,
            )
            .await;
        });
        // Registered so the notification - and the LLM call it makes - is aborted when
        // the client is stopped.
        app_state
            .register_client_task(client_id, notify_handle)
            .await;
        outcome
    }

    /// Fire one `openai_response_received` event at the LLM and apply any memory update.
    async fn notify_response(
        client_id: ClientId,
        event_data: serde_json::Value,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let protocol = Arc::new(crate::client::openai::actions::OpenAiClientProtocol::new());
        let event = Event::new(&OPENAI_CLIENT_RESPONSE_RECEIVED_EVENT, event_data);
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
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }

                // Execute what the model asked for. These used to be discarded
                // (`actions: _`), so answering openai_response_received did nothing --
                // the whole point of raising it. A model that reads a completion and
                // wants to send a follow-up prompt was silently ignored.
                //
                // They run through the `perform_*` round-trips, which raise no event and
                // call no LLM. That bounds the loop -- a follow-up cannot raise another
                // response event and drive the model round in circles -- and it is also
                // the only shape that compiles: routing them back through
                // `make_chat_completion` would make notify -> apply -> make -> notify a
                // self-referential async chain, which rustc cannot prove `Send`.
                use crate::llm::actions::client_trait::{Client, ClientActionResult};
                for action in actions {
                    let decoded = match protocol.execute_action(action.clone()) {
                        Ok(d) => d,
                        Err(e) => {
                            error!(
                                "OpenAI client {} rejected its own follow-up action: {}",
                                client_id, e
                            );
                            continue;
                        }
                    };
                    let ClientActionResult::Custom { name, data } = decoded else {
                        continue;
                    };
                    let outcome = match name.as_str() {
                        "openai_chat_completion" => Self::perform_chat_completion(
                            client_id,
                            data.get("messages")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                            data.get("model").and_then(|v| v.as_str()).map(String::from),
                            data.get("temperature").and_then(|v| v.as_f64()),
                            data.get("max_tokens")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as u32),
                            data.get("functions").cloned().filter(|v| !v.is_null()),
                            &app_state,
                            &status_tx,
                        )
                        .await
                        .map(|_| ()),
                        "openai_embedding" => Self::perform_embedding_request(
                            client_id,
                            data.get("input")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                            data.get("model").and_then(|v| v.as_str()).map(String::from),
                            &app_state,
                            &status_tx,
                        )
                        .await
                        .map(|_| ()),
                        other => {
                            info!(
                                "OpenAI client {} follow-up '{}' has no wire effect",
                                client_id, other
                            );
                            Ok(())
                        }
                    };
                    if let Err(e) = outcome {
                        error!("OpenAI client {} follow-up action failed: {}", client_id, e);
                    }
                }
            }
            Err(e) => {
                error!("LLM error for OpenAI client {}: {}", client_id, e);
            }
        }
    }

    /// Perform the chat-completion round-trip only. No LLM involvement, so a caller can
    /// await this and know exactly what OpenAI answered.
    #[allow(clippy::too_many_arguments)]
    pub async fn perform_chat_completion(
        client_id: ClientId,
        messages: serde_json::Value,
        model: Option<String>,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
        functions: Option<serde_json::Value>,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<OpenAiExchange> {
        // Get API configuration from client
        let (api_key, default_model, api_endpoint) = app_state
            .with_client_mut(client_id, |client| {
                let key = client
                    .get_protocol_field("api_key")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let model = client
                    .get_protocol_field("default_model")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let endpoint = client
                    .get_protocol_field("api_endpoint")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (key, model, endpoint)
            })
            .await
            .unwrap_or((None, None, None));

        let api_key = api_key.context("No API key found")?;
        let model_to_use =
            model.unwrap_or_else(|| default_model.unwrap_or_else(|| "gpt-3.5-turbo".to_string()));

        info!(
            "OpenAI client {} making chat completion request with model: {}",
            client_id, model_to_use
        );

        // Build OpenAI client
        use async_openai::{types::*, Client as OpenAiApiClient};

        let mut config = async_openai::config::OpenAIConfig::new().with_api_key(&api_key);

        // Override API base if custom endpoint is provided
        if let Some(endpoint) = api_endpoint {
            if !endpoint.is_empty() && endpoint != "https://api.openai.com/v1" {
                config = config.with_api_base(&endpoint);
            }
        }

        let openai_client = OpenAiApiClient::with_config(config);

        // Parse messages array
        let messages_array = messages.as_array().context("Messages must be an array")?;

        let mut chat_messages = Vec::new();
        for msg in messages_array {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .context("Message role is required")?;
            let content = msg
                .get("content")
                .and_then(|v| v.as_str())
                .context("Message content is required")?;

            let chat_message = match role {
                "system" => {
                    ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                        content: ChatCompletionRequestSystemMessageContent::Text(
                            content.to_string(),
                        ),
                        name: None,
                    })
                }
                "user" => ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(content.to_string()),
                    name: None,
                }),
                "assistant" => {
                    ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
                        content: Some(ChatCompletionRequestAssistantMessageContent::Text(
                            content.to_string(),
                        )),
                        name: None,
                        tool_calls: None,
                        refusal: None,
                        #[allow(deprecated)]
                        function_call: None,
                    })
                }
                _ => return Err(anyhow::anyhow!("Unknown message role: {}", role)),
            };
            chat_messages.push(chat_message);
        }

        // Build request
        let mut request = CreateChatCompletionRequestArgs::default();
        request.model(&model_to_use);
        request.messages(chat_messages);

        if let Some(temp) = temperature {
            request.temperature(temp as f32);
        }

        if let Some(tokens) = max_tokens {
            request.max_tokens(tokens as u16);
        }

        // Add function calling support
        if let Some(functions_value) = functions {
            if let Some(functions_array) = functions_value.as_array() {
                let mut tools = Vec::new();
                for func in functions_array {
                    // Convert the function definition to OpenAI tool format
                    if let Ok(tool) = serde_json::from_value::<
                        async_openai::types::ChatCompletionTool,
                    >(func.clone())
                    {
                        tools.push(tool);
                    } else {
                        warn!(
                            "OpenAI client {}: Failed to parse function definition: {:?}",
                            client_id, func
                        );
                    }
                }
                if !tools.is_empty() {
                    let tools_count = tools.len();
                    request.tools(tools);
                    info!(
                        "OpenAI client {}: Added {} function(s) to request",
                        client_id, tools_count
                    );
                }
            }
        }

        let request = request
            .build()
            .context("Failed to build chat completion request")?;

        // Make request
        match openai_client.chat().create(request).await {
            Ok(response) => {
                let choice = response
                    .choices
                    .first()
                    .context("No choices in OpenAI response")?;

                let content = choice
                    .message
                    .content
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_default();

                let total_tokens = response.usage.as_ref().map(|u| u.total_tokens).unwrap_or(0);
                let usage = serde_json::json!({
                    "prompt_tokens": response.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
                    "completion_tokens": response.usage.as_ref().map(|u| u.completion_tokens).unwrap_or(0),
                    "total_tokens": total_tokens,
                });

                // Extract tool_calls if present
                let tool_calls = choice.message.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.function.name,
                                    "arguments": call.function.arguments
                                }
                            })
                        })
                        .collect::<Vec<_>>()
                });

                info!(
                    "OpenAI client {} received response ({} tokens{})",
                    client_id,
                    total_tokens,
                    if tool_calls.is_some() {
                        ", with tool calls"
                    } else {
                        ""
                    }
                );

                let summary = format!(
                    "{} tokens, {} chars of content",
                    total_tokens,
                    content.chars().count()
                );

                // Built here, delivered by `notify_response` - inline for the LLM path,
                // from its own task for the injected-command path.
                let mut event_data = serde_json::json!({
                    "response_type": "chat_completion",
                    "content": content,
                    "model": response.model,
                    "usage": usage,
                });
                if let Some(calls) = tool_calls {
                    event_data["tool_calls"] = serde_json::json!(calls);
                }

                Ok(OpenAiExchange {
                    event_data,
                    summary,
                })
            }
            Err(e) => {
                Log::new(Some(status_tx))
                    .error(format!("OpenAI client {} request failed: {}", client_id, e));
                Err(e.into())
            }
        }
    }

    /// Perform the embedding round-trip only. No LLM involvement.
    pub async fn perform_embedding_request(
        client_id: ClientId,
        input: serde_json::Value,
        model: Option<String>,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<OpenAiExchange> {
        // Get API configuration from client
        let (api_key, api_endpoint) = app_state
            .with_client_mut(client_id, |client| {
                let key = client
                    .get_protocol_field("api_key")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let endpoint = client
                    .get_protocol_field("api_endpoint")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                (key, endpoint)
            })
            .await
            .unwrap_or((None, None));

        let api_key = api_key.context("No API key found")?;
        let model_to_use = model.unwrap_or_else(|| "text-embedding-ada-002".to_string());

        info!(
            "OpenAI client {} making embedding request with model: {}",
            client_id, model_to_use
        );

        // Build OpenAI client
        use async_openai::{types::*, Client as OpenAiApiClient};

        let mut config = async_openai::config::OpenAIConfig::new().with_api_key(&api_key);

        if let Some(endpoint) = api_endpoint {
            if !endpoint.is_empty() && endpoint != "https://api.openai.com/v1" {
                config = config.with_api_base(&endpoint);
            }
        }

        let openai_client = OpenAiApiClient::with_config(config);

        // Parse input (can be string or array)
        let input_value = if let Some(text) = input.as_str() {
            EmbeddingInput::String(text.to_string())
        } else if let Some(arr) = input.as_array() {
            let strings: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect();
            EmbeddingInput::StringArray(strings)
        } else {
            return Err(anyhow::anyhow!(
                "Input must be a string or array of strings"
            ));
        };

        // Build request
        let request = CreateEmbeddingRequestArgs::default()
            .model(&model_to_use)
            .input(input_value)
            .build()
            .context("Failed to build embedding request")?;

        // Make request
        match openai_client.embeddings().create(request).await {
            Ok(response) => {
                let embeddings: Vec<Vec<f32>> =
                    response.data.iter().map(|e| e.embedding.clone()).collect();

                let usage = serde_json::json!({
                    "prompt_tokens": response.usage.prompt_tokens,
                    "total_tokens": response.usage.total_tokens,
                });

                info!(
                    "OpenAI client {} received {} embeddings ({} tokens)",
                    client_id,
                    embeddings.len(),
                    response.usage.total_tokens
                );

                let summary = format!(
                    "{} embeddings of {} dimensions",
                    embeddings.len(),
                    embeddings.first().map(|e| e.len()).unwrap_or(0)
                );

                let event_data = serde_json::json!({
                    "response_type": "embedding",
                    "content": format!("Generated {} embeddings", embeddings.len()),
                    "model": response.model,
                    "usage": usage,
                    "embeddings_count": embeddings.len(),
                    "embedding_dimensions": embeddings.first().map(|e| e.len()).unwrap_or(0),
                });

                Ok(OpenAiExchange {
                    event_data,
                    summary,
                })
            }
            Err(e) => {
                Log::new(Some(status_tx)).error(format!(
                    "OpenAI client {} embedding request failed: {}",
                    client_id, e
                ));
                Err(e.into())
            }
        }
    }
}

/// The `openai_response_received` payload for a request that failed. Kept next to the
/// success payload so both paths report through the same event type.
fn error_event_data(error: &anyhow::Error) -> serde_json::Value {
    serde_json::json!({
        "response_type": "error",
        "content": error.to_string(),
    })
}

/// How an OpenAI API call is issued.
#[derive(Clone, Copy)]
enum Dispatch {
    /// Spawn the request and return immediately. Used by the connected-event handler,
    /// which runs inline in `connect()` and must not block client creation on a request
    /// that can take the full 30s timeout.
    Spawn,
    /// Await the API round-trip so the caller can report what actually happened. Used by
    /// the injected-command loop. The response event is still delivered to the LLM, from
    /// its own registered task, so a parked manual handler cannot wedge the command loop
    /// for the length of a human's think time.
    Await,
}

/// One completed OpenAI exchange.
///
/// Split out of [`OpenAiClient::make_chat_completion`] so the injected-command loop can
/// await the network round-trip - and report a truthful outcome - without also awaiting
/// the LLM call the response event triggers.
pub struct OpenAiExchange {
    /// The `openai_response_received` payload this exchange produced.
    pub event_data: serde_json::Value,
    /// A short human summary for an injected action's outcome detail.
    pub summary: String,
}

/// What [`OpenAiClient::apply_action`] did with one action. The OpenAI client owns no
/// socket, so there is no honest byte count to report - only "the API call ran" or
/// "the session should end".
enum Applied {
    /// The action ran; the string says what, for the injected action's outcome detail.
    Executed(String),
    /// The session should end.
    Disconnect,
}
