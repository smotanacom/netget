//! RSS protocol tests

#[cfg(all(test, feature = "rss"))]
pub mod e2e_test;

#[cfg(all(test, feature = "rss"))]
pub mod injection_test;

/// The first-byte deadline, the idle deadline and the connection cap, from the wire.
#[cfg(all(test, feature = "rss"))]
mod connection_bounds_test;
