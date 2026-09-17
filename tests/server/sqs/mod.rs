//! SQS protocol E2E tests

#[cfg(all(test, feature = "sqs"))]
pub mod e2e_test;

/// The first-byte deadline, the idle deadline and the connection cap, from the wire.
#[cfg(all(test, feature = "sqs"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "sqs"))]
pub mod real_client_test;
