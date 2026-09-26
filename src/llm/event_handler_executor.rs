//! Event handler executor - executes configured event handlers (script/static/llm)
//!
//! This module checks if an event has a configured handler and executes it accordingly.
//! Supports three handler types:
//! - Script: Execute inline script code
//! - Static: Execute predefined actions
//! - LLM: Delegate to LLM (fallback/default)

use crate::llm::actions::executor::{execute_actions, ExecutionResult};
use crate::llm::actions::protocol_trait::Server;
use crate::scripting::EventHandlerType;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use anyhow::{Context as AnyhowContext, Result};
use tracing::{debug, error, warn};

/// Heading under which a `{"type":"llm","instruction":"…"}` handler's instruction is
/// added to the event trigger message.
///
/// It is deliberately distinct from any wording the server-wide instruction uses, so a
/// test (and a human reading `netget.log`) can tell the two apart.
pub const HANDLER_INSTRUCTION_HEADER: &str = "Handler instruction for this event";

/// Result from checking event handlers
pub enum EventHandlerResult {
    /// Handler executed successfully with result
    Handled(ExecutionResult),
    /// No handler configured or handler requested LLM fallback.
    ///
    /// `instruction` is `Some` only for an explicit `{"type":"llm","instruction":"…"}`
    /// handler; the caller must add it to the prompt for this event. It used to be
    /// dropped here behind a `// TODO`, which meant an MCP caller could configure a
    /// per-event instruction, get no error, and silently receive the server-wide
    /// instruction instead.
    FallbackToLlm { instruction: Option<String> },
}

/// Check and execute event handler for the given event
///
/// # Arguments
/// * `state` - Application state
/// * `server_id` - Server ID for context
/// * `connection_id` - Optional connection ID
/// * `event_type_id` - Event type identifier (e.g., "tcp_data_received")
/// * `event_description` - Human-readable event description
/// * `event_data` - Structured event data for scripts
/// * `protocol` - Optional protocol for action execution
///
/// # Returns
/// * `Ok(EventHandlerResult::Handled(...))` - Handler executed successfully
/// * `Ok(EventHandlerResult::FallbackToLlm { instruction: None })` - No handler or fallback requested
/// * `Err(_)` - Handler execution failed critically
pub async fn try_execute_event_handler(
    state: &AppState,
    server_id: ServerId,
    connection_id: Option<crate::server::connection::ConnectionId>,
    event_type_id: &str,
    event_description: &str,
    event_data: Option<serde_json::Value>,
    protocol: Option<&dyn Server>,
) -> Result<EventHandlerResult> {
    // Get event handler configuration
    let event_handler_config = state.get_event_handler_config(server_id).await;

    let Some(config) = event_handler_config else {
        // No event handler configuration - use LLM
        return Ok(EventHandlerResult::FallbackToLlm { instruction: None });
    };

    // Find matching handler for this event type
    let Some(handler_type) = config.find_handler(event_type_id) else {
        // No matching handler - use LLM
        debug!("No handler matches event '{}', using LLM", event_type_id);
        return Ok(EventHandlerResult::FallbackToLlm { instruction: None });
    };

    match handler_type {
        EventHandlerType::Llm { instruction } => {
            // LLM handler explicitly configured with instruction: hand it back to the
            // caller, which adds it to this event's prompt.
            debug!(
                "LLM handler configured for event '{}' with instruction: {}",
                event_type_id, instruction
            );
            Ok(EventHandlerResult::FallbackToLlm {
                instruction: Some(instruction.to_string()),
            })
        }

        EventHandlerType::Script {
            language,
            code,
            resident,
            scope,
        } => {
            // Execute script handler
            execute_script_handler(
                state,
                server_id,
                connection_id,
                event_type_id,
                event_description,
                event_data,
                language,
                code,
                *resident,
                scope.as_deref(),
                protocol,
            )
            .await
        }

        EventHandlerType::Static { actions } => {
            // Execute static handler (may interpolate {{event.field}} from event_data)
            execute_static_handler(
                state,
                server_id,
                event_type_id,
                event_description,
                actions,
                event_data.as_ref(),
                protocol,
            )
            .await
        }

        EventHandlerType::Manual { timeout_secs } => {
            let actions = await_manual_answer(
                state,
                crate::state::intercepts::InterceptOwner::Server(server_id),
                connection_id.map(|c| c.as_u32()),
                event_type_id,
                event_description,
                event_data.clone(),
                *timeout_secs,
            )
            .await?;
            // The operator's actions run exactly as a static handler's would —
            // same interpolation, same executor — so "manual" is a static
            // response composed at answer time rather than configured up front.
            execute_static_handler(
                state,
                server_id,
                event_type_id,
                event_description,
                &actions,
                event_data.as_ref(),
                protocol,
            )
            .await
        }
    }
}

