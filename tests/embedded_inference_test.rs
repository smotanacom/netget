//! Unit tests for `netget::llm::embedded_inference`.
//!
//! Migrated out of `src/llm/embedded_inference.rs` — CLAUDE.md requires all
//! tests to live under `tests/` and reach internals through the public
//! `netget::` API.
//!
//! The whole file is gated on `embedded-llm` because the module itself is.

#![cfg(feature = "embedded-llm")]

use netget::llm::embedded_inference::{EmbeddedLLMBackend, InferenceConfig};

#[tokio::test]
#[ignore = "Requires actual GGUF model file"]
async fn test_load_model() {
    let backend = EmbeddedLLMBackend::new("./tests/fixtures/tiny-model.gguf")
        .await
        .expect("Failed to load model");

    assert!(backend.is_ready());
    let info = backend.get_model_info();
    assert!(info.vocab_size.is_some());
}

#[tokio::test]
#[ignore = "Requires actual GGUF model file"]
async fn test_generate() {
    let backend = EmbeddedLLMBackend::new("./tests/fixtures/tiny-model.gguf")
        .await
        .expect("Failed to load model");

    let response = backend.generate("Hello").await.expect("Generation failed");

    assert!(!response.is_empty());
}

#[test]
fn test_config_default() {
    let config = InferenceConfig::default();
    assert_eq!(config.context_size, 4096);
    assert_eq!(config.max_tokens, 2048);
    assert_eq!(config.temperature, 0.7);
    assert_eq!(config.top_p, 0.9);
}
