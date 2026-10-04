//! NetGet's AMQP 1.0 client against a rhea 3.0.5 broker (JavaScript, independent, unchanged):
//! SASL PLAIN, a message the broker accepts (and prints, properties included), one it rejects, a
//! refused link, and a receive of the message the broker sends to queue.out. Fails, never skips.
use crate::helpers::amqp1::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_rhea() {
    let broker = start_broker().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        broker.addr(),
        json!({"sasl": "plain", "username": "bob", "password": "pw"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    assert_eq!(
        logs(&state, owner, "amqp1_connected", 1).await[0].request["container_id"],
        "rhea-broker"
    );
    let send = |a: serde_json::Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for a in [
        json!({"type": "amqp1_send", "address": "inbox", "message": {"body": {"greeting": "hi"}, "properties": {"message_id": "n-1", "subject": "hello"}, "application_properties": {"priority": "high"}}}),
        json!({"type": "amqp1_send", "address": "inbox", "message": {"body": {"bad": true}}}),
        json!({"type": "amqp1_send", "address": "forbidden", "message": {"body": "x"}}),
        json!({"type": "amqp1_receive", "address": "queue.out", "count": 1, "timeout_secs": 5}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let o: Vec<serde_json::Value> = logs(&state, owner, "amqp1_outcome", 3)
        .await
        .into_iter()
        .map(|r| r.request)
        .collect();
    assert_eq!(o[0]["outcome"], "accepted", "{o:?}");
    assert_eq!(
        (
            o[1]["outcome"].as_str(),
            o[1]["condition"].as_str(),
            o[1]["description"].as_str()
        ),
        (
            Some("rejected"),
            Some("amqp:precondition-failed"),
            Some("bad message")
        )
    );
    assert_eq!(
        (o[2]["outcome"].as_str(), o[2]["condition"].as_str()),
        (Some("refused"), Some("amqp:unauthorized-access")),
        "{o:?}"
    );
    let m = &logs(&state, owner, "amqp1_messages", 1).await[0].request;
    assert_eq!(m["messages"][0]["body"], "from rhea", "{m}");
    assert_eq!(m["messages"][0]["properties"]["subject"], "greeting");
    assert_eq!(m["messages"][0]["application_properties"]["n"], 7);

    broker
        .wait_for_log("\"message_id\":\"n-1\"", Duration::from_secs(10))
        .await
        .unwrap();
    let printed = lines(&broker.log());
    let got = printed
        .iter()
        .find(|v| v["event"] == "message" && v["message_id"] == "n-1")
        .unwrap();
    assert_eq!(
        (
            got["body"].clone(),
            got["subject"].as_str(),
            got["application_properties"]["priority"].as_str()
        ),
        (json!({"greeting": "hi"}), Some("hello"), Some("high"))
    );
    assert!(matches!(
        send(json!({"type": "disconnect"})).await.unwrap(),
        ClientSendOutcome::Disconnected
    ));
    let refused = client_in(
        &state,
        broker.addr(),
        json!({"sasl": "plain", "username": "bob", "password": "nope"}),
    )
    .await;
    assert!(refused.unwrap_err().to_string().contains("SASL"));
    state.remove_client(cid).await;
}
