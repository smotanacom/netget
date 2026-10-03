//! Per-client LLM call budget.
//!
//! Every client protocol drives its own event loop: an event arrives, the loop
//! calls the model, executes the returned actions, and those actions may produce
//! another event. Nothing in that shape guarantees convergence — a handler that
//! always answers "issue another request" loops until the process dies. That is
//! not hypothetical: the DNS client made 211 model calls and then overflowed the
//! stack (`IMPROVEMENTS.md` item 49).
//!
//! Servers are protected by the per-connection Idle/Processing/Accumulating state
//! machine, which prevents re-entrancy. Clients have no equivalent, so this module
//! provides the backstop instead: a hard ceiling on LLM calls per client session,
//! enforced in one place.
//!
//! ## How it is applied
//!
//! [`call_llm_for_client`] here is a drop-in replacement for
//! [`crate::llm::action_helper::call_llm_for_client`] with an identical signature.
//! Client protocols import this one:
//!
//! ```ignore
//! use crate::client::llm_budget::call_llm_for_client;
//! ```
//!
//! Once the budget is exhausted the call returns `Err` with an explanatory message
//! instead of contacting the model, which every existing call site already logs.
//! A protocol that wants to fail louder can match on the error.
//!
//! The cap is `AppState::DEFAULT_CLIENT_LLM_CALL_LIMIT` (100), overridable at
//! runtime via `NETGET_CLIENT_LLM_CALL_LIMIT` (`0` = unlimited) or
//! `AppState::set_client_llm_call_limit()`.

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::llm::actions::client_trait::Client;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::ClientId;

/// Fraction of the budget after which each call emits a warning.
const WARN_AT_PERCENT: u32 = 80;

