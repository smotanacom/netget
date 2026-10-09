//! ncclient 0.7.1 — an independent Python NETCONF client on Paramiko — against NetGet's
//! server, over end-of-message framing (base 1.0) and chunked framing (base 1.1). It verifies
//! the pinned host key, authenticates, reads, writes, receives a typed rpc-error, is refused a
//! datastore the server never advertised, and closes the session. Fails, never skips, when
//! the peer is absent.
use crate::helpers::netconf::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

async fn ncclient(versions: &str, expected_base: &str) {
    let key = host_key();
    let state = state();
    let (sid, addr) = server_in(&state, device_policy(), json!({"host_key_path": key.path})).await;
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(python())
            .arg(peer_script())
            .args([
                "client",
                "127.0.0.1",
                &addr.port().to_string(),
                "admin",
                "secret",
                &key.b64,
                versions,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("ncclient run timed out")
    .expect("start ncclient");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "ncclient failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let steps: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let step = |name: &str| {
        steps
            .iter()
            .find(|s| s["step"] == name)
            .unwrap_or_else(|| panic!("no {name} step in {stdout}"))
            .clone()
    };
    let hello = step("hello");
    assert!(hello["session_id"]
        .as_str()
        .is_some_and(|s| s.parse::<u32>().unwrap() > 0));
    let caps: Vec<&str> = hello["server_capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(
        caps.contains(&"urn:ietf:params:netconf:base:1.0")
            && caps.contains(&"urn:ietf:params:netconf:base:1.1")
    );
    assert!(step("get")["data_xml"]
        .as_str()
        .unwrap()
        .contains("fixture α"));
    assert!(step("get-config")["data_xml"]
        .as_str()
        .unwrap()
        .contains("owner=\"fixture\""));
    assert_eq!(step("edit-config")["ok"], true);
    assert_eq!(step("lock")["error"], "lock-denied");
    assert_eq!(step("candidate")["error"], "invalid-value");
    step("closed");

    let server = AccessLogOwner::Server(sid.as_u32());
    let rpcs = logs(&state, server, "netconf_rpc", 4).await;
    let ops: Vec<_> = rpcs
        .iter()
        .map(|r| r.request["operation"].as_str().unwrap())
        .collect();
    assert_eq!(ops, ["get", "get-config", "edit-config", "lock"]);
    assert_eq!(rpcs[0].request["filter_type"], "subtree");
    assert!(rpcs[0].request["filter_xml"]
        .as_str()
        .unwrap()
        .contains(DEMO));
    assert!(rpcs[2].request["config_xml"]
        .as_str()
        .unwrap()
        .contains("changed &amp; saved"));
    // The candidate request was refused by Rust from the capability list; no handler call.
    assert_eq!(
        state
            .list_access_logs_for(Some(server), None)
            .await
            .iter()
            .filter(|e| e.event_type == "netconf_rpc")
            .count(),
        4
    );
    let negotiated = state.get_server(sid).await.unwrap();
    assert_eq!(negotiated.connections.len(), 1);
    let info = &negotiated
        .connections
        .values()
        .next()
        .unwrap()
        .protocol_info;
    assert_eq!(
        info.get("base_version").and_then(Value::as_str),
        Some(expected_base)
    );
    assert_eq!(info.get("username").and_then(Value::as_str), Some("admin"));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ncclient_completes_a_session_over_base_10_end_of_message_framing() {
    ncclient("1.0", "1.0").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ncclient_completes_a_session_over_base_11_chunked_framing() {
    ncclient("1.0,1.1", "1.1").await;
}
