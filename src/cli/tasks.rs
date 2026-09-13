//! Scheduled-task execution: the 1s tick every interactive and non-interactive
//! mode runs, which fires each due task through the model with the action
//! vocabulary its scope allows.

use tokio::sync::mpsc;

use crate::llm::OllamaClient;
use crate::state::app_state::AppState;

/// Execute all tasks that are due (public wrapper for non-interactive mode)
pub async fn execute_due_tasks_public(
    state: &AppState,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    execute_due_tasks(state, llm_client, status_tx).await
}

/// Execute all tasks that are due
async fn execute_due_tasks(
    state: &AppState,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    use crate::state::task::TaskStatus;
    use std::time::Instant;

    let now = Instant::now();
    let tasks = state.get_all_tasks().await;

    for task in tasks {
        // Skip if not scheduled or not yet due
        if task.status != TaskStatus::Scheduled {
            continue;
        }

        if task.next_execution > now {
            continue;
        }

        // Mark as executing
        state
            .update_task_status(task.id, TaskStatus::Executing)
            .await;

        // Spawn task execution to avoid blocking
        let state_clone = state.clone();
        let llm_clone = llm_client.clone();
        let status_tx_clone = status_tx.clone();
        let task_clone = task.clone();

        tokio::spawn(async move {
            execute_single_task(state_clone, llm_clone, status_tx_clone, task_clone).await
        });
    }
}

/// Build the list of actions a scheduled task may invoke.
///
/// This mirrors, scope for scope, the action list that
/// `PromptBuilder::build_task_execution_prompt` advertises to the model, and then applies
/// the same scripting-mode filter `PromptBuilder::build_action_prompt` applies. The result
/// is that the set the model is told about and the set `ConversationHandler` validates
/// against are identical rather than merely overlapping.
///
/// Previously an empty `Vec` was handed to the validator, so every action a scheduled task
/// returned was flagged unknown, retried twice, then `bail!`d — no scheduled task could
/// ever execute an action.
async fn build_task_actions(
    state: &AppState,
    scope: &crate::state::task::TaskScope,
    protocol_actions: Vec<crate::llm::actions::ActionDefinition>,
) -> Vec<crate::llm::actions::ActionDefinition> {
    use crate::llm::actions::{
        get_all_tool_actions, get_network_event_common_actions, get_network_event_tool_actions,
        get_user_input_common_actions,
    };
    use crate::llm::prompt::PromptBuilder;
    use crate::state::task::TaskScope;

    let selected_mode = state.get_selected_scripting_mode().await;
    let web_search_mode = state.get_web_search_mode().await;

    let actions = match scope {
        TaskScope::Global => {
            // Global tasks run in the user-input context, with open_server/open_client enabled.
            let scripting_env = state.get_scripting_env().await;
            let mut actions =
                get_user_input_common_actions(selected_mode, &scripting_env, true, true);
            actions.extend(get_all_tool_actions(web_search_mode));
            actions
        }
        TaskScope::Server(_) | TaskScope::Connection(_, _) | TaskScope::Client(_) => {
            // Server-, connection- and client-scoped tasks run in the network-event context.
            let mut actions = get_network_event_common_actions();
            actions.extend(protocol_actions);
            actions.extend(get_network_event_tool_actions(web_search_mode));
            actions
        }
    };

    let has_scripting = selected_mode != crate::state::app_state::ScriptingMode::Off;
    PromptBuilder::filter_actions_by_scripting_mode(actions, has_scripting)
}

