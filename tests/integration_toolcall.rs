//! Tool call integration and unit tests
//!
//! This test binary contains all tests related to tool calling:
//! - read_file tool tests
//! - web_search tool tests
//! - Integration tests with full NetGet runs

#[allow(dead_code, unused_imports)]
mod helpers;

#[path = "toolcall/read_file_test.rs"]
mod read_file_test;

#[path = "toolcall/read_file_integration_test.rs"]
mod read_file_integration_test;

#[path = "toolcall/web_search_test.rs"]
mod web_search_test;

#[path = "toolcall/web_search_integration_test.rs"]
mod web_search_integration_test;
