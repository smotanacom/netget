//! Kafka protocol E2E tests

#[cfg(all(test, feature = "kafka"))]
pub mod e2e_test;
#[cfg(all(test, feature = "kafka"))]
pub mod peer_inject_test;
