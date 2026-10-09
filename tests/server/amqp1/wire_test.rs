//! NetGet's container with NetGet's client and the codec: type-system round trips and refusals,
//! the pair (publish, outcomes, relay, produced messages), and no handler answer.
use crate::helpers::amqp1::*;
use netget::server::amqp1::types::{decode, encode, Value};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[test]
fn type_system() {
    let v = Value::described(
        0x10,
        vec![
            Value::str("c"),
            Value::Null,
            Value::Uint(1_048_576),
            Value::Ushort(15),
            Value::Array(vec![Value::sym("PLAIN"), Value::sym("ANONYMOUS")]),
            Value::Map(vec![(Value::sym("k"), Value::Long(-3))]),
        ],
    );
    let b = encode(&v);
    assert_eq!(decode(&b).unwrap(), (v, b.len()));
    // Compact forms the specification defines.
    assert_eq!(encode(&Value::Uint(0)), [0x43]);
    assert_eq!(encode(&Value::Ulong(7)), [0x53, 7]);
    assert_eq!(encode(&Value::List(vec![])), [0x45]);
    assert_eq!(encode(&Value::str("hi")), [0xa1, 2, b'h', b'i']);
    // Hostile input: a list whose size disagrees with its contents, truncation, depth.
    assert!(decode(&[0xc0, 0x05, 0x01, 0x43]).is_err());
    assert!(decode(&[0xb1, 0xff, 0xff, 0xff, 0xff]).is_err());
    let mut deep = Vec::new();
    for _ in 0..100 {
        deep.extend([0x00, 0x53, 0x01]);
    }
    deep.push(0x40);
    assert!(decode(&deep).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_container() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let consumer = client_in(
        &state,
        addr.to_string(),
        json!({"sasl": "plain", "username": "alice", "password": "secret"}),
    )
    .await
    .unwrap();
    let producer = client_in(&state, addr.to_string(), json!({"sasl": "none"}))
        .await
        .unwrap();
    let recv = {
        let state = state.clone();
        tokio::spawn(async move {
            state.send_to_client(consumer, json!({"type": "amqp1_receive", "address": "orders.confirmed", "count": 2, "timeout_secs": 6}), Duration::from_secs(30)).await
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let send = |a: serde_json::Value| state.send_to_client(producer, a, Duration::from_secs(20));
    for a in [
        json!({"type": "amqp1_send", "address": "orders", "message": {"body": {"order": 7}}}),
        json!({"type": "amqp1_send", "address": "orders", "message": {"body": "no id"}}),
        json!({"type": "amqp1_send", "address": "orders", "message": {"body_type": "data", "body": "{\"order\": 8}"}}),
        json!({"type": "amqp1_send", "address": "forbidden", "message": {"body": 1}}),
        json!({"type": "amqp1_receive", "address": "news", "count": 1, "timeout_secs": 5}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    assert!(matches!(
        recv.await.unwrap().unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let o: Vec<serde_json::Value> = logs(
        &state,
        AccessLogOwner::Client(producer.as_u32()),
        "amqp1_outcome",
        4,
    )
    .await
    .into_iter()
    .map(|r| r.request)
    .collect();
    assert_eq!(
        o.iter()
            .map(|x| x["outcome"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["accepted", "rejected", "accepted", "refused"],
        "{o:?}"
    );
    let news = &logs(
        &state,
        AccessLogOwner::Client(producer.as_u32()),
        "amqp1_messages",
        1,
    )
    .await[0]
        .request;
    assert_eq!(news["messages"][0]["body"], "fresh news");
    let confirmed = &logs(
        &state,
        AccessLogOwner::Client(consumer.as_u32()),
        "amqp1_messages",
        1,
    )
    .await[0]
        .request;
    let bodies: Vec<_> = confirmed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["body"].clone())
        .collect();
    assert_eq!(
        bodies,
        [
            json!({"order": 7, "status": "confirmed"}),
            json!({"order": 8, "status": "confirmed"})
        ]
    );
    state.remove_client(consumer).await;
    state.remove_client(producer).await;

    // No answer: SASL fails closed, and without SASL the connection is closed.
    let (sid2, addr2) = server_in(&state, vec![], json!({})).await;
    assert!(client_in(&state, addr2.to_string(), json!({}))
        .await
        .is_err());
    state.remove_server(sid).await;
    state.remove_server(sid2).await;
}
