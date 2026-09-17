//! Mercurial protocol tests

#[cfg(all(test, feature = "mercurial"))]
pub mod e2e_test;
#[cfg(all(test, feature = "mercurial"))]
mod real_client_test;
