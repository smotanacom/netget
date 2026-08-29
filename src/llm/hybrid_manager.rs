//! Hybrid LLM manager that orchestrates between Ollama and embedded backends
//!
//! Provides automatic fallback: Ollama (primary) → embedded (fallback) → error

use crate::llm::config::{LlmBackendType, NetGetConfig};
use crate::llm::ollama_client::OllamaClient;
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

#[cfg(feature = "embedded-llm")]
use crate::llm::embedded_inference::{EmbeddedLLMBackend, InferenceConfig};

/// Active LLM backend
pub enum ActiveBackend {
    Ollama(OllamaClient),
    #[cfg(feature = "embedded-llm")]
    Embedded(Arc<EmbeddedLLMBackend>),
}

impl ActiveBackend {
    /// Get backend type
    pub fn backend_type(&self) -> LlmBackendType {
        match self {
            ActiveBackend::Ollama(_) => LlmBackendType::Ollama,
            #[cfg(feature = "embedded-llm")]
            ActiveBackend::Embedded(_) => LlmBackendType::Embedded,
        }
    }

    /// Get backend name for display
    pub fn name(&self) -> &str {
        match self {
            ActiveBackend::Ollama(_) => "Ollama",
            #[cfg(feature = "embedded-llm")]
            ActiveBackend::Embedded(_) => "Embedded (llama.cpp)",
        }
    }
}

/// Hybrid LLM manager with automatic fallback
pub struct HybridLLMManager {
    /// Active backend
    backend: Arc<RwLock<ActiveBackend>>,
    /// Configuration
    config: Arc<RwLock<NetGetConfig>>,
}

impl HybridLLMManager {
    /// Initialize hybrid manager with automatic backend selection
    ///
    /// # Strategy
    /// 1. Try Ollama if preferred (default)
    /// 2. Fall back to embedded if Ollama unavailable
    /// 3. Use last backend from config if specified
    ///
    /// # Arguments
    /// * `force_embedded` - Skip Ollama and use embedded directly
    /// * `embedded_model_path` - Override embedded model path
    pub async fn new(force_embedded: bool, embedded_model_path: Option<String>) -> Result<Self> {
        // Load configuration
        let mut config = NetGetConfig::load().unwrap_or_else(|e| {
            warn!("Failed to load config, using defaults: {}", e);
            NetGetConfig::default()
        });

        // Override embedded model path if provided
        #[cfg(feature = "embedded-llm")]
        if let Some(path) = embedded_model_path {
            config.embedded.last_model_path = Some(path.into());
        }

        let config = Arc::new(RwLock::new(config));

        // Select backend
        let backend = if force_embedded {
            Self::init_embedded_backend(&config).await?
        } else {
            Self::init_backend_with_fallback(&config).await?
        };

        let backend = Arc::new(RwLock::new(backend));

        Ok(Self { backend, config })
    }

    /// Initialize backend with fallback strategy
    async fn init_backend_with_fallback(
        config: &Arc<RwLock<NetGetConfig>>,
    ) -> Result<ActiveBackend> {
        let config_read = config.read().await;

        // Try Ollama first (if preferred)
        if config_read.ollama.prefer {
            debug!("Attempting to connect to Ollama...");
            match Self::check_ollama_health(&config_read.ollama.base_url).await {
                Ok(()) => {
                    info!("✓ Ollama available at {}", config_read.ollama.base_url);
                    let client = OllamaClient::new(&config_read.ollama.base_url);
                    return Ok(ActiveBackend::Ollama(client));
                }
                Err(e) => {
                    warn!("Ollama not available: {}", e);
                }
            }
        }

        // Fall back to embedded
        #[cfg(feature = "embedded-llm")]
        {
            drop(config_read);
            return Self::init_embedded_backend(config).await;
        }

        #[cfg(not(feature = "embedded-llm"))]
        {
            Err(anyhow!(
                "Ollama not available and embedded LLM not enabled.\n\
                 \n\
                 To fix this:\n\
                 1. Start Ollama: https://ollama.com/download\n\
                 2. Or rebuild with --features embedded-llm"
            ))
        }
    }

