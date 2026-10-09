//! NetGet's router from raw WebSockets: subprotocol negotiation, HELLO rules, protocol
//! violations, routing errors (no such subscription / registration, duplicate registration,
//! a callee that leaves mid-call), pattern subscriptions with publisher exclusion and
//! black/white listing, fail-closed answers, an injected publication, and the NetGet pair.
use crate::helpers::wamp::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn routing_and_refusals_from_the_wire() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({"hello_timeout_secs": 2})).await;

    // Only wamp.2.json is spoken.
    assert!(raw(addr, "wamp.2.msgpack").await.is_err());
    // HELLO must come first, well formed, with a role, within the timeout.
    let mut ws = raw(addr, "wamp.2.json").await.unwrap();
    send(&mut ws, json!([48, 1, {}, "com.example.x"])).await;
    let a = recv(&mut ws).await.unwrap();
    assert_eq!(
        (a[0].as_u64(), a[2].as_str()),
        (Some(3), Some("wamp.error.protocol_violation"))
    );
    let mut ws = raw(addr, "wamp.2.json").await.unwrap();
    send(&mut ws, json!([1, "realm1", {"roles": {}}])).await;
    assert_eq!(recv(&mut ws).await.unwrap()[2], "wamp.error.no_such_role");
    let mut ws = raw(addr, "wamp.2.json").await.unwrap();
    assert_eq!(
        recv(&mut ws).await.unwrap()[2],
        "wamp.error.protocol_violation",
        "no HELLO in time"
    );

    let mut a = raw(addr, "wamp.2.json").await.unwrap();
    let mut b = raw(addr, "wamp.2.json").await.unwrap();
    let wa = hello(&mut a, "realm1").await;
    let wb = hello(&mut b, "realm1").await;
    assert_eq!(
        (
            wa[0].as_u64(),
            wa[2]["authrole"].as_str(),
            wa[2]["roles"]["broker"].is_object()
        ),
        (Some(2), Some("user"), true)
    );
    let (sa, sb) = (wa[1].as_u64().unwrap(), wb[1].as_u64().unwrap());

    // Subscriptions: wildcard on A, exact on B; publisher exclusion and listing.
    send(&mut a, json!([32, 1, {"match": "wildcard"}, "com..update"])).await;
    let sub_a = recv(&mut a).await.unwrap();
    assert_eq!(sub_a[0], 33);
    send(&mut b, json!([32, 1, {}, "com.stock.update"])).await;
    let sub_b = recv(&mut b).await.unwrap()[2].as_u64().unwrap();
    send(
        &mut b,
        json!([16, 2, {"acknowledge": true}, "com.stock.update", [1], {"px": 10}]),
    )
    .await;
    assert_eq!(
        recv(&mut b).await.unwrap()[0],
        17,
        "B is excluded from its own publication and gets PUBLISHED"
    );
    let ev = recv(&mut a).await.unwrap();
    assert_eq!(
        (
            ev[0].as_u64(),
            ev[3]["topic"].as_str(),
            ev[4].clone(),
            ev[5].clone()
        ),
        (
            Some(36),
            Some("com.stock.update"),
            json!([1]),
            json!({"px": 10})
        )
    );
    send(
        &mut a,
        json!([16, 2, {"exclude_me": false, "eligible": [sb]}, "com.stock.update", ["only b"]]),
    )
    .await;
    let ev = recv(&mut b).await.unwrap();
    assert_eq!(
        (ev[1].as_u64(), ev[4].clone()),
        (Some(sub_b), json!(["only b"]))
    );
    send(&mut a, json!([16, 3, {"exclude_me": false, "exclude": [sb], "acknowledge": true}, "com.stock.update", ["only a"]])).await;
    let mut seen = vec![recv(&mut a).await.unwrap(), recv(&mut a).await.unwrap()];
    seen.sort_by_key(|m| m[0].as_u64());
    assert_eq!(
        (seen[0][0].as_u64(), seen[1][0].as_u64(), seen[1][4].clone()),
        (Some(17), Some(36), json!(["only a"])),
        "PUBLISHED and A's own event; B is excluded"
    );
    send(&mut a, json!([34, 4, 999])).await;
    assert_eq!(
        recv(&mut a).await.unwrap()[4],
        "wamp.error.no_such_subscription"
    );

    // RPC: B registers, A calls through the router, B yields; duplicates and unknowns refused.
    send(&mut b, json!([64, 3, {}, "com.example.mul"])).await;
    assert_eq!(recv(&mut b).await.unwrap()[0], 65);
    send(&mut a, json!([64, 5, {}, "com.example.mul"])).await;
    assert_eq!(
        recv(&mut a).await.unwrap()[4],
        "wamp.error.procedure_already_exists"
    );
    send(
        &mut a,
        json!([48, 6, {"disclose_me": true}, "com.example.mul", [6, 7]]),
    )
    .await;
    let inv = recv(&mut b).await.unwrap();
    assert_eq!(
        (inv[0].as_u64(), inv[3]["caller"].as_u64(), inv[4].clone()),
        (Some(68), Some(sa), json!([6, 7]))
    );
    send(&mut b, json!([70, inv[1], {}, [42]])).await;
    assert_eq!(recv(&mut a).await.unwrap(), json!([50, 6, {}, [42]]));
    send(&mut a, json!([48, 7, {}, "com.example.mul", [1]])).await;
    let inv = recv(&mut b).await.unwrap();
    send(
        &mut b,
        json!([
            8,
            68,
            inv[1],
            {},
            "com.example.error.arity",
            ["two numbers"]
        ]),
    )
    .await;
    assert_eq!(
        recv(&mut a).await.unwrap(),
        json!([8, 48, 7, {}, "com.example.error.arity", ["two numbers"]])
    );
    send(&mut a, json!([66, 8, 12345])).await;
    assert_eq!(
        recv(&mut a).await.unwrap()[4],
        "wamp.error.no_such_registration"
    );
    // The callee leaves mid-call: the caller gets canceled.
    send(&mut a, json!([48, 9, {}, "com.example.mul", [2, 2]])).await;
    assert_eq!(recv(&mut b).await.unwrap()[0], 68);
    send(&mut b, json!([6, {}, "wamp.close.close_realm"])).await;
    assert_eq!(
        recv(&mut b).await.unwrap(),
        json!([6, {}, "wamp.close.goodbye_and_out"])
    );
    assert_eq!(recv(&mut a).await.unwrap()[4], "wamp.error.canceled");

    // An operator's publication through A's peer handle reaches A's wildcard subscription. Only
    // A and B reached the handler: the refused HELLOs above never did.
    let conn = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "wamp_hello",
        2,
    )
    .await[0]
        .connection_id
        .unwrap();
    let publish = json!({"type": "wamp_publish", "topic": "com.fx.update", "args": ["router"]});
    assert!(matches!(
        state
            .send_to_peer(sid, conn, publish, Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { bytes_sent: 1 }
    ));
    let ev = recv(&mut a).await.unwrap();
    assert_eq!(
        (ev[1].clone(), ev[3]["topic"].as_str(), ev[4].clone()),
        (sub_a[2].clone(), Some("com.fx.update"), json!(["router"]))
    );

    // A violation after WELCOME aborts.
    send(&mut a, json!([1, "realm1", {"roles": {"caller": {}}}])).await;
    assert_eq!(
        recv(&mut a).await.unwrap()[2],
        "wamp.error.protocol_violation"
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_handler_answer_fails_closed() {
    let state = state();
    let (sid, addr) = server_in(&state, vec![json!({"event_pattern": "wamp_hello", "handler": {"type": "static", "actions": [{"type": "wamp_welcome"}]}})], json!({})).await;
    let mut ws = raw(addr, "wamp.2.json").await.unwrap();
    assert_eq!(hello(&mut ws, "realm1").await[0], 2);
    send(&mut ws, json!([48, 1, {}, "com.example.anything"])).await;
    let e = recv(&mut ws).await.unwrap();
    assert_eq!(
        (e[0].as_u64(), e[1].as_u64(), e[4].as_str()),
        (Some(8), Some(48), Some("wamp.error.unavailable"))
    );
    let (sid2, addr2) = server_in(&state, vec![], json!({})).await;
    let mut ws = raw(addr2, "wamp.2.json").await.unwrap();
    let a = hello(&mut ws, "realm1").await;
    assert_eq!(
        (a[0].as_u64(), a[2].as_str()),
        (Some(3), Some("wamp.error.not_authorized"))
    );
    state.remove_server(sid).await;
    state.remove_server(sid2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_router() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let echo = json!({"type": "script", "language": "python", "code": "import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'wamp_yield','invocation':e['invocation'],'args':e['args']}]}))"});
    let callee = client_in(&state, addr.to_string(), json!({}), echo.clone())
        .await
        .unwrap();
    let caller = client_in(&state, addr.to_string(), json!({}), echo)
        .await
        .unwrap();
    let send = |c, a: serde_json::Value| state.send_to_client(c, a, Duration::from_secs(10));
    send(
        callee,
        json!({"type": "wamp_register", "procedure": "com.example.echo"}),
    )
    .await
    .unwrap();
    send(
        callee,
        json!({"type": "wamp_subscribe", "topic": "com.example.chat"}),
    )
    .await
    .unwrap();
    logs(
        &state,
        AccessLogOwner::Client(callee.as_u32()),
        "wamp_reply",
        2,
    )
    .await;
    send(caller, json!({"type": "wamp_call", "procedure": "com.example.echo", "args": ["hi"], "kwargs": {"n": 1}})).await.unwrap();
    send(
        caller,
        json!({"type": "wamp_call", "procedure": "com.example.time"}),
    )
    .await
    .unwrap();
    send(
        caller,
        json!({"type": "wamp_publish", "topic": "com.example.chat", "args": ["hello callee"]}),
    )
    .await
    .unwrap();
    let r: Vec<serde_json::Value> = logs(
        &state,
        AccessLogOwner::Client(caller.as_u32()),
        "wamp_reply",
        3,
    )
    .await
    .into_iter()
    .map(|r| r.request)
    .collect();
    let echo = r
        .iter()
        .find(|r| r["target"] == "com.example.echo")
        .unwrap();
    assert_eq!(
        (echo["ok"].as_bool(), echo["args"].clone()),
        (Some(true), json!(["hi"]))
    );
    let time = r
        .iter()
        .find(|r| r["target"] == "com.example.time")
        .unwrap();
    assert_eq!(time["args"], json!(["2026-10-04T12:00:00Z"]));
    assert!(r
        .iter()
        .any(|r| r["operation"] == "publish" && r["ok"] == true));
    let ev = logs(
        &state,
        AccessLogOwner::Client(callee.as_u32()),
        "wamp_event",
        1,
    )
    .await;
    assert_eq!(
        (
            ev[0].request["topic"].as_str(),
            ev[0].request["args"].clone()
        ),
        (Some("com.example.chat"), json!(["hello callee"]))
    );
    let inv = logs(
        &state,
        AccessLogOwner::Client(callee.as_u32()),
        "wamp_invocation",
        1,
    )
    .await;
    assert_eq!(inv[0].request["procedure"], "com.example.echo");
    send(caller, json!({"type": "wamp_goodbye"})).await.unwrap();
    let left = logs(
        &state,
        AccessLogOwner::Client(caller.as_u32()),
        "wamp_left",
        1,
    )
    .await;
    assert_eq!(left[0].request["reason"], "wamp.close.goodbye_and_out");
    state.remove_client(callee).await;
    state.remove_client(caller).await;
    state.remove_server(sid).await;
}
