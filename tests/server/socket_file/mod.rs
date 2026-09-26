//! Unix domain socket file tests
//!
//! Platform: Unix/Linux only
#![cfg(all(test, feature = "socket_file", unix))]

pub mod connection_bounds_test;
pub mod test;