    /// Initialize embedded backend
    #[cfg(feature = "embedded-llm")]
    async fn init_embedded_backend(config: &Arc<RwLock<NetGetConfig>>) -> Result<ActiveBackend> {
        let config_read = config.read().await;

        let model_path = config_read
            .embedded
            .last_model_path
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "No embedded model configured.\n\
                     \n\
                     To download a model:\n\
                     1. Visit https://huggingface.co/TheBloke\n\
                     2. Download a GGUF model (e.g., Mistral-7B-Instruct-v0.1.Q4_K_M.gguf)\n\
                     3. Run: netget --embedded-model path/to/model.gguf\n\
                     \n\
                     Or use /model llama pull <name> once running"
                )
            })?;

        info!("Loading embedded LLM from: {}", model_path.display());

        let inference_config = InferenceConfig {
            context_size: config_read.embedded.context_size,
            max_tokens: config_read.embedded.max_tokens,
            temperature: 0.7,
            top_p: 0.9,
            n_gpu_layers: config_read.embedded.n_gpu_layers,
            n_threads: config_read.embedded.n_threads,
        };

        let backend = EmbeddedLLMBackend::new_with_config(model_path, inference_config)
            .await
            .context("Failed to initialize embedded LLM")?;

        info!("✓ Embedded LLM ready");

        Ok(ActiveBackend::Embedded(Arc::new(backend)))
    }

    /// Check Ollama health (GET /api/tags)
    async fn check_ollama_health(base_url: &str) -> Result<()> {
        let url = format!("{}/api/tags", base_url);
        // Same 5-second budget, same reason for not using a bare client: a literal-IP host
        // must not spend it in the system resolver.
        let client = crate::llm::ollama_client::client_for_endpoint_with_timeout(
            base_url,
            std::time::Duration::from_secs(5),
        );

        client
            .get(&url)
            .send()
            .await
            .context("Failed to connect to Ollama")?
            .error_for_status()
            .context("Ollama health check failed")?;

        Ok(())
    }

    /// Get active backend type
    pub async fn backend_type(&self) -> LlmBackendType {
        self.backend.read().await.backend_type()
    }

    /// Get active backend name
    pub async fn backend_name(&self) -> String {
        self.backend.read().await.name().to_string()
    }

    /// Get OllamaClient (if using Ollama backend)
    pub async fn ollama_client(&self) -> Option<OllamaClient> {
        match &*self.backend.read().await {
            ActiveBackend::Ollama(client) => Some(client.clone()),
            #[cfg(feature = "embedded-llm")]
            ActiveBackend::Embedded(_) => None,
        }
    }

    /// Generate completion (unified interface)
    ///
    /// Note: This is a simplified interface for basic text generation.
    /// In practice, use ConversationHandler for full LLM integration with actions.
    pub async fn generate(&self, model: &str, prompt: &str) -> Result<String> {
        match &*self.backend.read().await {
            ActiveBackend::Ollama(client) => {
                let response = client
                    .generate(model, prompt)
                    .await
                    .context("Ollama generation failed")?;
                Ok(response.text)
            }
            #[cfg(feature = "embedded-llm")]
            ActiveBackend::Embedded(backend) => {
                // Embedded backend doesn't need model name (it's already loaded)
                backend
                    .generate(prompt)
                    .await
                    .context("Embedded generation failed")
            }
        }
    }

    /// Switch to Ollama backend
    pub async fn switch_to_ollama(&self) -> Result<()> {
        let config_read = self.config.read().await;
        Self::check_ollama_health(&config_read.ollama.base_url).await?;

        let client = OllamaClient::new(&config_read.ollama.base_url);
        let mut backend_write = self.backend.write().await;
        *backend_write = ActiveBackend::Ollama(client);

        drop(config_read);
        drop(backend_write);

        // Save to config
        let mut config_write = self.config.write().await;
        config_write.set_last_backend(LlmBackendType::Ollama)?;

        info!("Switched to Ollama backend");
        Ok(())
    }

    /// Switch to embedded backend
    #[cfg(feature = "embedded-llm")]
    pub async fn switch_to_embedded(&self, model_path: Option<String>) -> Result<()> {
        // Update config if model path provided
        if let Some(path) = model_path {
            let mut config_write = self.config.write().await;
            config_write.embedded.last_model_path = Some(path.into());
            config_write.save()?;
        }

        let backend = Self::init_embedded_backend(&self.config).await?;
        let mut backend_write = self.backend.write().await;
        *backend_write = backend;

        drop(backend_write);

        let mut config_write = self.config.write().await;
        config_write.set_last_backend(LlmBackendType::Embedded)?;

        info!("Switched to embedded backend");
        Ok(())
    }

    /// Get current configuration (read-only)
    pub async fn config(&self) -> NetGetConfig {
        self.config.read().await.clone()
    }

    /// Update Ollama model in config
    pub async fn set_ollama_model(&self, model: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.set_ollama_model(model)
    }

    /// Update embedded model in config
    #[cfg(feature = "embedded-llm")]
    pub async fn set_embedded_model(&self, path: String) -> Result<()> {
        let mut config = self.config.write().await;
        config.set_embedded_model(path)
    }
}
