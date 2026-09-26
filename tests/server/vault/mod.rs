//! Vault tests

#[cfg(all(test, feature = "vault"))]
mod api_test;

#[cfg(all(test, feature = "vault"))]
mod connection_bounds_test;

#[cfg(all(test, feature = "vault"))]
mod e2e_test;

#[cfg(all(test, feature = "vault"))]
mod llm_failure_test;

#[cfg(all(test, feature = "vault"))]
pub mod real_client_test;