/// Budget-checked wrapper around [`crate::llm::action_helper::call_llm_for_client`].
///
/// Consumes one unit of the calling client's LLM call budget and fails without
/// contacting the model once that budget is exhausted. See the module docs.
///
/// ## Event-handler routing lives here
///
/// This wrapper is the single choke point every client protocol calls, so the
/// client's configured `event_handlers` (script / static / per-event LLM
/// instruction) are dispatched **here**, before the budget debit — a
/// deterministic handler answers without contacting the model and without
/// consuming budget, which is the budget's whole point (it bounds *model*
/// calls). Anything calling `action_helper::call_llm_for_client` directly
/// bypasses both the budget and this routing.
///
/// Only events route (`event: Some`); the initial-instruction call
/// (`event: None`) has nothing to match a pattern against.
#[allow(clippy::too_many_arguments)]
pub async fn call_llm_for_client(
    llm_client: &OllamaClient,
    state: &AppState,
    client_id: String,
    instruction: &str,
    memory: &str,
    event: Option<&Event>,
    protocol: &dyn Client,
    status_tx: &mpsc::UnboundedSender<String>,
) -> Result<ClientLlmResult> {
    use crate::llm::action_helper::split_client_common_actions;
    use crate::llm::event_handler_executor::{
        try_execute_client_event_handler_with_privacy, ClientEventHandlerResult,
        HANDLER_INSTRUCTION_HEADER,
    };

    let offered_actions =
        crate::llm::actions::client_trait::client_llm_action_set(protocol, state, event);
    let private_payloads = crate::utils::redact::actions_have_credentials(&offered_actions);

    // Deterministic script/static routing, before any budget or model involvement.
    let mut handler_instruction: Option<String> = None;
    if let (Some(cid), Some(ev)) = (ClientId::from_string(&client_id), event) {
        match try_execute_client_event_handler_with_privacy(
            state,
            cid,
            ev.id(),
            &ev.event_type.description,
            Some(ev.data.clone()),
            private_payloads,
        )
        .await
        .map_err(|e| {
            if private_payloads {
                anyhow::anyhow!("credential-bearing event handler failed; details hidden")
            } else {
                e
            }
        }) {
            Ok(ClientEventHandlerResult::Handled { actions }) => {
                // Common actions (provide_feedback) are executed centrally,
                // mirroring the LLM path below; the protocol actions go back
                // to the client's loop.
                let (common_actions, protocol_actions) = split_client_common_actions(actions);
                if !common_actions.is_empty() {
                    if let Err(e) = crate::llm::actions::executor::execute_actions(
                        common_actions,
                        state,
                        None,
                        None,
                        Some(cid),
                    )
                    .await
                    {
                        warn!("Client common action execution failed: {}", e);
                    }
                }
                state
                    .record_access_log(
                        crate::state::AccessLogOwner::Client(cid.as_u32()),
                        protocol.protocol_name(),
                        None,
                        ev.id(),
                        ev.data.clone(),
                        protocol_actions
                            .iter()
                            .map(|action| {
                                access_log_action(action, private_payloads, &offered_actions)
                            })
                            .collect(),
                    )
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                return Ok(ClientLlmResult {
                    actions: protocol_actions,
                    memory_updates: None,
                });
            }
            Ok(ClientEventHandlerResult::FallbackToLlm { instruction }) => {
                handler_instruction = instruction;
            }
            Err(e) => {
                // An unresolvable static reference is a hard error, same as the
                // server path — a typo'd field name must not look like it works.
                error!(
                    "Client {} event handler for '{}' failed: {}",
                    client_id,
                    ev.id(),
                    e
                );
                let _ = status_tx.send(format!(
                    "[CLIENT] ✖ client {} event handler for '{}' failed: {}",
                    client_id,
                    ev.id(),
                    e
                ));
                return Err(e);
            }
        }
    }

    // Per-event LLM instruction from an `{"type":"llm"}` handler augments the
    // client-wide instruction for this one call.
    let composed_instruction = handler_instruction
        .map(|extra| format!("{instruction}\n\n{HANDLER_INSTRUCTION_HEADER}: {extra}"));
    let instruction = composed_instruction.as_deref().unwrap_or(instruction);

    if let Some(cid) = ClientId::from_string(&client_id) {
        match state.try_consume_client_llm_call(cid).await {
            Ok(used) => {
                let limit = state.get_client_llm_call_limit().await;
                if limit > 0
                    && u64::from(used) * 100 >= u64::from(limit) * u64::from(WARN_AT_PERCENT)
                {
                    warn!(
                        "Client {} has used {}/{} of its LLM call budget",
                        cid, used, limit
                    );
                    let _ = status_tx.send(format!(
                        "[CLIENT] ⚠ client {} has used {}/{} LLM calls",
                        cid, used, limit
                    ));
                }
            }
            Err(msg) => {
                error!("{}", msg);
                let _ = status_tx.send(format!("[CLIENT] ✖ {}", msg));
                return Err(anyhow::anyhow!(msg));
            }
        }
    }

    let result = crate::llm::action_helper::call_llm_for_client(
        llm_client,
        state,
        client_id.clone(),
        instruction,
        memory,
        event,
        protocol,
        status_tx,
    )
    .await?;

    // Record the event and the actions the model produced, mirroring the
    // server-side access log. Client entries record *produced* actions —
    // execution happens later in the client's own loop, which does not report
    // back here.
    if let (Some(cid), Some(ev)) = (ClientId::from_string(&client_id), event) {
        state
            .record_access_log(
                crate::state::AccessLogOwner::Client(cid.as_u32()),
                protocol.protocol_name(),
                None,
                ev.id(),
                ev.data.clone(),
                result
                    .actions
                    .iter()
                    .map(|action| access_log_action(action, private_payloads, &offered_actions))
                    .collect(),
            )
            .await;
    }

    Ok(result)
}

fn access_log_action(
    action: &serde_json::Value,
    private_payloads: bool,
    offered_actions: &[crate::llm::actions::ActionDefinition],
) -> serde_json::Value {
    if !private_payloads {
        return crate::utils::redact::redact_sensitive(action);
    }
    // A handler can copy a credential into any parameter or an invalid type.
    // Record only a name from the already-offered definitions, keeping the
    // original action untouched for execution and explicit display actions.
    match action["type"].as_str().filter(|name| {
        offered_actions
            .iter()
            .any(|definition| definition.name == *name)
    }) {
        Some(name) => serde_json::json!({"type":name}),
        None => serde_json::json!({}),
    }
}
