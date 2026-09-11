//! TLS client tests

#[cfg(all(test, feature = "tls"))]
mod command_channel_test;
#[cfg(all(test, feature = "tls"))]
pub mod e2e_test;
#[cfg(all(test, feature = "tls"))]
mod multi_turn_test;
