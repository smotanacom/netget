use netget::{
    llm::actions::protocol_trait::Server,
    server::oci_registry::actions::{
        action_within_budget, apply_blob_descriptors, OciRegistryProtocol, MAX_ACTION_DEPTH,
        MAX_ACTION_NODES, MAX_ACTION_RETAINED_BYTES,
    },
    utils::json_budget::drop_iteratively,
};
use serde_json::{json, Map, Value};
fn nested(layers: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..layers {
        value = Value::Array(vec![value]);
    }
    value
}
#[test]
fn constructed_actions_and_descriptor_copies_refuse_deep_values_safely() {
    let value = Value::Object(Map::from_iter([
        ("type".into(), Value::String("send_oci_manifest".into())),
        ("manifest".into(), nested(10000)),
    ]));
    assert!(OciRegistryProtocol::new().execute_action(value).is_err());
    let mut manifest = Value::Object(Map::from_iter([("config".into(), nested(10000))]));
    assert!(apply_blob_descriptors(&mut manifest, &[]).is_err());
    drop_iteratively(manifest);
}
#[test]
fn action_preflight_has_exact_depth_node_and_retained_boundaries() {
    assert!(action_within_budget(&nested(MAX_ACTION_DEPTH)));
    assert!(!action_within_budget(&nested(MAX_ACTION_DEPTH + 1)));
    assert!(action_within_budget(&Value::Array(vec![
        Value::Null;
        MAX_ACTION_NODES
            - 1
    ])));
    assert!(!action_within_budget(&Value::Array(vec![
        Value::Null;
        MAX_ACTION_NODES
    ])));
    let bytes = MAX_ACTION_RETAINED_BYTES - std::mem::size_of::<Value>();
    assert!(action_within_budget(&Value::String("x".repeat(bytes))));
    assert!(!action_within_budget(&Value::String("x".repeat(bytes + 1))));
}
#[test]
fn string_manifests_are_validated_after_parsing_before_serialization() {
    for layers in [MAX_ACTION_DEPTH, MAX_ACTION_DEPTH + 1] {
        let text = format!("{}null{}", "{\"x\":".repeat(layers), "}".repeat(layers));
        let action = json!({"type":"send_oci_manifest","manifest":text});
        let result = OciRegistryProtocol::new().execute_action(action);
        assert_eq!(result.is_ok(), layers == MAX_ACTION_DEPTH);
    }
}
