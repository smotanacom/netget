//! Model selection utilities for Ollama
//!
//! This module provides functions to query available models from Ollama
//! and select the best one based on size and recency.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Information about an Ollama model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub size: u64,
    pub modified_at: String,
}

/// Query Ollama for available models and return detailed information
pub async fn query_available_models(ollama_url: &str) -> Result<Vec<ModelInfo>> {
    // Built through the shared helper, not `Client::new()`: this call has a 5-second timeout,
    // and pointing it at a literal IP used to spend all five inside `getaddrinfo("127.0.0.1")`
    // under load. It surfaced as `bluetooth_read_request` handlers "answering" in exactly
    // 5.003s with zero calls reaching the backend — the read failed closed on a timeout that
    // was pure name resolution. See `crate::llm::ollama_client::client_for_endpoint`.
    let client = crate::llm::ollama_client::client_for_endpoint(ollama_url);
    let url = format!("{}/api/tags", ollama_url);
    let response = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .context("Failed to connect to Ollama API. Is Ollama running?")?;

    if !response.status().is_success() {
        anyhow::bail!("Ollama API returned error status: {}", response.status());
    }

    let body = response
        .text()
        .await
        .context("Failed to read response from Ollama")?;

    // Parse the response
    let json: serde_json::Value =
        serde_json::from_str(&body).context("Failed to parse Ollama API response")?;

    let models = json
        .get("models")
        .and_then(|m| m.as_array())
        .context("Ollama API response missing 'models' array")?;

    let mut result = Vec::new();
    for model in models {
        let name = model
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let size = model.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        let modified_at = model
            .get("modified_at")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        if !name.is_empty() {
            result.push(ModelInfo {
                name,
                size,
                modified_at,
            });
        }
    }

    Ok(result)
}

/// Select the best model from available models
/// Prioritizes: largest size, then most recent modification
pub fn select_best_model(models: &[ModelInfo]) -> Option<String> {
    if models.is_empty() {
        return None;
    }

    // Sort by size (descending), then by modified_at (descending)
    let mut sorted = models.to_vec();
    sorted.sort_by(|a, b| {
        // First compare by size (larger is better)
        match b.size.cmp(&a.size) {
            std::cmp::Ordering::Equal => {
                // If sizes are equal, compare by modified_at (more recent is better)
                b.modified_at.cmp(&a.modified_at)
            }
            other => other,
        }
    });

    debug!(
        "Available models sorted by size: {:?}",
        sorted
            .iter()
            .map(|m| format!("{} ({} bytes)", m.name, m.size))
            .collect::<Vec<_>>()
    );

    Some(sorted[0].name.clone())
}

/// Check if Ollama is available and return available models
pub async fn check_ollama_availability(ollama_url: &str) -> Result<Vec<ModelInfo>> {
    query_available_models(ollama_url)
        .await
        .context("Ollama is not available")
}

/// Select or validate a model for use
///
/// # Arguments
/// * `configured_model` - Model from settings/args (None = auto-select)
/// * `interactive` - Whether running in interactive mode (affects error handling)
/// * `ollama_url` - URL of the Ollama server
///
/// # Returns
/// * `Ok(Some(model))` - Model selected and ready to use
/// * `Ok(None)` - No model available but should continue (interactive mode only)
/// * `Err(_)` - Critical error, should exit
pub async fn select_or_validate_model(
    configured_model: Option<String>,
    interactive: bool,
    ollama_url: &str,
) -> Result<Option<String>> {
    // Query Ollama for available models
    let models = match check_ollama_availability(ollama_url).await {
        Ok(models) => models,
        Err(e) => {
            let error_msg = format!(
                "✗  Ollama is not available: {}\n   Please ensure Ollama is running: https://ollama.ai\n   Use `/model` to list and select a model once Ollama is running.",
                e
            );

            if interactive {
                // In interactive mode, show warning but allow continuing
                warn!("{}", error_msg);
                return Ok(None);
            } else {
                // In non-interactive mode, this is a critical error
                anyhow::bail!("{}", error_msg);
            }
        }
    };

    if models.is_empty() {
        let error_msg = "✗  No models available in Ollama.\n   Please pull a model first: ollama pull qwen2.5-coder:32b\n   Use `/model` to list and select a model.";

        if interactive {
            warn!("{}", error_msg);
            return Ok(None);
        } else {
            anyhow::bail!("{}", error_msg);
        }
    }

    // If a model is configured, validate it exists
    if let Some(ref model_name) = configured_model {
        let model_exists = models.iter().any(|m| m.name == *model_name);

        if !model_exists {
            let available_models = models
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");

            let error_msg = format!(
                "✗  Configured model '{}' not found in Ollama.\n   Available models: {}\n   Use `/model` to select a different model.",
                model_name, available_models
            );

            if interactive {
                warn!("{}", error_msg);
                // Try to auto-select a different model
                if let Some(best_model) = select_best_model(&models) {
                    warn!("⚠  Auto-selecting model: {}", best_model);
                    return Ok(Some(best_model));
                }
                return Ok(None);
            } else {
                anyhow::bail!("{}", error_msg);
            }
        }

        info!("✓  Using configured model: {}", model_name);
        return Ok(Some(model_name.clone()));
    }

    // No model configured, auto-select the best one
    if let Some(best_model) = select_best_model(&models) {
        warn!(
            "⚠  No model configured, auto-selected: {} (largest/most recent)",
            best_model
        );
        info!("   To set a different model, use: /model or edit ~/.netget settings");
        Ok(Some(best_model))
    } else {
        // This shouldn't happen since we checked models.is_empty() above
        Ok(None)
    }
}

/// Ensure we have a model for LLM calls
/// If no model is set, attempts to auto-select one from Ollama
/// Returns error if Ollama is not available or no models exist
pub async fn ensure_model_selected(current_model: Option<String>) -> Result<String> {
    if let Some(model) = current_model {
        // Model already set
        debug!("Model already selected: {}", model);
        return Ok(model);
    }

    // No model set, try to auto-select one
    warn!("⚠  No model selected, attempting to auto-select from available models...");

    // Default to localhost:11434 since we don't have the ollama_url parameter here
    // This function is typically called from interactive TUI mode where ollama_url
    // should already be validated during startup
    match select_or_validate_model(None, false, "http://localhost:11434").await {
        Ok(Some(model)) => {
            info!("✓  Auto-selected model: {}", model);
            warn!(
                "⚠  Auto-selected model: {} (no model was configured)",
                model
            );
            Ok(model)
        }
        Ok(None) => {
            anyhow::bail!(
                "✗  No model available.\n   Please:\n   1. Ensure Ollama is running: https://ollama.ai\n   2. Pull a model: ollama pull qwen2.5-coder:32b\n   3. Use `/model` to select a model"
            )
        }
        Err(e) => Err(e),
    }
}
