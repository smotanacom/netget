//! Draining the feedback buffer — the other half of `provide_feedback`.
//!
//! `provide_feedback` accumulates entries on the owning server or client
//! (`AppState::add_server_feedback` / `add_client_feedback`) and
//! `call_llm_for_feedback` knows how to turn a batch of them into adjustment actions.
//! Nothing connected the two: the buffer was written and never read, and
//! `call_llm_for_feedback` had no callers at all. The tool was advertised to the model
//! whenever `feedback_instructions` was set, the model could call it, and the result went
//! nowhere — which is worse than not offering the tool, because the model has no way to
//! discover that its report was discarded.
//!
//! This module is the missing drain. It is polled from the same 1-second timer that runs
//! due scheduled tasks (`execute_due_tasks`), because a feedback batch is the same shape of
//! work: timer-driven, LLM-answered, and it adjusts a running instance.
//!
//! Coverage follows that timer exactly — the dashboard (`tui::event_loop`) and non-interactive mode
//! drive it; MCP mode does not run a task timer at all, so neither scheduled tasks nor
//! feedback fire there. That is a pre-existing MCP gap, not a property of this module.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::llm::OllamaClient;
use crate::state::app_state::DueFeedback;
use crate::state::AppState;

/// How long an instance is left alone after a feedback batch is taken.
///
/// Leading edge: the first entry after a quiet period is processed at the next tick
/// (within a second), and everything that arrives during a burst is coalesced into the
/// next batch rather than producing an LLM call per entry. A busy server that reports
/// feedback on every request therefore costs at most one model round-trip per window.
pub const FEEDBACK_DEBOUNCE: Duration = Duration::from_secs(30);

/// Process every instance whose accumulated feedback is due.
///
/// Each batch is handled in its own task so one slow model call cannot hold up the timer
/// or another instance's feedback.
pub async fn execute_due_feedback(
    state: &AppState,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
) {
    let due = state.take_due_feedback(FEEDBACK_DEBOUNCE).await;
    if due.is_empty() {
        return;
    }

    for batch in due {
        let state = state.clone();
        let llm_client = llm_client.clone();
        let status_tx = status_tx.clone();
        tokio::spawn(async move {
            process_feedback_batch(state, llm_client, status_tx, batch).await;
        });
    }
}

/// Label an instance for logs: `server #3` / `client #1`.
fn describe(batch: &DueFeedback) -> String {
    match (batch.server_id, batch.client_id) {
        (Some(sid), _) => format!("server #{}", sid.as_u32()),
        (_, Some(cid)) => format!("client #{}", cid.as_u32()),
        _ => "unknown instance".to_string(),
    }
}

/// Run one batch: ask the model what to adjust, then apply it.
async fn process_feedback_batch(
    state: AppState,
    llm_client: OllamaClient,
    status_tx: mpsc::UnboundedSender<String>,
    batch: DueFeedback,
) {
    let who = describe(&batch);
    info!(
        "Processing {} feedback entries for {}",
        batch.entries.len(),
        who
    );
    let _ = status_tx.send(format!(
        "[INFO] Processing {} feedback entries for {}",
        batch.entries.len(),
        who
    ));

    let actions = match crate::llm::action_helper::call_llm_for_feedback(
        &llm_client,
        &state,
        batch.server_id,
        batch.client_id,
        &batch.instructions,
        &batch.instruction,
        &batch.memory,
        &batch.entries,
        &status_tx,
    )
    .await
    {
        Ok(actions) => actions,
        Err(e) => {
            // The batch is gone: it was taken out of the buffer under the same lock that
            // stamped the debounce timestamp. Re-queueing it would let one payload the
            // model cannot answer retry forever, so it is dropped — loudly. Feedback is a
            // best-effort signal channel, not a durable queue, and nothing on the wire is
            // waiting on this call.
            error!(
                "Feedback processing failed for {} ({} entries dropped): {}",
                who,
                batch.entries.len(),
                e
            );
            let _ = status_tx.send(format!(
                "[ERROR] Feedback processing failed for {} ({} entries dropped): {}",
                who,
                batch.entries.len(),
                e
            ));
            return;
        }
    };

    if actions.is_empty() {
        debug!("Feedback for {} produced no adjustments", who);
        return;
    }

    apply_feedback_actions(&state, &llm_client, &status_tx, &batch, actions).await;
}

