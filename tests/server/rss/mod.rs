//! RSS protocol tests

#[cfg(all(test, feature = "rss"))]
pub mod e2e_test;

#[cfg(all(test, feature = "rss"))]
pub mod injection_test;
