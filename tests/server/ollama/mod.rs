//! Ollama server tests
#![cfg(all(test, feature = "ollama"))]

pub mod e2e_test;
pub mod embeddings_test;
pub mod real_client_test;
pub mod refusal_status_test;
