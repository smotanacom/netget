use super::common::{
    self, assert_failure, assert_success, hello, logon, pull, run, Peer, CYPHER_SHELL_PROPOSALS,
};
use netget::llm::actions::protocol_trait::Server;
use netget::server::bolt::{
    messages::{self, Request},
    packstream::{self, Value},
    values,
};
use serde_json::{json, Value as Json};

fn nested(layers: usize) -> Json {
    let mut value = Json::Null;
    for _ in 0..layers {
        value = Json::Array(vec![value]);
    }
    value
}
fn answer(cell: Json) -> Json {
    Json::Object(serde_json::Map::from_iter([
        ("type".into(), Json::String("send_bolt_records".into())),
        (
            "fields".into(),
            Json::Array(vec![Json::String("value".into())]),
        ),
        ("records".into(), Json::Array(vec![Json::Array(vec![cell])])),
    ]))
}
#[test]
fn paging_requires_an_explicit_integer_count_and_valid_optional_cursor() {
    for tag in [messages::PULL, messages::DISCARD] {
        for map in [
            Value::map(Vec::<(&str, Value)>::new()),
            Value::map([("n", Value::Null)]),
            Value::map([("n", Value::string("all"))]),
            Value::map([("n", Value::Int(1)), ("qid", Value::string("last"))]),
            Value::map([("n", Value::Int(1)), ("qid", Value::Int(-2))]),
        ] {
            assert!(messages::parse_request(Value::Struct {
                tag,
                fields: vec![map]
            })
            .is_err());
        }
        assert!(messages::parse_request(Value::Struct {
            tag,
            fields: vec![Value::map([("n", Value::Int(-1))])]
        })
        .is_ok());
    }
    assert!(matches!(
        messages::parse_request(Value::Struct {
            tag: messages::PULL,
            fields: vec![Value::map([("n", Value::Int(2)), ("qid", Value::Int(7))])]
        })
        .unwrap(),
        Request::Pull { n: 2, qid: 7 }
    ));
}
#[test]
fn every_known_message_has_its_native_arity_and_required_field_types() {
    for (tag, fields) in [
        (messages::HELLO, vec![Value::Map(vec![])]),
        (messages::LOGON, vec![Value::Map(vec![])]),
        (messages::BEGIN, vec![Value::Map(vec![])]),
        (
            messages::RUN,
            vec![
                Value::string("RETURN 1"),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        ),
        (
            messages::ROUTE,
            vec![Value::Map(vec![]), Value::List(vec![]), Value::Map(vec![])],
        ),
        (messages::TELEMETRY, vec![Value::Int(0)]),
        (messages::GOODBYE, vec![]),
        (messages::RESET, vec![]),
        (messages::LOGOFF, vec![]),
        (messages::COMMIT, vec![]),
        (messages::ROLLBACK, vec![]),
    ] {
        assert!(messages::parse_request(Value::Struct {
            tag,
            fields: fields.clone()
        })
        .is_ok());
        let mut extra = fields.clone();
        extra.push(Value::Null);
        assert!(messages::parse_request(Value::Struct { tag, fields: extra }).is_err());
        if !fields.is_empty() {
            let mut missing = fields.clone();
            missing.pop();
            assert!(messages::parse_request(Value::Struct {
                tag,
                fields: missing
            })
            .is_err());
        }
    }
    assert!(messages::parse_request(Value::Struct {
        tag: messages::RUN,
        fields: vec![Value::string("q"), Value::Null, Value::Map(vec![])]
    })
    .is_err());
    assert!(messages::parse_request(Value::Struct {
        tag: messages::TELEMETRY,
        fields: vec![Value::Int(4)]
    })
    .is_err());
    assert!(messages::parse_request(Value::Struct {
        tag: messages::ROUTE,
        fields: vec![
            Value::Map(vec![]),
            Value::List(vec![Value::Int(1)]),
            Value::Map(vec![])
        ]
    })
    .is_err());
}
#[test]
fn record_depth_includes_the_structure_and_row_envelope() {
    let accepted =
        values::query_answer(&answer(nested(packstream::MAX_PACKSTREAM_DEPTH - 2))).unwrap();
    let wire = packstream::to_bytes(&messages::record(
        accepted.records.into_iter().next().unwrap(),
    ));
    assert!(packstream::decode(&wire).is_ok());
    assert!(
        values::query_answer(&answer(nested(packstream::MAX_PACKSTREAM_DEPTH - 1)))
            .unwrap_err()
            .contains("wire depth")
    );
    let graph = answer(json!({"$node":{"id":1,"properties":{"deep":nested(29)}}}));
    assert!(
        values::query_answer(&graph).is_err(),
        "graph structure/properties also consume wire depth"
    );
}
#[test]
fn constructed_depth_is_refused_before_copying_and_owned_disposal_is_safe() {
    let action = answer(nested(10000));
    assert!(values::query_answer(&action).is_err());
    assert!(netget::server::bolt::actions::BoltProtocol::new()
        .execute_action(action)
        .is_err());
    let exact = nested(values::MAX_ACTION_DEPTH);
    assert!(values::action_within_budget(&exact));
    let excessive = nested(values::MAX_ACTION_DEPTH + 1);
    assert!(!values::action_within_budget(&excessive));
}
#[test]
fn answer_preflight_enforces_the_exact_node_and_retained_content_caps() {
    let exact = Json::Array(vec![Json::Null; values::MAX_ACTION_NODES - 1]);
    assert!(values::action_within_budget(&exact));
    let excessive = Json::Array(vec![Json::Null; values::MAX_ACTION_NODES]);
    assert!(!values::action_within_budget(&excessive));
    let len = values::MAX_ACTION_RETAINED_BYTES - std::mem::size_of::<Json>();
    assert!(values::action_within_budget(&Json::String("x".repeat(len))));
    assert!(!values::action_within_budget(&Json::String(
        "x".repeat(len + 1)
    )));
}
#[tokio::test]
async fn malformed_pull_never_drains_the_result_and_closes_with_a_native_error() {
    let state = common::new_state().await;
    let (_id, port, _rx) = common::start(
        &state,
        vec![common::accept_logins(), common::graph_handler()],
        None,
    )
    .await;
    let mut peer = Peer::connect_and_login(port).await;
    peer.send(&run("UNWIND range(1, 5) AS n RETURN n")).await;
    assert_success(&peer.recv().await);
    // Literal PackStream: PULL {n:"bad"}. It must never mean PULL {n:-1}.
    peer.send_raw(&packstream::chunk(&[
        0xb1, 0x3f, 0xa1, 0x81, b'n', 0x83, b'b', b'a', b'd',
    ]))
    .await;
    assert_failure(&peer.recv().await, "Neo.ClientError.Request.Invalid");
    peer.expect_eof(5).await;
}
#[tokio::test]
async fn an_overdepth_answer_fails_before_records_and_reset_keeps_the_server_usable() {
    let state = common::new_state().await;
    let handler = json!({"event_pattern":"bolt_query","handler":{"type":"static","actions":[answer(nested(31))]}});
    let (_id, port, _rx) =
        common::start(&state, vec![common::accept_logins(), handler], None).await;
    let mut peer = Peer::connect(port).await;
    peer.handshake(CYPHER_SHELL_PROPOSALS).await;
    peer.send(&hello()).await;
    assert_success(&peer.recv().await);
    peer.send(&logon("neo4j", "fixture")).await;
    assert_success(&peer.recv().await);
    peer.send(&run("RETURN nested")).await;
    assert_failure(
        &peer.recv().await,
        "Neo.TransientError.General.DatabaseUnavailable",
    );
    peer.send(&Value::Struct {
        tag: messages::RESET,
        fields: vec![],
    })
    .await;
    assert_success(&peer.recv().await);
    peer.send_all(&[run("CALL db.ping()"), pull(-1)]).await;
    assert_success(&peer.recv().await);
    let record = peer.recv().await;
    assert!(matches!(
        record,
        Value::Struct {
            tag: messages::RECORD,
            ..
        }
    ));
    assert_success(&peer.recv().await);
}
