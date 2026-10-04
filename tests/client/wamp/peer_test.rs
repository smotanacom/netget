//! NetGet's WAMP client against the nexus 3.3.0 router (Go; independent, unchanged) and its
//! local session: a call to nexus's callee and one that fails, a subscription receiving nexus's
//! ticks, a publication nexus's subscriber prints, and a registration nexus's caller invokes
//! and NetGet's handler answers. Fails, never skips.
use crate::helpers::wamp::*;
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_nexus() {
    let router = start_nexus_router().await.unwrap();
    let state = state();
    let echo = json!({"type": "script", "language": "python", "code": "import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'wamp_yield','invocation':e['invocation'],'args':['netget']+e['args']}]}))"});
    let cid = client_in(&state, router.addr(), json!({"realm": "realm1"}), echo)
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let welcome = logs(&state, owner, "wamp_welcome", 1).await;
    assert!(welcome[0].request["roles"]
        .as_array()
        .unwrap()
        .contains(&json!("dealer")));
    let send = |a: serde_json::Value| state.send_to_client(cid, a, Duration::from_secs(10));
    for a in [
        json!({"type": "wamp_call", "procedure": "com.example.add2", "args": [20, 22]}),
        json!({"type": "wamp_call", "procedure": "com.example.add2", "args": [1]}),
        json!({"type": "wamp_subscribe", "topic": "com.example.tick"}),
        json!({"type": "wamp_publish", "topic": "com.example.fromnetget", "args": ["hello nexus"], "kwargs": {"n": 1}}),
        json!({"type": "wamp_register", "procedure": "com.example.netget.echo"}),
    ] {
        send(a).await.unwrap();
    }
    let r: Vec<serde_json::Value> = logs(&state, owner, "wamp_reply", 5)
        .await
        .into_iter()
        .map(|r| r.request)
        .collect();
    let calls: Vec<&serde_json::Value> = r.iter().filter(|r| r["operation"] == "call").collect();
    assert!(
        calls
            .iter()
            .any(|c| c["ok"] == true && c["args"] == json!([42])),
        "{r:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c["ok"] == false && c["error"] == "com.example.error.arity"),
        "{r:?}"
    );
    assert!(r
        .iter()
        .any(|r| r["operation"] == "subscribe" && r["ok"] == true));
    let ticks = logs(&state, owner, "wamp_event", 2).await;
    assert_eq!(ticks[0].request["topic"], "com.example.tick");
    let inv = logs(&state, owner, "wamp_invocation", 1).await;
    assert_eq!(
        (
            inv[0].request["procedure"].as_str(),
            inv[0].request["args"].clone()
        ),
        (Some("com.example.netget.echo"), json!(["ping", "1"]))
    );
    router
        .wait_for_log("\"step\":\"echo\"", Duration::from_secs(15))
        .await
        .unwrap();
    router
        .wait_for_log("\"step\":\"fromnetget\"", Duration::from_secs(15))
        .await
        .unwrap();
    let out = lines(&router.log());
    assert_eq!(
        step(&out, "echo")["args"],
        json!(["netget", "ping", "1"]),
        "nexus's caller got NetGet's yield"
    );
    assert_eq!(step(&out, "fromnetget")["args"], json!(["hello nexus"]));
    assert_eq!(step(&out, "fromnetget")["kwargs"], json!({"n": 1}));
    send(json!({"type": "wamp_goodbye"})).await.unwrap();
    assert_eq!(
        logs(&state, owner, "wamp_left", 1).await[0].request["reason"],
        "wamp.close.goodbye_and_out"
    );
    state.remove_client(cid).await;
}
