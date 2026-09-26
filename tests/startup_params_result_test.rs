//! `StartupParams` must report bad model/client input, never panic.
//!
//! The JSON reaching `StartupParams` comes from the LLM (`open_server`) or
//! straight from an MCP client (`start_server`'s `startup_params`), so it is
//! untrusted. Every accessor returns a `Result` so the failure travels back to
//! the caller as a tool error instead of aborting the per-request task.

use netget::llm::actions::ParameterDefinition;
use netget::protocol::{StartupParamError, StartupParams};
use serde_json::json;

fn param(name: &str, type_hint: &str) -> ParameterDefinition {
    ParameterDefinition {
        name: name.to_string(),
        type_hint: type_hint.to_string(),
        description: String::new(),
        required: false,
        example: json!(null),
        default: None,
    }
}

fn schema() -> Vec<ParameterDefinition> {
    vec![
        param("send_first", "boolean"),
        param("banner", "string"),
        param("max_connections", "number"),
        param("headers", "object"),
        param("hosts", "array"),
    ]
}

#[test]
fn undeclared_key_is_rejected_by_new() {
    let err = StartupParams::new(json!({ "undeclared_xyz": 1 }), schema())
        .expect_err("undeclared key must be rejected");

    match &err {
        StartupParamError::Undeclared { key, allowed, .. } => {
            assert_eq!(key, "undeclared_xyz");
            // The allowed list is what a retrying model needs, so it must be complete.
            assert_eq!(
                allowed,
                &vec![
                    "banner".to_string(),
                    "headers".to_string(),
                    "hosts".to_string(),
                    "max_connections".to_string(),
                    "send_first".to_string(),
                ]
            );
        }
        other => panic!("expected Undeclared, got {other:?}"),
    }

    // The message still names the offending key and lists the allowed ones.
    let msg = err.to_string();
    assert!(msg.contains("undeclared_xyz"), "{msg}");
    assert!(msg.contains("send_first"), "{msg}");
    assert!(msg.contains("get_startup_parameters()"), "{msg}");
}

#[test]
fn declared_keys_are_accepted_by_new() {
    StartupParams::new(json!({ "send_first": true, "banner": "hi" }), schema())
        .expect("declared keys must be accepted");
}

#[test]
fn wrong_typed_value_is_an_error_not_a_panic() {
    let params = StartupParams::new(json!({ "send_first": "yes-please" }), schema()).unwrap();

    let err = params
        .get_optional_bool("send_first")
        .expect_err("a string is not a boolean");
    let msg = err.to_string();
    assert!(msg.contains("send_first"), "{msg}");
    assert!(msg.contains("not a boolean"), "{msg}");
    assert!(matches!(err, StartupParamError::Invalid { .. }));
}

#[test]
fn missing_required_value_is_an_error() {
    let params = StartupParams::new(json!({}), schema()).unwrap();

    assert!(params.get_string("banner").is_err());
    assert!(params.get_bool("send_first").is_err());
    assert!(params.get_i64("max_connections").is_err());
    assert!(params.get_u64("max_connections").is_err());
    assert!(params.get_object("headers").is_err());
    assert!(params.get_array("hosts").is_err());
}

#[test]
fn absent_optional_values_are_none() {
    let params = StartupParams::new(json!({}), schema()).unwrap();

    assert_eq!(params.get_optional_bool("send_first").unwrap(), None);
    assert_eq!(params.get_optional_string("banner").unwrap(), None);
    assert_eq!(params.get_optional_i64("max_connections").unwrap(), None);
    assert_eq!(params.get_optional_u64("max_connections").unwrap(), None);
    assert_eq!(params.get_optional_u32("max_connections").unwrap(), None);
    assert!(params.get_optional_object("headers").unwrap().is_none());
    assert!(params.get_optional_array("hosts").unwrap().is_none());
}

#[test]
fn explicit_null_reads_as_absent() {
    // A model that spells "unset" as `null` should not be treated as a type error.
    let params = StartupParams::new(json!({ "banner": null }), schema()).unwrap();
    assert_eq!(params.get_optional_string("banner").unwrap(), None);
}

#[test]
fn present_values_round_trip() {
    let params = StartupParams::new(
        json!({
            "send_first": true,
            "banner": "220 ready",
            "max_connections": 12,
            "headers": { "x": "y" },
            "hosts": ["a", "b"],
        }),
        schema(),
    )
    .unwrap();

    assert!(params.get_bool("send_first").unwrap());
    assert_eq!(params.get_string("banner").unwrap(), "220 ready");
    assert_eq!(params.get_i64("max_connections").unwrap(), 12);
    assert_eq!(params.get_u64("max_connections").unwrap(), 12);
    assert_eq!(
        params.get_optional_u32("max_connections").unwrap(),
        Some(12)
    );
    assert_eq!(params.get_object("headers").unwrap().len(), 1);
    assert_eq!(params.get_array("hosts").unwrap().len(), 2);
}

#[test]
fn u32_overflow_is_an_error() {
    let params =
        StartupParams::new(json!({ "max_connections": u32::MAX as u64 + 1 }), schema()).unwrap();

    let msg = params
        .get_optional_u32("max_connections")
        .unwrap_err()
        .to_string();
    assert!(msg.contains("max_connections"), "{msg}");
    assert!(msg.contains("u32::MAX"), "{msg}");
}

#[test]
fn accessing_a_key_the_protocol_never_declared_is_an_error() {
    let params = StartupParams::new(json!({}), schema()).unwrap();

    let err = params
        .get_optional_string("never_declared")
        .expect_err("undeclared key must not be readable");
    let msg = err.to_string();
    assert!(msg.contains("never_declared"), "{msg}");
    assert!(msg.contains("banner"), "{msg}");
}
