//! BGP E2E tests module

#[cfg(all(test, feature = "bgp"))]
pub mod test;

#[cfg(all(test, feature = "bgp"))]
pub mod e2e_test;

#[cfg(all(test, feature = "bgp"))]
mod static_default_test;

#[cfg(all(test, feature = "bgp"))]
mod peer_inject_test;

#[cfg(all(test, feature = "bgp"))]
mod llm_failure_test;