/// Park an event for the operator and wait for their actions.
///
/// Shared by the server and client dispatchers. On timeout, dismissal, or a
/// dead channel this returns `Err`, which the caller propagates — landing on
/// the same fail-closed path as an LLM failure, so the peer gets a category
/// error (`crate::utils::wire_failure`), never silence and never an invented
/// success.
async fn await_manual_answer(
    state: &AppState,
    owner: crate::state::intercepts::InterceptOwner,
    connection_id: Option<u32>,
    event_type_id: &str,
    event_description: &str,
    event_data: Option<serde_json::Value>,
    timeout_secs: u64,
) -> Result<Vec<serde_json::Value>> {
    let (id, reply_rx) = state
        .park_intercept(
            owner,
            connection_id,
            event_type_id,
            event_description,
            event_data,
            timeout_secs,
        )
        .await;
    tracing::info!(
        "manual handler: event '{}' parked as intercept #{} — waiting up to {}s for an \
         operator answer at the dashboard",
        event_type_id,
        id,
        timeout_secs
    );

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), reply_rx).await {
        Ok(Ok(actions)) => {
            debug!(
                "manual handler: intercept #{} answered with {} action(s)",
                id,
                actions.len()
            );
            Ok(actions)
        }
        Ok(Err(_)) => {
            // Sender dropped: the operator dismissed it (fail closed, now).
            anyhow::bail!(
                "manual handler: the operator dismissed request #{id} for event '{event_type_id}'"
            )
        }
        Err(_) => {
            state.remove_intercept(id).await;
            warn!(
                "manual handler: intercept #{} for event '{}' got no operator answer within \
                 {}s — failing closed",
                id, event_type_id, timeout_secs
            );
            anyhow::bail!(
                "manual handler: no operator answer within {timeout_secs}s for event \
                 '{event_type_id}'"
            )
        }
    }
}

/// Result from checking a **client's** event handlers.
///
/// Unlike the server variant, `Handled` carries the raw action JSON rather than
/// an `ExecutionResult`: for servers, handler actions are executed centrally
/// via `execute_actions` + the `Server` trait, but a client's protocol actions
/// can only be executed by its own connection loop, which owns the socket. The
/// caller (`client::llm_budget::call_llm_for_client`) returns these actions to
/// the loop exactly as it returns LLM-produced ones, so the loop cannot tell —
/// and need not care — which source answered.
pub enum ClientEventHandlerResult {
    /// Handler produced actions for the client's loop to execute.
    Handled { actions: Vec<serde_json::Value> },
    /// No handler configured or handler requested LLM fallback. `instruction`
    /// is `Some` only for an explicit `{"type":"llm","instruction":"…"}`
    /// handler; the caller must add it to the prompt for this event.
    FallbackToLlm { instruction: Option<String> },
}

