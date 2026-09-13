//! Regression tests for the scheduled-task LLM action list.
//!
//! `execute_single_task` (src/cli/tasks.rs) used to hand an empty
//! `Vec<ActionDefinition>` to `ConversationHandler`, while
//! `PromptBuilder::build_task_execution_prompt` advertised a real action list to the model.
//! `ConversationHandler` derives `valid_action_names` from the list it is given and has no
//! bypass for an empty list, so *every* action a scheduled task returned was flagged as an
//! unknown action, retried twice, and the call then `bail!`d — no scheduled task could ever
//! execute an action.
//!
//! These tests pin the invariant that makes the fix correct: the set of actions advertised
//! in the task prompt and the set validated against must be identical.

use netget::llm::actions::{
    get_all_tool_actions, get_network_event_common_actions, get_network_event_tool_actions,
    get_user_input_common_actions, ActionDefinition, ToolAction,
};
use netget::llm::PromptBuilder;
use netget::state::app_state::{AppState, ScriptingMode};
use netget::state::server::{ServerInstance, ServerStatus};
use netget::state::{ScheduledTask, ServerId, TaskId, TaskScope};
use std::collections::BTreeSet;

/// Replica of the unknown-action check in `ConversationHandler::generate_with_tools_and_retry`
/// (src/llm/conversation.rs:504-594): `valid_action_names` is built from the action list the
/// handler was constructed with, and anything not in it is reported as an unknown action.
fn unknown_actions(
    available_actions: &[ActionDefinition],
    model_actions: &[serde_json::Value],
) -> Vec<String> {
    let valid_action_names: BTreeSet<String> =
        available_actions.iter().map(|a| a.name.clone()).collect();

    model_actions
        .iter()
        .filter_map(|action| {
            let action_type = action.get("type").and_then(|v| v.as_str())?;
            if ToolAction::is_tool_action(action) {
                return None;
            }
            if !valid_action_names.contains(action_type) {
                Some(action_type.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Extract the action names the prompt advertises to the model.
///
/// `build_action_prompt` renders both the "Available Tools" and "Available Actions"
/// sections as numbered headings: `## <n>. <action_name>`.
fn advertised_action_names(prompt: &str) -> BTreeSet<String> {
    prompt
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("## ")?;
            let (index, name) = rest.split_once(". ")?;
            // Only numbered headings are action entries; "## Understanding Memory" etc. are not.
            if index.parse::<u32>().is_err() {
                return None;
            }
            let name = name.trim();
            if name.is_empty() || name.contains(' ') {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

fn names(actions: &[ActionDefinition]) -> BTreeSet<String> {
    actions.iter().map(|a| a.name.clone()).collect()
}

async fn test_state() -> AppState {
    let state = AppState::new();
    state
        .set_scripting_env(netget::scripting::ScriptingEnvironment {
            python: None,
            javascript: None,
            go: None,
            perl: None,
        })
        .await;
    state.set_selected_scripting_mode(ScriptingMode::Off).await;
    state
}

/// Server-scoped task: the prompt is built from
/// `get_network_event_common_actions() + protocol_actions + get_network_event_tool_actions()`.
/// The validator must receive exactly that.
#[tokio::test]
async fn test_server_scoped_task_actions_match_prompt() {
    let state = test_state().await;

    let mut server = ServerInstance::new(
        ServerId::new(1),
        9999,
        "Tcp".to_string(),
        "Echo everything".to_string(),
    );
    server.status = ServerStatus::Running;
    let server_id = state.add_server(server).await;

    let task = ScheduledTask::new_recurring(
        TaskId::new(1),
        "heartbeat".to_string(),
        TaskScope::Server(server_id),
        60,
        None,
        "Log a heartbeat message".to_string(),
        None,
    );

    // No protocol actions: keeps the assertion independent of which protocol features are on.
    let protocol_actions: Vec<ActionDefinition> = Vec::new();

    let prompt =
        PromptBuilder::build_task_execution_prompt(&state, &task, protocol_actions.clone()).await;

    // Same composition the fixed `build_task_actions` uses for network-scoped tasks.
    // (`filter_actions_by_scripting_mode` is a no-op here: none of these are script actions.)
    let mut task_actions = get_network_event_common_actions();
    task_actions.extend(protocol_actions);
    task_actions.extend(get_network_event_tool_actions(
        state.get_web_search_mode().await,
    ));

    assert!(
        !task_actions.is_empty(),
        "scheduled task must be given a non-empty action list"
    );
    assert_eq!(
        names(&task_actions),
        advertised_action_names(&prompt),
        "actions validated against must be identical to actions advertised in the task prompt"
    );

    // A plausible model response for this task.
    let model_actions = vec![
        serde_json::json!({"type": "show_message", "message": "heartbeat"}),
        serde_json::json!({"type": "append_to_log", "file": "hb.log", "content": "tick"}),
        serde_json::json!({"type": "set_memory", "value": "beats: 1"}),
    ];

    // Before the fix: an empty list meant every action was unknown -> two retries -> bail!.
    assert_eq!(
        unknown_actions(&[], &model_actions),
        vec![
            "show_message".to_string(),
            "append_to_log".to_string(),
            "set_memory".to_string()
        ],
        "sanity check: conversation.rs has no empty-list bypass"
    );

    // After the fix: accepted.
    assert!(
        unknown_actions(&task_actions, &model_actions).is_empty(),
        "actions returned by the model in the scheduled-task path must be accepted"
    );
}

/// Global-scoped task: the prompt is built from
/// `get_user_input_common_actions(..) + get_all_tool_actions(..)`, then scripting-filtered.
#[tokio::test]
async fn test_global_task_actions_match_prompt() {
    let state = test_state().await;

    let task = ScheduledTask::new_one_shot(
        TaskId::new(2),
        "startup".to_string(),
        TaskScope::Global,
        5,
        "Open a TCP server on an available port".to_string(),
        None,
    );

    let prompt = PromptBuilder::build_task_execution_prompt(&state, &task, Vec::new()).await;

    let selected_mode = state.get_selected_scripting_mode().await;
    let scripting_env = state.get_scripting_env().await;
    let mut task_actions = get_user_input_common_actions(selected_mode, &scripting_env, true, true);
    task_actions.extend(get_all_tool_actions(state.get_web_search_mode().await));

    // Mirror of `PromptBuilder::filter_actions_by_scripting_mode` at name level: with
    // scripting off, `update_script` is dropped (it only strips parameters elsewhere).
    // The production fix calls that filter directly; it is pub(crate) so unreachable here.
    if selected_mode == ScriptingMode::Off {
        task_actions.retain(|a| a.name != "update_script");
    }

    assert!(!task_actions.is_empty());
    assert_eq!(
        names(&task_actions),
        advertised_action_names(&prompt),
        "global task: validated actions must equal advertised actions"
    );

    let model_actions = vec![
        serde_json::json!({"type": "open_server", "protocol": "Tcp", "port": 9000, "instruction": "echo"}),
        serde_json::json!({"type": "show_message", "message": "server opened"}),
    ];

    assert!(!unknown_actions(&[], &model_actions).is_empty());
    assert!(
        unknown_actions(&task_actions, &model_actions).is_empty(),
        "global scheduled task must be able to invoke open_server/show_message"
    );
}
