//! Unit tests for `netget::llm::hybrid_manager`.
//!
//! Migrated out of `src/llm/hybrid_manager.rs` — CLAUDE.md requires all tests to
//! live under `tests/` and reach internals through the public `netget::` API.
//!
//! The whole file is gated on `embedded-llm` because `hybrid_manager` itself is.

#![cfg(feature = "embedded-llm")]

use netget::llm::config::LlmBackendType;
use netget::llm::hybrid_manager::ActiveBackend;
use netget::llm::ollama_client::OllamaClient;
use std::sync::Arc;

#[tokio::test]
async fn test_backend_type() {
    let client = OllamaClient::new("http://localhost:11434");
    let backend = ActiveBackend::Ollama(client);
    assert_eq!(backend.backend_type(), LlmBackendType::Ollama);
    assert_eq!(backend.name(), "Ollama");
}

#[tokio::test]
#[ignore = "Requires model file"]
async fn test_embedded_backend() {
    use netget::llm::embedded_inference::EmbeddedLLMBackend;

    let backend = EmbeddedLLMBackend::new("./test-model.gguf").await.unwrap();
    let active = ActiveBackend::Embedded(Arc::new(backend));
    assert_eq!(active.backend_type(), LlmBackendType::Embedded);
}
