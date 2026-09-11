//! MCP (Model Context Protocol) tests

#[cfg(all(test, feature = "mcp"))]
pub mod e2e_test;
#[cfg(all(test, feature = "mcp"))]
mod llm_failure_test;

#[cfg(all(test, feature = "mcp"))]
mod error_code_range_test;
