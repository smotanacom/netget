//! Easy protocol startup logic

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tracing::info;

use crate::llm::OllamaClient;
use crate::protocol::EASY_REGISTRY;
use crate::state::{AppState, EasyId, EasyInstance, EasyStatus};

/// Start an easy protocol instance
///
/// This function:
/// 1. Creates an EasyInstance and adds it to AppState
/// 2. Calls the Easy protocol's generate_startup_action() to get underlying protocol action
/// 3. Executes the action to start the underlying server/client
/// 4. Links the underlying server/client to the easy instance for event routing
///
/// # Arguments
/// * `protocol_name` - Easy protocol name (e.g., "http-easy")
/// * `user_instruction` - Optional custom instruction from user
/// * `port` - Optional port override
/// * `state` - Application state
/// * `llm_client` - LLM client
///
/// # Returns
/// Easy protocol ID
pub async fn start_easy_protocol(
    protocol_name: &str,
    user_instruction: Option<String>,
    port: Option<u16>,
    state: Arc<AppState>,
    llm_client: Arc<OllamaClient>,
) -> Result<EasyId> {
    // Get easy protocol from registry
    let easy_protocol = EASY_REGISTRY
        .get_by_name(protocol_name)
        .ok_or_else(|| anyhow::anyhow!("Easy protocol '{}' not found", protocol_name))?;

    let underlying_protocol = easy_protocol.underlying_protocol();

    info!(
        "Starting easy protocol '{}' (wrapping '{}')",
        protocol_name, underlying_protocol
    );

    // Create easy instance
    let easy_instance = EasyInstance::new(
        EasyId::new(0), // Will be assigned by AppState
        protocol_name.to_string(),
        underlying_protocol.to_string(),
        user_instruction.clone(),
    );

    // Add to state and get assigned ID
    let easy_id = state.add_easy_instance(easy_instance).await;

    // Update status to Starting
    state
        .update_easy_status(easy_id, EasyStatus::Starting)
        .await;

    // Generate startup action for underlying protocol
    let action = match easy_protocol.generate_startup_action(user_instruction.clone(), port) {
        Ok(action) => action,
        Err(error) => {
            state
                .update_easy_status(easy_id, EasyStatus::Error(error.to_string()))
                .await;
            return Err(error).context("Failed to generate startup action");
        }
    };

    info!(
        "Generated startup action: {}",
        serde_json::to_string_pretty(&action)?
    );

    // Execute the startup action (open_server or open_client)
    match execute_easy_startup_action(&action, &state, &llm_client).await {
        Ok(underlying_id) => {
            match underlying_id {
                EasyUnderlyingId::Server(server_id) => {
                    state.link_server_to_easy(server_id, easy_id).await
                }
                EasyUnderlyingId::Client(client_id) => {
                    state.link_client_to_easy(client_id, easy_id).await
                }
            }

            // Update status to Running
            state.update_easy_status(easy_id, EasyStatus::Running).await;

            Ok(easy_id)
        }
        Err(e) => {
            // Update status to Error
            state
                .update_easy_status(easy_id, EasyStatus::Error(e.to_string()))
                .await;
            Err(e).context("Failed to start underlying protocol")
        }
    }
}

/// Typed ownership avoids narrowing arbitrary JSON integers when linking wrappers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EasyUnderlyingId {
    Server(crate::state::ServerId),
    Client(crate::state::ClientId),
}

/// Execute a generated easy action through the same validated forms as normal
/// startup. Optional handlers, memory, parameters and tasks are preserved.
pub async fn execute_easy_startup_action(
    action: &JsonValue,
    state: &AppState,
    llm_client: &OllamaClient,
) -> Result<EasyUnderlyingId> {
    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel();
    // Keep lifecycle messages observable without needing a dashboard receiver.
    tokio::spawn(async move {
        while let Some(message) = status_rx.recv().await {
            tracing::info!("{}", message);
        }
    });
    match action.get("type").and_then(JsonValue::as_str) {
        Some("open_server") => {
            let form: crate::cli::management::ServerForm =
                serde_json::from_value(action.clone())
                    .context("Invalid easy open_server action")?;
            Ok(EasyUnderlyingId::Server(
                form.create(state, status_tx).await?,
            ))
        }
        Some("open_client") => {
            let form: crate::cli::management::ClientForm =
                serde_json::from_value(action.clone())
                    .context("Invalid easy open_client action")?;
            Ok(EasyUnderlyingId::Client(
                form.create(state, llm_client.clone(), status_tx).await?,
            ))
        }
        Some(other) => anyhow::bail!("Unknown easy startup action type: {}", other),
        None => anyhow::bail!("Startup action missing string 'type' field"),
    }
}