/// Check and execute the configured event handler for a **client** event.
///
/// The client mirror of [`try_execute_event_handler`]. Reads the client's own
/// `event_handler_config` (stored since forever, dispatched from nowhere until
/// now), matches first-match-wins, and:
/// - `Static` — interpolates `{{event.field}}` references and returns the
///   rendered actions. An unresolvable reference is a hard `Err`, same as the
///   server path.
/// - `Script` — runs the script with a client-shaped `ScriptInput` (`client`
///   set, `server` absent) and returns its actions. A script failure falls
///   back to the LLM, same as the server path.
/// - `Llm { instruction }` — falls back with the per-event instruction.
pub async fn try_execute_client_event_handler(
    state: &AppState,
    client_id: crate::state::ClientId,
    event_type_id: &str,
    event_description: &str,
    event_data: Option<serde_json::Value>,
) -> Result<ClientEventHandlerResult> {
    let Some(config) = state.get_client_event_handler_config(client_id).await else {
        return Ok(ClientEventHandlerResult::FallbackToLlm { instruction: None });
    };

    let Some(handler_type) = config.find_handler(event_type_id) else {
        debug!(
            "No client handler matches event '{}', using LLM",
            event_type_id
        );
        return Ok(ClientEventHandlerResult::FallbackToLlm { instruction: None });
    };

    match handler_type {
        EventHandlerType::Llm { instruction } => Ok(ClientEventHandlerResult::FallbackToLlm {
            instruction: Some(instruction.to_string()),
        }),

        EventHandlerType::Static { actions } => {
            debug!(
                "Static client handler executing for event '{}' ({} actions)",
                event_type_id,
                actions.len()
            );
            let actions =
                crate::scripting::event_handler::interpolate_actions(actions, event_data.as_ref())
                    .with_context(|| {
                        format!(
                            "Static handler for client event '{}' could not be rendered",
                            event_type_id
                        )
                    })?;
            Ok(ClientEventHandlerResult::Handled { actions })
        }

        EventHandlerType::Script {
            language,
            code,
            resident,
            scope,
        } => {
            execute_client_script_handler(
                state,
                client_id,
                event_type_id,
                event_description,
                event_data,
                language,
                code,
                *resident,
                scope.as_deref(),
            )
            .await
        }

        EventHandlerType::Manual { timeout_secs } => {
            let actions = await_manual_answer(
                state,
                crate::state::intercepts::InterceptOwner::Client(client_id),
                None,
                event_type_id,
                event_description,
                event_data.clone(),
                *timeout_secs,
            )
            .await?;
            // Interpolate exactly as a static handler would, then hand the
            // actions to the client's own loop — only it owns the socket.
            let actions =
                crate::scripting::event_handler::interpolate_actions(&actions, event_data.as_ref())
                    .with_context(|| {
                        format!(
                            "Manual answer for client event '{}' could not be rendered",
                            event_type_id
                        )
                    })?;
            Ok(ClientEventHandlerResult::Handled { actions })
        }
    }
}

/// Execute a script handler in client context, returning its actions for the
/// client's loop.
#[allow(clippy::too_many_arguments)]
async fn execute_client_script_handler(
    state: &AppState,
    client_id: crate::state::ClientId,
    event_type_id: &str,
    event_description: &str,
    event_data: Option<serde_json::Value>,
    language: &str,
    code: &str,
    resident: bool,
    scope: Option<&str>,
) -> Result<ClientEventHandlerResult> {
    let Some(client) = state.get_client(client_id).await else {
        warn!(
            "Client #{} not found for script execution",
            client_id.as_u32()
        );
        return Ok(ClientEventHandlerResult::FallbackToLlm { instruction: None });
    };

    let event_json =
        event_data.unwrap_or_else(|| serde_json::json!({"description": event_description}));

    let script_input = crate::scripting::types::ScriptInput {
        event_type_id: event_type_id.to_string(),
        server: None,
        client: Some(crate::scripting::types::ClientContext {
            id: client.id.as_u32(),
            remote_addr: client.remote_addr.clone(),
            protocol: client.protocol_name.clone(),
            memory: client.memory.clone(),
            instruction: client.instruction.clone(),
        }),
        connection: None,
        event: event_json,
    };

    let script_language = match language.to_lowercase().as_str() {
        "python" => crate::scripting::ScriptLanguage::Python,
        "javascript" | "js" => crate::scripting::ScriptLanguage::JavaScript,
        "go" => crate::scripting::ScriptLanguage::Go,
        "perl" => crate::scripting::ScriptLanguage::Perl,
        _ => {
            warn!(
                "Unknown script language '{}', falling back to LLM",
                language
            );
            return Ok(ClientEventHandlerResult::FallbackToLlm { instruction: None });
        }
    };

    let scripting_env = state.get_scripting_env().await;
    if !scripting_env.is_available(script_language) {
        warn!(
            "Script language {} not available, falling back to LLM",
            script_language.as_str()
        );
        return Ok(ClientEventHandlerResult::FallbackToLlm { instruction: None });
    }

    let script_config = crate::scripting::types::ScriptConfig {
        language: script_language,
        source: crate::scripting::types::ScriptSource::Inline(code.to_string()),
        handles_contexts: vec![event_type_id.to_string()],
    };

    let use_resident =
        resident && crate::scripting::resident::resident_language_supported(script_language);
    if resident && !use_resident {
        warn!(
            "Resident mode requested for '{}' but that language runs per-event only; \
             falling back to per-event execution",
            script_language.as_str()
        );
    }

    let script_result = if use_resident {
        let resident_scope = crate::scripting::ResidentScope::parse(scope);
        crate::scripting::ResidentScriptManager::dispatch(
            &script_config,
            &script_input,
            resident_scope,
        )
        .await
    } else {
        crate::scripting::executor::execute_script_async(&script_config, &script_input).await
    };

    match script_result {
        Ok(response) => {
            debug!(
                "Script handled client event '{}' ({} actions)",
                event_type_id,
                response.actions.len()
            );
            Ok(ClientEventHandlerResult::Handled {
                actions: response.actions,
            })
        }
        Err(e) => {
            warn!("Client script execution failed: {}, falling back to LLM", e);
            Ok(ClientEventHandlerResult::FallbackToLlm { instruction: None })
        }
    }
}

