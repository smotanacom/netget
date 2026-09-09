//! JSON-RPC protocol tests

#[cfg(all(test, feature = "jsonrpc"))]
pub mod e2e_test;
#[cfg(all(test, feature = "jsonrpc"))]
mod llm_failure_test;
