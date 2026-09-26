//! Prometheus exporter tests

#[cfg(all(test, feature = "prometheus"))]
mod connection_bounds_test;

#[cfg(all(test, feature = "prometheus"))]
mod e2e_test;

#[cfg(all(test, feature = "prometheus"))]
mod exposition_test;

#[cfg(all(test, feature = "prometheus"))]
mod llm_failure_test;

#[cfg(all(test, feature = "prometheus"))]
mod real_client_test;
