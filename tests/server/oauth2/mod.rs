//! OAuth2 protocol E2E tests

#![cfg(all(test, feature = "oauth2"))]

pub mod body_limit_test;
pub mod e2e_test;
pub mod llm_failure_test;
