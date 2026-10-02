//! Unit tests for `netget::protocol::log_template`.
//!
//! Migrated out of `src/protocol/log_template.rs` — CLAUDE.md requires all tests
//! to live under `tests/` and reach internals through the public `netget::` API.

use netget::protocol::log_template::{LogLevel, LogTemplate};
use serde_json::json;

#[test]
fn field_values_containing_placeholders_remain_literal() {
    let template = LogTemplate::new().with_info("peer={peer}; message={message}; peer={peer}");
    let data = json!({"peer": "{message}", "message": "$1 {peer}\r\nforged"});
    assert_eq!(
        template.render(LogLevel::Info, &data),
        Some("peer={message}; message=$1 {peer}  forged; peer={message}".into())
    );
}

#[test]
fn test_simple_field_access() {
    let data = json!({
        "method": "GET",
        "path": "/api/users",
        "status": 200
    });

    let template = LogTemplate::new().with_info("{method} {path} -> {status}");

    let result = template.render(LogLevel::Info, &data);
    assert_eq!(result, Some("GET /api/users -> 200".to_string()));
}

#[test]
fn test_nested_field_access() {
    let data = json!({
        "headers": {
            "content_type": "application/json",
            "user_agent": "curl/7.0"
        }
    });

    let template = LogTemplate::new().with_debug("Content-Type: {headers.content_type}");

    let result = template.render(LogLevel::Debug, &data);
    assert_eq!(result, Some("Content-Type: application/json".to_string()));
}

#[test]
fn test_length_function() {
    let data = json!({
        "headers": {"a": 1, "b": 2, "c": 3},
        "body": "Hello, World!",
        "items": [1, 2, 3, 4, 5]
    });

    let template = LogTemplate::new()
        .with_debug("headers: {headers_len}, body: {body_len}B, items: {items_len}");

    let result = template.render(LogLevel::Debug, &data);
    assert_eq!(result, Some("headers: 3, body: 13B, items: 5".to_string()));
}

#[test]
fn test_json_pretty() {
    let data = json!({"x": 1, "y": 2});

    let template = LogTemplate::new().with_trace("Data: {json_pretty(.)}");

    let result = template.render(LogLevel::Trace, &data);
    assert!(result.is_some());
    assert!(result.unwrap().contains("\"x\": 1"));
}

#[test]
fn test_preview_function() {
    let data = json!({
        "long_text": "This is a very long text that should be truncated for display purposes"
    });

    let template = LogTemplate::new().with_info("{preview(long_text,20)}");

    let result = template.render(LogLevel::Info, &data);
    assert_eq!(result, Some("This is a very long ...".to_string()));
}

#[test]
fn test_hex_function() {
    let data = json!({
        "data": "Hello"
    });

    let template = LogTemplate::new().with_trace("Hex: {hex(data)}");

    let result = template.render(LogLevel::Trace, &data);
    assert_eq!(result, Some("Hex: 48656c6c6f".to_string()));
}

#[test]
fn test_missing_field() {
    let data = json!({
        "method": "GET"
    });

    let template = LogTemplate::new().with_info("{method} {missing_field}");

    let result = template.render(LogLevel::Info, &data);
    assert_eq!(result, Some("GET ".to_string()));
}

#[test]
fn test_no_template_returns_none() {
    let data = json!({"x": 1});
    let template = LogTemplate::new();

    assert!(template.render(LogLevel::Info, &data).is_none());
    assert!(template.render(LogLevel::Debug, &data).is_none());
    assert!(template.render(LogLevel::Trace, &data).is_none());
}

#[test]
fn test_has_any() {
    let empty = LogTemplate::new();
    assert!(!empty.has_any());

    let with_info = LogTemplate::new().with_info("test");
    assert!(with_info.has_any());
}
