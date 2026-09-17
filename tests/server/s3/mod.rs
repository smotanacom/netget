//! S3 protocol tests

#[cfg(all(test, feature = "s3"))]
pub mod e2e_test;

/// The first-byte deadline, the idle deadline and the connection cap, from the wire.
#[cfg(all(test, feature = "s3"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "s3"))]
pub mod real_client_test;
