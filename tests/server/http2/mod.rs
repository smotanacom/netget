//! HTTP/2 protocol tests

#[cfg(all(test, feature = "http2"))]
mod e2e_test;

#[cfg(all(test, feature = "http2"))]
mod failure_semantics_test;