/// Execute a script handler
#[allow(clippy::too_many_arguments)]
async fn execute_script_handler(
    state: &AppState,
    server_id: ServerId,
    connection_id: Option<crate::server::connection::ConnectionId>,
    event_type_id: &str,
    event_description: &str,
    event_data: Option<serde_json::Value>,
    language: &str,
    code: &str,
    resident: bool,
    scope: Option<&str>,
    protocol: Option<&dyn Server>,
) -> Result<EventHandlerResult> {
    // Get server info to build script input
    let server_info = state.get_server(server_id).await;

    let Some(server) = server_info else {
        warn!(
            "Server #{} not found for script execution",
            server_id.as_u32()
        );
        return Ok(EventHandlerResult::FallbackToLlm { instruction: None });
    };

    // Build connection context if available
    let connection_context = if let Some(conn_id) = connection_id {
        server.connections.get(&conn_id).map(|conn_state| {
            crate::scripting::types::ConnectionContext {
                id: conn_id.to_string(),
                remote_addr: conn_state.remote_addr.to_string(),
                bytes_received: conn_state.bytes_received,
                bytes_sent: conn_state.bytes_sent,
            }
        })
    } else {
        None
    };

    // Build structured input for script
    let event_json =
        event_data.unwrap_or_else(|| serde_json::json!({"description": event_description}));

    let script_input = crate::scripting::types::ScriptInput {
        event_type_id: event_type_id.to_string(),
        client: None,
        server: Some(crate::scripting::types::ServerContext {
            id: server.id.as_u32(),
            port: server.port,
            stack: crate::protocol::server_registry::registry()
                .stack_name_by_protocol(&server.protocol_name)
                .unwrap_or("UNKNOWN")
                .to_string(),
            memory: server.memory.clone(),
            instruction: server.instruction.clone(),
        }),
        connection: connection_context,
        event: event_json,
    };

    // Parse language
    let script_language = match language.to_lowercase().as_str() {
        "python" => crate::scripting::ScriptLanguage::Python,
        "javascript" | "js" => crate::scripting::ScriptLanguage::JavaScript,
        "go" => crate::scripting::ScriptLanguage::Go,
        "perl" => crate::scripting::ScriptLanguage::Perl,
        _ => {
            warn!(
                "Unknown script language '{}', falling back to LLM",
                language
            );
            return Ok(EventHandlerResult::FallbackToLlm { instruction: None });
        }
    };

    // Check if language is available
    let scripting_env = state.get_scripting_env().await;
    if !scripting_env.is_available(script_language) {
        warn!(
            "Script language {} not available, falling back to LLM",
            script_language.as_str()
        );
        return Ok(EventHandlerResult::FallbackToLlm { instruction: None });
    }

    // Build ScriptConfig for execution
    let script_config = crate::scripting::types::ScriptConfig {
        language: script_language,
        source: crate::scripting::types::ScriptSource::Inline(code.to_string()),
        handles_contexts: vec![event_type_id.to_string()],
    };

    // Choose resident (persistent) or per-event execution.
    //
    // Resident mode is opt-in and keeps one interpreter process alive per scope
    // so in-process state survives between events. A resident handler for a
    // language that has no persistent form (Go) transparently falls back to the
    // per-event path so the request is still handled.
    let use_resident =
        resident && crate::scripting::resident::resident_language_supported(script_language);
    if resident && !use_resident {
        warn!(
            "Resident mode requested for '{}' but that language runs per-event only; \
             falling back to per-event execution",
            script_language.as_str()
        );
    }

    let script_result = if use_resident {
        let resident_scope = crate::scripting::ResidentScope::parse(scope);
        crate::scripting::ResidentScriptManager::dispatch(
            &script_config,
            &script_input,
            resident_scope,
        )
        .await
    } else {
        crate::scripting::executor::execute_script_async(&script_config, &script_input).await
    };

    // Execute the script
    match script_result {
        Ok(response) => {
            debug!(
                "Script handled event '{}' ({} actions)",
                event_type_id,
                response.actions.len()
            );

            // Register SCRIPT conversation for tracking
            let truncated_desc = format!(
                "SCRIPT \"{}\"",
                crate::utils::truncate_for_log(event_description, 27)
            );
            let conv_id = format!(
                "script-{}-{:x}",
                crate::utils::clock::SystemTime::now()
                    .duration_since(crate::utils::clock::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
                rand::random::<u32>()
            );
            state
                .register_conversation(
                    conv_id.clone(),
                    crate::state::app_state::ConversationSource::Network {
                        server_id,
                        connection_id,
                    },
                    truncated_desc,
                )
                .await;

            // Execute the script's actions with server context
            let result = execute_actions(response.actions, state, protocol, Some(server_id), None)
                .await
                .context("Failed to execute script actions")?;

            // End conversation tracking
            state.end_conversation(&conv_id).await;

            Ok(EventHandlerResult::Handled(result))
        }
        Err(e) => {
            warn!("Script execution failed: {}, falling back to LLM", e);
            Ok(EventHandlerResult::FallbackToLlm { instruction: None })
        }
    }
}

/// Execute a static handler
///
/// Before dispatch, every `{{event.field}}` reference in the configured actions is
/// replaced with the matching value from `event_data`. A whole-string reference keeps the
/// referenced value's JSON type, which is what makes static mode usable for protocols that
/// must echo a correlation id (DNS `query_id`, DHCP `xid`, SNMP `request-id`, STUN
/// transaction id). See `crate::scripting::event_handler` for the full syntax.
///
/// Actions with no references are passed through untouched. An unresolvable reference is a
/// hard error rather than a silent `null`, so a typo'd field name cannot look like it works.
#[allow(clippy::too_many_arguments)]
async fn execute_static_handler(
    state: &AppState,
    server_id: ServerId,
    event_type_id: &str,
    event_description: &str,
    actions: &[serde_json::Value],
    event_data: Option<&serde_json::Value>,
    protocol: Option<&dyn Server>,
) -> Result<EventHandlerResult> {
    debug!(
        "Static handler executing for event '{}' ({} actions)",
        event_type_id,
        actions.len()
    );

    // Substitute {{event.…}} references from the triggering event
    let actions = match crate::scripting::event_handler::interpolate_actions(actions, event_data) {
        Ok(actions) => actions,
        Err(e) => {
            error!(
                "Static handler for event '{}' has an unresolvable reference: {}",
                event_type_id, e
            );
            return Err(e).with_context(|| {
                format!(
                    "Static handler for event '{}' could not be rendered",
                    event_type_id
                )
            });
        }
    };

    // Execute the static actions with server context
    let result = execute_actions(actions, state, protocol, Some(server_id), None)
        .await
        .context("Failed to execute static actions")?;

    // Log as STATIC interaction (no conversation tracking needed for static responses)
    debug!("Static handler completed: {}", event_description);

    Ok(EventHandlerResult::Handled(result))
}
