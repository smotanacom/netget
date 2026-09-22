//! Git protocol tests

#[cfg(all(test, feature = "git"))]
pub mod connection_bounds_test;
#[cfg(all(test, feature = "git"))]
pub mod e2e_test;
