use netget::{
    client::bolt::{api, BoltClientProtocol},
    llm::actions::{client_trait::Client, protocol_trait::Protocol},
    server::bolt::{
        messages as m,
        packstream::{self, Value as Wire},
    },
};
use serde_json::{json, Value};
fn nested(layers: usize) -> Value {
    let mut v = Value::Null;
    for _ in 0..layers {
        v = Value::Array(vec![v]);
    }
    v
}
fn record(value: Wire) -> Vec<u8> {
    packstream::to_bytes(&m::record(vec![value]))
}
#[test]
fn advertised_actions_validate_and_every_event_retains_the_private_login_definition() {
    let p = BoltClientProtocol::new();
    for action in p.get_async_actions(&super::common::state()) {
        assert!(p.execute_action(action.example).is_ok(), "{}", action.name)
    }
    for event in p.get_event_types() {
        assert!(event.actions.iter().any(|a| a.name == "bolt_login"));
    }
}
#[test]
fn plain_parameters_do_not_promote_graph_like_maps_or_unsigned_integers() {
    let api::Action::Run { parameters, .. } = api::action(
        &json!({"type":"bolt_run","query":"RETURN $x","parameters":{"x":{"$node":{"id":1}}}}),
    )
    .unwrap() else {
        panic!()
    };
    assert!(matches!(parameters.get("x"), Some(Wire::Map(_))));
    for action in [
        json!({"type":"bolt_pull","n":0}),
        json!({"type":"bolt_pull","n":501}),
        json!({"type":"bolt_pull","n":"all"}),
        json!({"type":"bolt_login","password":"private"}),
        json!({"type":"bolt_run","query":"x","parameters":{"big":u64::MAX}}),
        json!({"type":"bolt_begin","mode":"anything"}),
    ] {
        assert!(api::action(&action).is_err());
    }
}
#[test]
fn action_depth_nodes_retained_caps_and_deep_owned_refusal_are_exact() {
    let v = nested(api::MAX_DEPTH);
    assert!(api::within_budget(&v));
    let v = nested(api::MAX_DEPTH + 1);
    assert!(!api::within_budget(&v));
    assert!(api::within_budget(&Value::Array(vec![
        Value::Null;
        api::MAX_NODES - 1
    ])));
    assert!(!api::within_budget(&Value::Array(vec![
        Value::Null;
        api::MAX_NODES
    ])));
    let size = api::MAX_RETAINED_BYTES - std::mem::size_of::<Value>();
    assert!(api::within_budget(&Value::String("x".repeat(size))));
    assert!(!api::within_budget(&Value::String("x".repeat(size + 1))));
    assert!(BoltClientProtocol::new()
        .execute_action(nested(10000))
        .is_err());
}
#[test]
fn negotiated_versions_reject_unoffered_reserved_and_manifest_values() {
    for minor in [0, 1, 2, 3, 4, 6, 7, 8] {
        assert_eq!(api::selected_version([0, 0, minor, 5]).unwrap(), minor)
    }
    for bytes in [
        [0, 0, 5, 5],
        [0, 0, 9, 5],
        [0, 1, 8, 5],
        [1, 0, 8, 5],
        [0, 0, 0, 0],
        [0, 0, 0, 6],
        [0, 0, 1, 255],
    ] {
        assert!(api::selected_version(bytes).is_err())
    }
}
#[test]
fn replies_require_typed_metadata_arity_and_native_versioned_failures() {
    for raw in [
        Wire::Struct {
            tag: m::SUCCESS,
            fields: vec![],
        },
        Wire::Struct {
            tag: m::IGNORED,
            fields: vec![Wire::Null],
        },
        m::success(vec![("has_more", Wire::string("true"))]),
        m::success(vec![("fields", Wire::List(vec![Wire::Int(1)]))]),
        m::success(vec![("qid", Wire::Int(-1))]),
        m::success(vec![("type", Wire::string("write"))]),
    ] {
        assert!(api::reply(&packstream::to_bytes(&raw), 8).is_err())
    }
    for minor in [0, 4, 8] {
        let raw = m::failure(minor, "Neo.ClientError.Statement.SyntaxError", "syntax");
        assert!(matches!(
            api::reply(&packstream::to_bytes(&raw), minor).unwrap(),
            api::Reply::Failure(_)
        ))
    }
    assert!(api::reply(
        &packstream::to_bytes(&m::failure(
            4,
            "Neo.ClientError.Statement.SyntaxError",
            "syntax"
        )),
        8
    )
    .is_err());
}
#[test]
fn typed_graph_tables_preserve_direction_and_validate_native_indices() {
    let node = |id| Wire::Struct {
        tag: 0x4e,
        fields: vec![
            Wire::Int(id),
            Wire::List(vec![Wire::string("Person")]),
            Wire::map([("name", Wire::string("Alice"))]),
            Wire::string(format!("node:{id}")),
        ],
    };
    let rel = Wire::Struct {
        tag: 0x72,
        fields: vec![
            Wire::Int(9),
            Wire::string("KNOWS"),
            Wire::Map(vec![]),
            Wire::string("rel:9"),
        ],
    };
    let path = |index| Wire::Struct {
        tag: 0x50,
        fields: vec![
            Wire::List(vec![node(1), node(2)]),
            Wire::List(vec![rel.clone()]),
            Wire::List(vec![Wire::Int(index), Wire::Int(1)]),
        ],
    };
    let api::Reply::Record(row) = api::reply(&record(path(-1)), 8).unwrap() else {
        panic!()
    };
    assert_eq!(row[0]["$path"]["indices"], json!([-1, 1]));
    assert_eq!(row[0]["$path"]["nodes"][0]["$node"]["element_id"], "node:1");
    for index in [0, 2, i64::MIN] {
        assert!(api::reply(&record(path(index)), 8).is_err())
    }
}
#[test]
fn temporal_units_bytes_and_nonfinite_floats_are_typed_without_coercion() {
    let raw = m::record(vec![
        Wire::Struct {
            tag: 0x49,
            fields: vec![Wire::Int(0), Wire::Int(123), Wire::Int(3600)],
        },
        Wire::Bytes(vec![0xff, 0]),
        Wire::Float(f64::NAN),
    ]);
    let api::Reply::Record(row) = api::reply(&packstream::to_bytes(&raw), 8).unwrap() else {
        panic!()
    };
    assert_eq!(row[0]["$datetime"]["nanoseconds"], 123);
    assert_eq!(row[1]["$bytes"], json!({"length":2,"content_omitted":true}));
    assert_eq!(row[2]["$float"], "NaN");
    for value in [
        Wire::Struct {
            tag: 0x49,
            fields: vec![Wire::Int(0), Wire::Int(1_000_000_000), Wire::Int(0)],
        },
        Wire::Struct {
            tag: 0x74,
            fields: vec![Wire::Int(-1)],
        },
        Wire::Struct {
            tag: 0x58,
            fields: vec![
                Wire::Int(7203),
                Wire::Float(f64::INFINITY),
                Wire::Float(2.0),
            ],
        },
    ] {
        assert!(api::reply(&record(value), 8).is_err())
    }
}
#[test]
fn credential_reflections_omit_dynamic_keys_and_values_while_preserving_fixed_graph_schema() {
    let node = Wire::Struct {
        tag: 0x4e,
        fields: vec![
            Wire::Int(1),
            Wire::List(vec![Wire::string("properties")]),
            Wire::map([("properties", Wire::string("secret properties"))]),
            Wire::string("node:1"),
        ],
    };
    let api::Reply::Record(row) = api::reply_private(&record(node), 8, Some("properties")).unwrap()
    else {
        panic!()
    };
    assert!(row[0]["$node"]["properties"].is_object());
    assert_eq!(row[0]["$node"]["properties"]["<redacted>"], "<redacted>");
    assert_eq!(row[0]["$node"]["labels"][0], "<redacted>");
    let raw = m::success(vec![
        ("fields", Wire::List(vec![Wire::string("fields")])),
        ("fields-copy", Wire::string("fields")),
    ]);
    let api::Reply::Success(v) =
        api::reply_private(&packstream::to_bytes(&raw), 8, Some("fields")).unwrap()
    else {
        panic!()
    };
    assert_eq!(v["fields"], json!(["<redacted>"]));
    assert_eq!(v["<redacted>"], "<redacted>");
}
#[test]
fn inbound_wire_depth_and_message_byte_caps_include_the_record_envelope() {
    let nested = |n| {
        let mut v = Wire::Null;
        for _ in 0..n {
            v = Wire::List(vec![v]);
        }
        v
    };
    assert!(api::reply(&record(nested(30)), 8).is_ok());
    assert!(api::reply(&record(nested(31)), 8).is_err());
    let overhead = record(Wire::Bytes(vec![0; 65536])).len() - 65536;
    let exact = record(Wire::Bytes(vec![
        0;
        packstream::MAX_MESSAGE_BYTES - overhead
    ]));
    assert_eq!(exact.len(), packstream::MAX_MESSAGE_BYTES);
    assert!(api::reply(&exact, 8).is_ok());
    let too_much = record(Wire::Bytes(vec![
        0;
        packstream::MAX_MESSAGE_BYTES - overhead
            + 1
    ]));
    assert!(api::reply(&too_much, 8).is_err());
}

