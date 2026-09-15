//! Save/load utility for persisting server and client configurations
//!
//! This module handles serializing server/client state to action arrays
//! and saving them to `.netget` files, as well as loading and parsing them.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::state::app_state::AppState;
use crate::state::client::ClientId;
use crate::state::server::ServerId;

/// File extension for NetGet save files
pub const NETGET_EXTENSION: &str = ".netget";

/// Normalize a filename to ensure it has the correct extension
/// Strips any existing extension and adds .netget
pub fn normalize_filename(name: &str) -> String {
    // Strip .netget extension if present to avoid duplication
    let name = name.trim();
    let name = if let Some(stripped) = name.strip_suffix(NETGET_EXTENSION) {
        stripped
    } else {
        // Strip any other extension
        Path::new(name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(name)
    };

    format!("{}{}", name, NETGET_EXTENSION)
}

/// Convert a server instance to an open_server action
fn server_to_action(server: &crate::state::server::ServerInstance) -> Value {
    let mut action = json!({
        "type": "open_server",
        "port": server.port,
        "base_stack": server.protocol_name,
        "instruction": server.instruction,
    });

    // Add optional fields if present
    if !server.memory.is_empty() {
        action["initial_memory"] = json!(server.memory);
    }

    if let Some(ref params) = server.startup_params {
        action["startup_params"] = params.clone();
    }

    // Note: send_first is protocol-specific and not stored in ServerInstance
    // It will use protocol defaults when recreated

    action
}

/// Convert a client instance to an open_client action
fn client_to_action(client: &crate::state::client::ClientInstance) -> Value {
    let mut action = json!({
        "type": "open_client",
        "protocol": client.protocol_name,
        "remote_addr": client.remote_addr,
        "instruction": client.instruction,
    });

    // Add optional fields if present
    if !client.memory.is_empty() {
        action["initial_memory"] = json!(client.memory);
    }

    if let Some(ref params) = client.startup_params {
        action["startup_params"] = params.clone();
    }

    action
}

/// Save all servers and clients to a file
pub async fn save_all(state: &AppState, filename: &str) -> Result<PathBuf> {
    let servers = state.get_all_servers().await;
    let clients = state.get_all_clients().await;

    let mut actions = Vec::new();

    // Convert servers to actions
    for server in servers {
        actions.push(server_to_action(&server));
    }

    // Convert clients to actions
    for client in clients {
        actions.push(client_to_action(&client));
    }

    save_actions(actions, filename).await
}

/// Save a specific server to a file
pub async fn save_server(state: &AppState, server_id: ServerId, filename: &str) -> Result<PathBuf> {
    let (port, protocol_name, instruction, memory, startup_params) = state
        .with_server_mut(server_id, |s| {
            (
                s.port,
                s.protocol_name.clone(),
                s.instruction.clone(),
                s.memory.clone(),
                s.startup_params.clone(),
            )
        })
        .await
        .context("Server not found")?;

    let server = crate::state::server::ServerInstance {
        id: server_id,
        port,
        protocol_name,
        instruction,
        memory,
        status: crate::state::server::ServerStatus::Stopped,
        connections: Default::default(),
        local_addr: None,
        created_at: crate::utils::clock::Instant::now(),
        status_changed_at: crate::utils::clock::Instant::now(),
        startup_params,
        event_handler_config: None,
        protocol_data: serde_json::Value::Null,
        log_files: Default::default(),
        feedback_instructions: None,
        feedback_buffer: Vec::new(),
        last_feedback_processed: None,
        recent_connections: Default::default(),
        connection_opened_at: Default::default(),
    };

    let actions = vec![server_to_action(&server)];
    save_actions(actions, filename).await
}

/// Save a specific client to a file
pub async fn save_client(state: &AppState, client_id: ClientId, filename: &str) -> Result<PathBuf> {
    let (remote_addr, protocol_name, instruction, memory, startup_params) = state
        .with_client_mut(client_id, |c| {
            (
                c.remote_addr.clone(),
                c.protocol_name.clone(),
                c.instruction.clone(),
                c.memory.clone(),
                c.startup_params.clone(),
            )
        })
        .await
        .context("Client not found")?;

    let client = crate::state::client::ClientInstance {
        id: client_id,
        remote_addr,
        protocol_name,
        instruction,
        memory,
        status: crate::state::client::ClientStatus::Disconnected,
        connection: None,
        created_at: crate::utils::clock::Instant::now(),
        status_changed_at: crate::utils::clock::Instant::now(),
        startup_params,
        event_handler_config: None,
        protocol_data: serde_json::Value::Null,
        log_files: Default::default(),
        feedback_instructions: None,
        feedback_buffer: Vec::new(),
        last_feedback_processed: None,
        connection_history: Default::default(),
    };

    let actions = vec![client_to_action(&client)];
    save_actions(actions, filename).await
}

/// Save an array of actions to a file
async fn save_actions(actions: Vec<Value>, filename: &str) -> Result<PathBuf> {
    let filename = normalize_filename(filename);
    let path = PathBuf::from(&filename);

    // Wrap actions in the standard LLM format: {"actions": [...]}
    let wrapped = json!({
        "actions": actions
    });

    // Serialize to pretty JSON
    let json =
        serde_json::to_string_pretty(&wrapped).context("Failed to serialize actions to JSON")?;

    // Write to file
    let mut file = fs::File::create(&path)
        .await
        .context(format!("Failed to create file: {}", filename))?;

    file.write_all(json.as_bytes())
        .await
        .context(format!("Failed to write to file: {}", filename))?;

    file.flush()
        .await
        .context(format!("Failed to flush file: {}", filename))?;

    Ok(path)
}

/// Load actions from a file
pub async fn load_actions(filename: &str) -> Result<Vec<Value>> {
    // Normalize filename (add .netget if missing)
    let filename = normalize_filename(filename);

    // Read file
    let content = fs::read_to_string(&filename)
        .await
        .context(format!("Failed to read file: {}", filename))?;

    // Parse JSON - expect {"actions": [...]} format
    let parsed: Value = serde_json::from_str(&content)
        .context(format!("Failed to parse JSON from file: {}", filename))?;

    // Extract actions array
    let actions = parsed
        .get("actions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("File must contain {{\"actions\": [...]}} format"))?
        .clone();

    Ok(actions)
}

/// Check if a string is a valid actions JSON in {"actions": [...]} format
pub fn is_actions_json(input: &str) -> bool {
    // Try to parse as JSON object
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        // Check for {"actions": [...]} format
        if let Some(actions) = value.get("actions").and_then(|v| v.as_array()) {
            // Check if all elements have a "type" field
            return !actions.is_empty()
                && actions.iter().all(|item| {
                    item.as_object()
                        .and_then(|obj| obj.get("type"))
                        .and_then(|t| t.as_str())
                        .is_some()
                });
        }
    }
    false
}
