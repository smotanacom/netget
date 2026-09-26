//! Docker Engine API tests

#[cfg(all(test, feature = "docker"))]
mod api_test;

#[cfg(all(test, feature = "docker"))]
mod connection_bounds_test;

#[cfg(all(test, feature = "docker"))]
mod e2e_test;

#[cfg(all(test, feature = "docker"))]
mod llm_failure_test;

#[cfg(all(test, feature = "docker"))]
pub mod real_client_test;
