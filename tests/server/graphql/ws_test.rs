//! graphql-transport-ws without peers: the handshake, the lifecycle close codes (4400, 4401,
//! 4408, 4409, 4429), ping/pong, `error` for refused operations, events from the start answer
//! and from `send_to_peer`, a handler-less server failing closed, and the NetGet pair.
use crate::helpers::graphql::*;
use futures::{SinkExt, StreamExt};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn open(addr: std::net::SocketAddr, protocol: Option<&str>) -> Result<Socket, String> {
    let mut req = format!("ws://{addr}/graphql")
        .into_client_request()
        .unwrap();
    if let Some(p) = protocol {
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", p.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(s, _)| s)
        .map_err(|e| e.to_string())
}

async fn put(ws: &mut Socket, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

/// The next JSON message, or `Err(close code)` when the server closes.
async fn take(ws: &mut Socket) -> Result<Value, u16> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("no message from the server")
        {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t).unwrap()),
            Some(Ok(Message::Close(f))) => {
                return Err(f.map(|f| u16::from(f.code)).unwrap_or(1005))
            }
            Some(Ok(_)) => continue,
            None | Some(Err(_)) => return Err(1006),
        }
    }
}

async fn ready(addr: std::net::SocketAddr) -> Socket {
    let mut ws = open(addr, Some("graphql-transport-ws")).await.unwrap();
    put(&mut ws, json!({"type": "connection_init"})).await;
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "connection_ack"})
    );
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn lifecycle_close_codes_and_messages_follow_graphql_ws() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        book_policy(),
        json!({"schema": BOOK_SCHEMA, "connection_init_timeout_secs": 1}),
    )
    .await;
    let refused = open(addr, None).await.unwrap_err();
    assert!(refused.contains("400"), "no subprotocol: {refused}");
    assert!(
        open(addr, Some("graphql-ws")).await.is_err(),
        "legacy protocol refused"
    );

    let mut ws = open(addr, Some("graphql-transport-ws")).await.unwrap();
    assert_eq!(take(&mut ws).await, Err(4408), "no connection_init in time");
    let mut ws = open(addr, Some("graphql-transport-ws")).await.unwrap();
    put(&mut ws, json!({"type": "subscribe", "id": "1", "payload": {"query": "subscription { countdown(from: 1) }"}})).await;
    assert_eq!(take(&mut ws).await, Err(4401), "subscribe before ack");
    let mut ws = ready(addr).await;
    put(&mut ws, json!({"type": "connection_init"})).await;
    assert_eq!(take(&mut ws).await, Err(4429));
    let mut ws = ready(addr).await;
    ws.send(Message::Text("{nope".into())).await.unwrap();
    assert_eq!(take(&mut ws).await, Err(4400));

    let mut ws = ready(addr).await;
    put(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(take(&mut ws).await.unwrap(), json!({"type": "pong"}));
    put(&mut ws, json!({"type": "subscribe", "id": "c", "payload": {"query": "subscription { countdown(from: 2) }"}})).await;
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "next", "id": "c", "payload": {"data": {"countdown": 2}}})
    );
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "next", "id": "c", "payload": {"data": {"countdown": 1}}})
    );
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "complete", "id": "c"})
    );
    put(
        &mut ws,
        json!({"type": "subscribe", "id": "v", "payload": {"query": "subscription { nope }"}}),
    )
    .await;
    let v = take(&mut ws).await.unwrap();
    assert_eq!(
        (v["type"].as_str(), v["id"].as_str()),
        (Some("error"), Some("v"))
    );
    assert!(v["payload"][0]["message"]
        .as_str()
        .unwrap()
        .contains("nope"));
    put(
        &mut ws,
        json!({"type": "subscribe", "id": "f", "payload": {"query": "subscription { forbidden }"}}),
    )
    .await;
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "error", "id": "f", "payload": [{"message": "not authorized"}]})
    );
    put(
        &mut ws,
        json!({"type": "subscribe", "id": "q", "payload": {"query": "{ hello }"}}),
    )
    .await;
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "next", "id": "q", "payload": {"data": {"hello": "Hello, world"}}})
    );
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "complete", "id": "q"})
    );

    // A subscription fed through the peer handle, then a duplicate id.
    put(&mut ws, json!({"type": "subscribe", "id": "b", "payload": {"query": "subscription { bookAdded { title } }"}})).await;
    let owner = AccessLogOwner::Server(sid.as_u32());
    let start = logs(&state, owner, "graphql_subscription_start", 3).await;
    let conn = start[2].connection_id.unwrap();
    let peer = |a: Value| state.send_to_peer(sid, conn, a, Duration::from_secs(10));
    assert!(matches!(
        peer(json!({"type":"graphql_event","subscription_id":"b","data":{"bookAdded":{"title":"Emma"}}})).await.unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "next", "id": "b", "payload": {"data": {"bookAdded": {"title": "Emma"}}}})
    );
    for bad in [
        json!({"type":"graphql_event","subscription_id":"zzz","data":{}}),
        json!({"type":"graphql_event","subscription_id":"b","data":"not an object"}),
        json!({"type":"graphql_result","data":{}}),
    ] {
        assert!(
            matches!(
                peer(bad.clone()).await.unwrap(),
                ClientSendOutcome::Rejected { .. }
            ),
            "{bad}"
        );
    }
    // A wrong type in pushed data is a field error in that event, not a dropped event.
    peer(
        json!({"type":"graphql_event","subscription_id":"b","data":{"bookAdded":{"title":["x"]}}}),
    )
    .await
    .unwrap();
    let v = take(&mut ws).await.unwrap();
    assert!(
        v["payload"]["data"].is_null()
            && v["payload"]["errors"][0]["path"] == json!(["bookAdded", "title"]),
        "{v}"
    );
    put(&mut ws, json!({"type": "subscribe", "id": "b", "payload": {"query": "subscription { bookAdded { title } }"}})).await;
    assert_eq!(take(&mut ws).await, Err(4409));

    let mut ws = ready(addr).await;
    put(&mut ws, json!({"type": "subscribe", "id": "b2", "payload": {"query": "subscription { bookAdded { title } }"}})).await;
    let start = logs(&state, owner, "graphql_subscription_start", 4).await;
    let conn = start[3].connection_id.unwrap();
    assert!(matches!(
        state
            .send_to_peer(
                sid,
                conn,
                json!({"type":"graphql_complete","subscription_id":"b2"}),
                Duration::from_secs(10)
            )
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    assert_eq!(
        take(&mut ws).await.unwrap(),
        json!({"type": "complete", "id": "b2"})
    );
    assert!(matches!(
        state
            .send_to_peer(
                sid,
                conn,
                json!({"type":"disconnect"}),
                Duration::from_secs(10)
            )
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    assert_eq!(take(&mut ws).await, Err(1000));
    state.remove_server(sid).await;

    // No handler and no model: the subscription is refused with a category error, never fed.
    let state = crate::helpers::graphql::state();
    let (sid, addr) = server_in(&state, vec![], json!({"schema": BOOK_SCHEMA})).await;
    let mut ws = ready(addr).await;
    put(&mut ws, json!({"type": "subscribe", "id": "x", "payload": {"query": "subscription { bookAdded { title } }"}})).await;
    let v = take(&mut ws).await.unwrap();
    assert_eq!(
        (v["type"].as_str(), v["id"].as_str()),
        (Some("error"), Some("x"))
    );
    assert!(v["payload"][0]["message"].is_string());
    put(&mut ws, json!({"type": "subscribe", "id": "x", "payload": {"query": "subscription { bookAdded { title } }"}})).await;
    let v = take(&mut ws).await.unwrap();
    assert_eq!(
        v["type"], "error",
        "the failed id is free again rather than a duplicate: {v}"
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server_agree_on_subscriptions() {
    let state = state();
    let (sid, addr) = server_in(&state, book_policy(), json!({"schema": BOOK_SCHEMA})).await;
    let cid = client_in(&state, addr.to_string(), json!({}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(15));
    assert!(matches!(
        send(
            json!({"type":"graphql_subscribe","query":"subscription Count { countdown(from: 2) }"})
        )
        .await
        .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let events = logs(&state, owner, "graphql_subscription_event", 2).await;
    assert_eq!(events[1].request["data"], json!({"countdown": 1}));
    assert_eq!(events[0].request["operation_name"], "Count");
    logs(&state, owner, "graphql_subscription_complete", 1).await;
    assert!(matches!(
        send(json!({"type":"graphql_subscribe","query":"subscription { bookAdded { title } }"}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let server = AccessLogOwner::Server(sid.as_u32());
    let start = logs(&state, server, "graphql_subscription_start", 2).await;
    let conn = start[1].connection_id.unwrap();
    let sub = start[1].request["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    state
        .send_to_peer(sid, conn, json!({"type":"graphql_event","subscription_id":sub,"data":{"bookAdded":{"title":"Emma"}}}), Duration::from_secs(10))
        .await
        .unwrap();
    let events = logs(&state, owner, "graphql_subscription_event", 3).await;
    assert_eq!(
        events[2].request["data"],
        json!({"bookAdded": {"title": "Emma"}})
    );
    assert!(matches!(
        send(json!({"type":"graphql_subscribe","query":"subscription { forbidden }"}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let refused = logs(&state, owner, "graphql_subscription_error", 1).await;
    assert_eq!(refused[0].request["errors"][0]["message"], "not authorized");
    // Disconnecting the socket from the server's side ends the remaining subscription with an
    // error event on the client.
    state
        .send_to_peer(
            sid,
            conn,
            json!({"type":"disconnect"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let ended = logs(&state, owner, "graphql_subscription_error", 2).await;
    assert_eq!(ended[1].request["subscription_id"], sub);
    assert!(ended[1].request["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("closed"));
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
