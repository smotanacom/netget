//! Ollama client tests
#![cfg(all(test, feature = "ollama"))]

pub mod command_channel_test;
pub mod e2e_test;
pub mod endpoint_targeting_test;