/// Execute a single task
async fn execute_single_task(
    state: AppState,
    llm_client: OllamaClient,
    status_tx: mpsc::UnboundedSender<String>,
    task: crate::state::ScheduledTask,
) {
    use crate::llm::prompt::PromptBuilder;
    use crate::state::task::{TaskExecutionResult, TaskScope};

    let _ = status_tx.send(format!("[TASK] Executing task '{}'", task.name));

    // Get protocol actions if server, connection, or client-scoped
    let protocol_actions = match &task.scope {
        TaskScope::Server(server_id) | TaskScope::Connection(server_id, _) => {
            if let Some(protocol_name) = state.get_protocol_name(*server_id).await {
                if let Some(protocol) =
                    crate::protocol::server_registry::registry().get(&protocol_name)
                {
                    protocol.get_sync_actions()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
        TaskScope::Client(client_id) => {
            if let Some(protocol_name) = state.get_protocol_name_for_client(*client_id).await {
                if let Some(protocol) =
                    crate::protocol::client_registry::CLIENT_REGISTRY.get(&protocol_name)
                {
                    // Clients union; servers narrow. A client has one LLM entry point, so it
                    // cannot express an async/sync narrowing and `get_sync_actions()` alone
                    // hides most of its vocabulary - 81 of 99 client protocols, including
                    // `disconnect` on nearly all of them, so a scheduled task could not tell
                    // a client to hang up. This is the same defect `call_llm_for_client` and
                    // `action_catalog_for_pattern` each had; `client_llm_action_set` is the
                    // one place the rule lives.
                    crate::llm::actions::client_trait::client_llm_action_set(
                        protocol.as_ref(),
                        &state,
                        None,
                    )
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
        TaskScope::Global => Vec::new(),
    };

    // Actions genuinely available to this task. This MUST match the set advertised by
    // `PromptBuilder::build_task_execution_prompt` below, because `ConversationHandler`
    // derives `valid_action_names` from it and rejects anything else as an unknown action.
    let task_actions = build_task_actions(&state, &task.scope, protocol_actions.clone()).await;

    // Build prompt
    let prompt = PromptBuilder::build_task_execution_prompt(&state, &task, protocol_actions).await;

    // Get current model, ensuring one is selected
    let model = match crate::llm::ensure_model_selected(state.get_ollama_model().await).await {
        Ok(m) => m,
        Err(e) => {
            let error_msg = format!("Model selection failed: {}", e);
            let _ = status_tx.send(format!(
                "[ERROR] Failed to ensure model is selected for task execution: {}",
                e
            ));
            // Update task status to failed
            state
                .update_task_status(
                    task.id,
                    crate::state::task::TaskStatus::Failed(error_msg.clone()),
                )
                .await;
            let result = TaskExecutionResult {
                success: false,
                actions: Vec::new(),
                error: Some(error_msg),
            };
            state.record_task_execution(task.id, &result).await;
            return;
        }
    };

    // Register task as conversation
    let conversation_source = match &task.scope {
        TaskScope::Global => crate::state::app_state::ConversationSource::Task {
            task_name: task.name.clone(),
        },
        TaskScope::Server(server_id) => crate::state::app_state::ConversationSource::Task {
            task_name: format!("{}#{}", task.name, server_id.as_u32()),
        },
        TaskScope::Connection(server_id, conn_id) => {
            crate::state::app_state::ConversationSource::Task {
                task_name: format!("{}#{}/{}", task.name, server_id.as_u32(), conn_id),
            }
        }
        TaskScope::Client(client_id) => crate::state::app_state::ConversationSource::Task {
            task_name: format!("{}@{}", task.name, client_id.as_u32()),
        },
    };

    let truncated_instruction = crate::utils::truncate_for_log(&task.instruction, 27);

    // Get rate limiter for scheduled tasks (discards if rate limited)
    let rate_limiter = state.get_rate_limiter().await;

    // Create conversation handler with tracking
    let mut conversation = crate::llm::ConversationHandler::new(
        prompt.clone(),
        std::sync::Arc::new(llm_client.clone()),
        model.clone(),
        rate_limiter,
        crate::llm::RequestSource::Network, // Scheduled tasks are discarded if rate limited
    )
    .with_native_tools(&task_actions)
    .with_status_tx(status_tx.clone())
    .with_tracking(state.clone(), conversation_source, truncated_instruction);

    // Add empty user message to trigger generation
    conversation.add_user_message("Execute the task.".to_string());

    // Generate with conversation handler (handles tracking automatically)
    let web_search_mode = state.get_web_search_mode().await;
    let actions = match conversation
        .generate_with_tools_and_retry(
            state.get_web_approval_channel().await,
            web_search_mode,
            task_actions,
        )
        .await
    {
        Ok(actions) => actions,
        Err(e) => {
            // Execution failed
            let error = format!("LLM call failed: {}", e);
            let _ = status_tx.send(format!("[ERROR] Task '{}' failed: {}", task.name, error));

            let result = TaskExecutionResult {
                success: false,
                actions: Vec::new(),
                error: Some(error),
            };

            handle_task_failure(&state, &status_tx, task, result).await;
            return;
        }
    };

    // Get protocol for execution (if server, connection, or client-scoped)
    let protocol = match &task.scope {
        TaskScope::Server(server_id) | TaskScope::Connection(server_id, _) => state
            .get_protocol_name(*server_id)
            .await
            .and_then(|name| crate::protocol::server_registry::registry().get(&name)),
        TaskScope::Client(_client_id) => {
            // Client protocols are handled differently - they don't use the server protocol registry
            // For now, return None as task execution for clients needs client-specific implementation
            None
        }
        TaskScope::Global => None,
    };

    // Extract server_id and client_id from task scope for context
    let (server_id, client_id) = match &task.scope {
        TaskScope::Server(sid) | TaskScope::Connection(sid, _) => (Some(*sid), None),
        TaskScope::Client(cid) => (None, Some(*cid)),
        TaskScope::Global => (None, None),
    };

    // Execute actions with task context
    match crate::llm::execute_actions(
        actions.clone(),
        &state,
        protocol.as_deref(),
        server_id,
        client_id,
    )
    .await
    {
        Ok(_exec_result) => {
            // Success
            let _ = status_tx.send(format!(
                "[TASK] Task '{}' completed successfully",
                task.name
            ));

            let result = TaskExecutionResult {
                success: true,
                actions,
                error: None,
            };

            handle_task_success(&state, &status_tx, task, result).await;
        }
        Err(e) => {
            // Execution failed
            let error = format!("Action execution failed: {}", e);
            let _ = status_tx.send(format!("[ERROR] Task '{}' failed: {}", task.name, error));

            let result = TaskExecutionResult {
                success: false,
                actions,
                error: Some(error),
            };

            handle_task_failure(&state, &status_tx, task, result).await;
        }
    }
}

/// Handle task success
async fn handle_task_success(
    state: &AppState,
    status_tx: &mpsc::UnboundedSender<String>,
    task: crate::state::ScheduledTask,
    result: crate::state::TaskExecutionResult,
) {
    use crate::state::task::{TaskStatus, TaskType};
    use std::time::{Duration, Instant};

    // Record execution
    state.record_task_execution(task.id, &result).await;

    match &task.task_type {
        TaskType::OneShot { .. } => {
            // One-shot task completed
            state
                .update_task_status(task.id, TaskStatus::Completed)
                .await;
            state.remove_task(task.id).await;
            let _ = status_tx.send(format!(
                "[TASK] One-shot task '{}' completed and removed",
                task.name
            ));
        }
        TaskType::Recurring {
            interval_secs,
            max_executions,
            executions_count,
        } => {
            // Check if max executions reached
            if let Some(max) = max_executions {
                if *executions_count >= *max {
                    state
                        .update_task_status(task.id, TaskStatus::Completed)
                        .await;
                    state.remove_task(task.id).await;
                    let _ = status_tx.send(format!(
                        "[TASK] Recurring task '{}' reached max executions ({}) and removed",
                        task.name, max
                    ));
                    return;
                }
            }

            // Schedule next execution
            let next = Instant::now() + Duration::from_secs(*interval_secs);
            state.update_task_next_execution(task.id, next).await;
            state
                .update_task_status(task.id, TaskStatus::Scheduled)
                .await;
        }
    }
}

/// Handle task failure with exponential backoff retry
async fn handle_task_failure(
    state: &AppState,
    status_tx: &mpsc::UnboundedSender<String>,
    task: crate::state::ScheduledTask,
    result: crate::state::TaskExecutionResult,
) {
    use crate::state::task::TaskStatus;
    use std::time::{Duration, Instant};

    const MAX_FAILURES: u64 = 5;
    const BACKOFF_BASE_SECS: u64 = 60; // 1 minute base backoff

    // Record execution
    state.record_task_execution(task.id, &result).await;

    let failure_count = task.failure_count + 1;

    if failure_count >= MAX_FAILURES {
        // Too many failures, disable task
        state
            .update_task_status(
                task.id,
                TaskStatus::Failed(result.error.unwrap_or_else(|| "Unknown error".to_string())),
            )
            .await;
        state.remove_task(task.id).await;
        let _ = status_tx.send(format!(
            "[ERROR] Task '{}' failed {} times, removing from schedule",
            task.name, MAX_FAILURES
        ));
    } else {
        // Retry with exponential backoff
        let backoff_secs = BACKOFF_BASE_SECS * 2u64.pow((failure_count - 1) as u32);
        let next = Instant::now() + Duration::from_secs(backoff_secs);

        state.update_task_next_execution(task.id, next).await;
        state
            .update_task_status(task.id, TaskStatus::Scheduled)
            .await;

        let _ = status_tx.send(format!(
            "[WARN] Task '{}' failed (attempt {}/{}), retrying in {} seconds",
            task.name, failure_count, MAX_FAILURES, backoff_secs
        ));
    }
}
