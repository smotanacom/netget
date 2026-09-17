pub mod e2e_aws_sdk_test;
#[cfg(all(test, feature = "dynamo"))]
pub mod e2e_test;

/// The first-byte deadline, the idle deadline and the connection cap, from the wire.
#[cfg(all(test, feature = "dynamo"))]
mod connection_bounds_test;