/// Where a single adjustment action has to be executed.
enum Route {
    /// Instruction updates: retargeted onto the instance the feedback came from
    Instruction(String),
    /// Needs `EventHandler` (spawns/stops servers and clients, changes the model)
    Handler(crate::llm::CommonAction),
    /// Executable by `llm::execute_actions` with the instance ids attached
    Passthrough(serde_json::Value),
}

/// Decide how one action must be applied.
///
/// `update_instruction` gets special treatment. `EventHandler` applies it to
/// `get_first_server_id()`, which is right for a user typing at the TUI and wrong here: a
/// feedback batch belongs to one known instance, and retargeting it at server #1 would let
/// feedback about server #3 silently rewrite a different server's instruction. Everything
/// else is routed by whether the plain executor can actually perform it — see the
/// "must be handled by caller" arms in `llm::actions::executor`.
fn route(action: serde_json::Value) -> Route {
    use crate::llm::CommonAction;

    match CommonAction::from_json(&action) {
        Ok(CommonAction::UpdateInstruction { instruction }) => Route::Instruction(instruction),
        Ok(CommonAction::UpdateClientInstruction { instruction, .. }) => {
            Route::Instruction(instruction)
        }
        Ok(
            common @ (CommonAction::OpenServer { .. }
            | CommonAction::CloseServer { .. }
            | CommonAction::CloseAllServers
            | CommonAction::ChangeModel { .. }
            | CommonAction::ScheduleTask { .. }
            | CommonAction::CancelTask { .. }
            | CommonAction::OpenClient { .. }
            | CommonAction::CloseClient { .. }
            | CommonAction::CloseAllClients
            | CommonAction::CloseConnectionById { .. }
            | CommonAction::ReconnectClient { .. }
            | CommonAction::ShowMessage { .. }),
        ) => Route::Handler(common),
        _ => Route::Passthrough(action),
    }
}

/// Apply the adjustment actions the model returned.
///
/// The model answers with the *user-input* action vocabulary (that is what
/// `call_llm_for_feedback` advertises), and much of that vocabulary — `open_server`,
/// `close_server`, `change_model` — is deliberately not executable by
/// `llm::execute_actions`, which logs "must be handled by caller" and moves on. Those go
/// through `EventHandler::execute_server_management_action`, as the interactive path does.
/// Everything else goes to `execute_actions` *with the instance ids attached*, so memory
/// and log actions land on the instance the feedback came from rather than on whichever
/// server happens to be first.
async fn apply_feedback_actions(
    state: &AppState,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
    batch: &DueFeedback,
    actions: Vec<serde_json::Value>,
) {
    let who = describe(batch);
    let mut management = Vec::new();
    let mut passthrough = Vec::new();

    for action in actions {
        match route(action) {
            Route::Instruction(instruction) => {
                if let Some(sid) = batch.server_id {
                    state.set_instruction(sid, instruction.clone()).await;
                } else if let Some(cid) = batch.client_id {
                    state
                        .set_instruction_for_client(cid, instruction.clone())
                        .await;
                }
                info!("Feedback updated the instruction for {}", who);
                let _ = status_tx.send(format!(
                    "[INFO] Feedback updated the instruction for {}: {}",
                    who,
                    crate::utils::truncate_for_log(&instruction, 120)
                ));
            }
            Route::Handler(common) => management.push(common),
            Route::Passthrough(value) => passthrough.push(value),
        }
    }

    if !management.is_empty() {
        let mut handler = crate::events::EventHandler::new(state.clone(), llm_client.clone());
        for action in management {
            if let Err(e) = handler
                .execute_server_management_action(action, status_tx)
                .await
            {
                error!("Feedback adjustment failed for {}: {}", who, e);
                let _ = status_tx.send(format!(
                    "[ERROR] Feedback adjustment failed for {}: {}",
                    who, e
                ));
            }
        }
    }

    if passthrough.is_empty() {
        return;
    }

    match crate::llm::execute_actions(passthrough, state, None, batch.server_id, batch.client_id)
        .await
    {
        Ok(result) => {
            let failures = result.failure_summary();
            for msg in result.messages {
                let _ = status_tx.send(msg);
            }
            if let Some(summary) = failures {
                warn!("Feedback adjustment for {} partly failed: {}", who, summary);
                let _ = status_tx.send(format!(
                    "[WARN] Feedback adjustment for {} partly failed: {}",
                    who, summary
                ));
            }
        }
        Err(e) => {
            error!("Failed to apply feedback adjustments for {}: {}", who, e);
            let _ = status_tx.send(format!(
                "[ERROR] Failed to apply feedback adjustments for {}: {}",
                who, e
            ));
        }
    }
}
