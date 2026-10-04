//! Independent WAMP clients, unchanged, against NetGet's router with the policy script:
//! autobahn-python 24.4.2 and nexus 3.3.0 (Go) each join realm1, register and call a procedure
//! through the router, subscribe, publish with acknowledgement, call the router's own
//! procedures (a result and an application error) and are refused realm "blocked"; autobahn
//! also subscribes by prefix, sees its own callee's error routed back, is refused a duplicate
//! registration and leaves with GOODBYE. Fails, never skips.
use crate::helpers::wamp::*;
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;

async fn run(program: &str, args: &[&str]) -> Vec<serde_json::Value> {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer timed out")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    lines(&text)
}

#[tokio::test(flavor = "multi_thread")]
async fn autobahn_python_against_netget_router() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let script = format!("{}/tests/server/wamp/peer.py", env!("CARGO_MANIFEST_DIR"));
    let l = run(
        &peer("NETGET_WAMP_PYTHON"),
        &[&script, "127.0.0.1", &addr.port().to_string()],
    )
    .await;
    assert_eq!(step(&l, "add2")["result"], 5);
    assert!(
        step(&l, "arity")["error"]
            .as_str()
            .unwrap()
            .starts_with("wamp.error."),
        "the callee's own error came back: {l:?}"
    );
    assert!(step(&l, "published")["publication"].as_u64().unwrap() > 0);
    assert_eq!(
        step(&l, "event")["event"],
        json!(["exact", ["hello"], {"who": "autobahn"}])
    );
    assert_eq!(
        step(&l, "prefix")["event"],
        json!(["prefix", "com.example.pre.fix", [7]])
    );
    assert_eq!(
        step(&l, "time")["result"],
        json!({"args": ["2026-10-04T12:00:00Z"], "kwargs": {"zone": "utc"}})
    );
    assert_eq!(
        step(&l, "forbidden")["error"],
        "com.example.error.forbidden"
    );
    assert_eq!(step(&l, "forbidden")["args"], json!(["not for you"]));
    assert_eq!(
        step(&l, "duplicate")["error"],
        "wamp.error.procedure_already_exists"
    );
    assert_eq!(step(&l, "blocked")["error"], "wamp.error.no_such_realm");
    assert_eq!(step(&l, "leave")["reason"], "wamp.close.goodbye_and_out");
    let owner = AccessLogOwner::Server(sid.as_u32());
    let hellos = logs(&state, owner, "wamp_hello", 3).await;
    assert!(
        hellos[0].request["roles"].as_array().unwrap().len() == 4,
        "{}",
        hellos[0].request
    );
    let calls = logs(&state, owner, "wamp_call", 2).await;
    assert_eq!(
        (
            calls[0].request["procedure"].as_str(),
            calls[0].request["args"].clone()
        ),
        (Some("com.example.time"), json!(["utc"]))
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nexus_client_against_netget_router() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let l = run(
        &peer("NETGET_WAMP_NEXUS"),
        &["client", &format!("ws://{addr}/")],
    )
    .await;
    assert!(step(&l, "join")["session"].as_u64().is_some(), "{l:?}");
    assert_eq!(step(&l, "add2")["args"], json!([5]));
    assert_eq!(step(&l, "event")["args"], json!(["hello"]));
    assert_eq!(step(&l, "event")["kwargs"], json!({"from": "nexus"}));
    assert_eq!(
        step(&l, "com.example.time")["args"],
        json!(["2026-10-04T12:00:00Z"])
    );
    assert_eq!(
        step(&l, "com.example.forbidden")["error"],
        "com.example.error.forbidden"
    );
    assert!(
        step(&l, "blocked")["error"]
            .as_str()
            .unwrap()
            .contains("no_such_realm"),
        "{l:?}"
    );
    state.remove_server(sid).await;
}