#[test]
fn constructed_wire_values_are_refused_without_recursive_owned_destruction() {
    let mut v = Wire::Null;
    for _ in 0..10000 {
        v = Wire::List(vec![v])
    }
    assert!(api::message(
        m::RUN,
        vec![Wire::string("RETURN $x"), v, Wire::Map(vec![])]
    )
    .is_err());
}

#[test]
fn declared_text_password_map_list_fields_and_request_page_caps_have_exact_boundaries() {
    let run = |text: String, parameters: Value| json!({"type":"bolt_run","query":text,"parameters":parameters});
    assert!(api::action(&run("x".repeat(api::MAX_TEXT), json!({}))).is_ok());
    assert!(api::action(&run("x".repeat(api::MAX_TEXT + 1), json!({}))).is_err());
    for (len, accepted) in [(api::MAX_PASSWORD, true), (api::MAX_PASSWORD + 1, false)] {
        assert_eq!(
            api::action(
                &json!({"type":"bolt_login","username":"neo4j","password":"x".repeat(len)})
            )
            .is_ok(),
            accepted
        );
    }
    for (len, accepted) in [(256, true), (257, false)] {
        let parameters = Value::Object((0..len).map(|i| (format!("p{i}"), Value::Null)).collect());
        assert_eq!(
            api::action(&run("RETURN 1".into(), parameters)).is_ok(),
            accepted
        );
    }
    for (len, accepted) in [(10000, true), (10001, false)] {
        assert_eq!(
            api::action(&run("RETURN $x".into(), json!({"x":vec![Value::Null;len]}))).is_ok(),
            accepted
        );
    }
    assert!(api::action(&json!({"type":"bolt_pull","n":500})).is_ok());
    assert!(api::action(&json!({"type":"bolt_pull","n":501})).is_err());
    for (len, accepted) in [(api::MAX_FIELDS, true), (api::MAX_FIELDS + 1, false)] {
        let raw = m::success(vec![("fields", Wire::List(vec![Wire::string("x"); len]))]);
        assert_eq!(api::reply(&packstream::to_bytes(&raw), 8).is_ok(), accepted);
    }
    let plain = record(Wire::String("x".repeat(api::MAX_TEXT)));
    assert!(api::reply(&plain, 8).is_ok());
    assert!(api::reply(&record(Wire::String("x".repeat(api::MAX_TEXT + 1))), 8).is_err());
}
#[test]
fn native_update_counts_are_nonnegative_integers_and_update_flags_are_booleans() {
    for (stats, accepted) in [
        (
            Wire::map([
                ("nodes-created", Wire::Int(1)),
                ("contains-updates", Wire::Bool(true)),
            ]),
            true,
        ),
        (Wire::map([("nodes-created", Wire::Int(-1))]), false),
        (Wire::map([("nodes-created", Wire::string("1"))]), false),
        (Wire::map([("contains-updates", Wire::Int(1))]), false),
    ] {
        let raw = m::success(vec![("stats", stats)]);
        assert_eq!(api::reply(&packstream::to_bytes(&raw), 8).is_ok(), accepted)
    }
}

#[test]
fn hello_protocol_version_is_checked_before_reflection_redaction() {
    let raw = m::success(vec![
        ("server", Wire::string("Neo4j/fixture")),
        ("protocol_version", Wire::string("5.8")),
    ]);
    assert!(api::reply_private(&packstream::to_bytes(&raw), 8, Some("5.8")).is_ok());
    assert!(api::reply(&packstream::to_bytes(&raw), 4).is_err());
}
