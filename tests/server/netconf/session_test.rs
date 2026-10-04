//! NetGet's NETCONF client against NetGet's NETCONF server: both base versions, every
//! implemented operation shape, and the lifecycle rules — authentication, host-key pinning,
//! kill-session, the hello deadline, and stopping a server with a request parked for a human.
use crate::helpers::netconf::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner, ClientStatus};
use serde_json::json;
use std::time::Duration;
use tokio::io::AsyncReadExt;

fn client_params(key: &HostKey, versions: &[&str]) -> serde_json::Value {
    json!({"username":"admin","password":"secret","host_key_sha256":key.sha256,"base_versions":versions})
}

/// Answer every client event with nothing, so events are recorded without a model.
fn quiet() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"netconf_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"netconf_rpc_reply","handler":{"type":"static","actions":[]}}),
    ]
}

fn walk_handlers() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"netconf_connected","handler":{"type":"static","actions":[{"type":"netconf_rpc","operation":"get-config","source":"running","filter_xml":"<demo xmlns=\"urn:netget:netconf-peer\"/>"}]}}),
        json!({"event_pattern":"netconf_rpc_reply","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "nxt={'get-config':{'type':'netconf_rpc','operation':'edit-config','target':'running','default_operation':'merge','config_xml':'<demo xmlns=\"urn:netget:netconf-peer\"><label>changed &amp; saved</label></demo>'},\n",
            "     'edit-config':{'type':'netconf_rpc','operation':'lock','target':'running'},\n",
            "     'lock':{'type':'netconf_rpc','operation':'custom','input_xml':'<uptime-query xmlns=\"urn:netget:netconf-peer\"/>'},\n",
            "     'custom':{'type':'netconf_rpc','operation':'close-session'}}.get(e['operation'])\n",
            "print(json.dumps({'actions':[nxt] if nxt else []}))\n"
        )}}),
    ]
}

async fn walk(versions: &[&str], expected_base: &str) {
    let key = host_key();
    let state = state();
    let (sid, addr) = server_in(&state, device_policy(), json!({"host_key_path": key.path})).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        walk_handlers(),
        client_params(&key, versions),
    )
    .await
    .unwrap();
    let client = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, client, "netconf_connected", 1).await;
    assert_eq!(connected[0].request["base_version"], expected_base);
    assert!(connected[0].request["session_id"].as_u64().unwrap() > 0);
    let caps = connected[0].request["server_capabilities"]
        .as_array()
        .unwrap();
    assert!(caps
        .iter()
        .any(|c| c == "urn:ietf:params:netconf:capability:writable-running:1.0"));
    let replies = logs(&state, client, "netconf_rpc_reply", 5).await;
    let ops: Vec<_> = replies
        .iter()
        .map(|r| r.request["operation"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ops,
        [
            "get-config",
            "edit-config",
            "lock",
            "custom",
            "close-session"
        ]
    );
    // message-ids are Rust's own counter and every reply was matched to its request.
    let ids: Vec<_> = replies
        .iter()
        .map(|r| r.request["message_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["1", "2", "3", "4", "5"]);
    let data = replies[0].request["data_xml"].as_str().unwrap();
    assert!(
        data.contains("fixture α")
            && data.contains("urn:netget:netconf-peer")
            && data.contains("owner=\"fixture\""),
        "{data}"
    );
    assert_eq!(replies[1].request["ok"], true);
    let error = &replies[2].request["errors"][0];
    assert_eq!(error["error_tag"], "lock-denied");
    assert_eq!(error["error_type"], "protocol");
    let info = error["error_info_xml"].as_str().unwrap();
    assert!(
        info.contains("99</session-id>")
            && info.contains("urn:ietf:params:xml:ns:netconf:base:1.0"),
        "{info}"
    );
    assert!(replies[3].request["output_xml"]
        .as_str()
        .unwrap()
        .contains("42"));
    assert_eq!(replies[4].request["ok"], true);

    let server = AccessLogOwner::Server(sid.as_u32());
    let rpcs = logs(&state, server, "netconf_rpc", 4).await;
    assert_eq!(rpcs[0].request["source"], "running");
    assert!(rpcs[0].request["filter_xml"]
        .as_str()
        .unwrap()
        .contains("urn:netget:netconf-peer"));
    let config = rpcs[1].request["config_xml"].as_str().unwrap();
    assert!(config.contains("changed &amp; saved"), "{config}");
    assert_eq!(rpcs[1].request["default_operation"], "merge");
    assert_eq!(rpcs[1].request["username"], "admin");
    assert_eq!(rpcs[3].request["custom"], true);
    assert_eq!(rpcs[3].request["namespace"], DEMO);
    // close-session is answered by Rust, not the handler.
    assert_eq!(logs(&state, server, "netconf_rpc", 4).await.len(), 4);
    assert!(
        wait_until(Duration::from_secs(10), || async {
            state
                .get_client(cid)
                .await
                .is_none_or(|c| c.status == ClientStatus::Disconnected)
        })
        .await
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_walks_every_operation_shape_over_chunked_base_11() {
    walk(&["1.0", "1.1"], "1.1").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_walks_every_operation_shape_over_end_of_message_base_10() {
    walk(&["1.0"], "1.0").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_and_wrong_host_key_never_reach_a_session() {
    let key = host_key();
    let state = state();
    let (sid, addr) = server_in(&state, device_policy(), json!({"host_key_path": key.path})).await;
    let mut params = client_params(&key, &["1.0", "1.1"]);
    params["password"] = json!("wrong");
    let refused = client_in(&state, addr.to_string(), vec![], params).await;
    assert!(refused.is_err(), "a refused password must fail the client");
    let other = host_key();
    let refused = client_in(
        &state,
        addr.to_string(),
        vec![],
        client_params(&other, &["1.1"]),
    )
    .await;
    assert!(
        refused.is_err(),
        "a host key that does not match the pin must fail the handshake"
    );
    let server = AccessLogOwner::Server(sid.as_u32());
    // One password decision (the wrong one); the pinned-out client never authenticated.
    let auth = logs(&state, server, "netconf_auth", 1).await;
    assert_eq!(auth[0].request["password"], "wrong");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        state
            .list_access_logs_for(Some(server), None)
            .await
            .iter()
            .filter(|e| e.event_type == "netconf_auth")
            .count(),
        1
    );
    assert!(state
        .list_access_logs_for(Some(server), None)
        .await
        .iter()
        .all(|e| e.event_type != "netconf_rpc"));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_session_ends_another_live_session_and_refuses_itself() {
    let key = host_key();
    let state = state();
    let (sid, addr) = server_in(&state, device_policy(), json!({"host_key_path": key.path})).await;
    let first = client_in(
        &state,
        addr.to_string(),
        quiet(),
        client_params(&key, &["1.1"]),
    )
    .await
    .unwrap();
    let second = client_in(
        &state,
        addr.to_string(),
        quiet(),
        client_params(&key, &["1.1"]),
    )
    .await
    .unwrap();
    let first_id = logs(
        &state,
        AccessLogOwner::Client(first.as_u32()),
        "netconf_connected",
        1,
    )
    .await[0]
        .request["session_id"]
        .as_u64()
        .unwrap();
    let second_id = logs(
        &state,
        AccessLogOwner::Client(second.as_u32()),
        "netconf_connected",
        1,
    )
    .await[0]
        .request["session_id"]
        .as_u64()
        .unwrap();
    assert_ne!(first_id, second_id);
    let sent = state
        .send_to_client(
            first,
            json!({"type":"netconf_rpc","operation":"kill-session","session_id":first_id}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }));
    let own = logs(
        &state,
        AccessLogOwner::Client(first.as_u32()),
        "netconf_rpc_reply",
        1,
    )
    .await;
    assert_eq!(own[0].request["errors"][0]["error_tag"], "invalid-value");
    state
        .send_to_client(
            first,
            json!({"type":"netconf_rpc","operation":"kill-session","session_id":second_id}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let killed = logs(
        &state,
        AccessLogOwner::Client(first.as_u32()),
        "netconf_rpc_reply",
        2,
    )
    .await;
    assert_eq!(killed[1].request["ok"], true);
    assert!(
        wait_until(Duration::from_secs(10), || async {
            state
                .get_client(second)
                .await
                .is_none_or(|c| c.status != ClientStatus::Connected)
        })
        .await,
        "the killed session's client is still connected"
    );
    // The killer's session is untouched.
    assert_eq!(
        state.get_client(first).await.unwrap().status,
        ClientStatus::Connected
    );
    state.remove_client(first).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_refuses_datastores_the_server_did_not_advertise_before_writing() {
    let key = host_key();
    let state = state();
    let (sid, addr) = server_in(
        &state,
        device_policy(),
        json!({"host_key_path": key.path, "capabilities": []}),
    )
    .await;
    let cid = client_in(
        &state,
        addr.to_string(),
        vec![],
        client_params(&key, &["1.1"]),
    )
    .await
    .unwrap();
    for action in [
        json!({"type":"netconf_rpc","operation":"get-config","source":"candidate"}),
        json!({"type":"netconf_rpc","operation":"edit-config","target":"running","config_xml":"<a xmlns=\"urn:x\"/>"}),
        json!({"type":"netconf_rpc","operation":"commit"}),
        json!({"type":"netconf_rpc","operation":"get-config","source":"url"}),
        json!({"type":"netconf_rpc","operation":"edit-config","target":"running","config_xml":"<!DOCTYPE x [<!ENTITY e 'x'>]><a/>"}),
        json!({"type":"netconf_rpc","operation":"custom","input_xml":"<get xmlns=\"urn:ietf:params:xml:ns:netconf:base:1.0\"/>"}),
        json!({"type":"netconf_rpc","operation":"copy-config"}),
    ] {
        let outcome = state
            .send_to_client(cid, action.clone(), Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            matches!(outcome, ClientSendOutcome::Rejected { .. }),
            "{action} was not refused: {outcome:?}"
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let server = AccessLogOwner::Server(sid.as_u32());
    assert!(state
        .list_access_logs_for(Some(server), None)
        .await
        .iter()
        .all(|e| e.event_type != "netconf_rpc"));
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_server_ends_a_session_parked_on_a_manual_rpc() {
    let key = host_key();
    let state = state();
    let mut handlers = device_policy();
    handlers[1] =
        json!({"event_pattern":"netconf_rpc","handler":{"type":"manual","timeout_secs":300}});
    let (sid, addr) = server_in(&state, handlers, json!({"host_key_path": key.path})).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        vec![],
        client_params(&key, &["1.1"]),
    )
    .await
    .unwrap();
    state
        .send_to_client(
            cid,
            json!({"type":"netconf_rpc","operation":"get"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(20), || async {
            !state.list_intercepts().await.is_empty()
        })
        .await,
        "the rpc never parked"
    );
    assert_eq!(
        state
            .get_server(sid)
            .await
            .unwrap()
            .connections
            .values()
            .filter(|c| c.status == netget::state::server::ConnectionStatus::Active)
            .count(),
        1,
        "the parked session must still be a live connection"
    );
    state.remove_server(sid).await;
    assert!(
        wait_until(Duration::from_secs(10), || async {
            state
                .get_client(cid)
                .await
                .is_none_or(|c| c.status != ClientStatus::Connected)
        })
        .await,
        "the client's SSH connection outlived the stopped server"
    );
    assert!(
        wait_until(Duration::from_secs(5), || async {
            state.list_intercepts().await.is_empty()
        })
        .await,
        "the parked rpc outlived its session"
    );
    state.remove_client(cid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_never_completes_hello_is_closed_at_the_handshake_deadline() {
    let key = host_key();
    let state = state();
    let (sid, addr) = server_in(
        &state,
        device_policy(),
        json!({"host_key_path": key.path, "handshake_timeout_secs": 1}),
    )
    .await;
    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    let started = std::time::Instant::now();
    let mut sink = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), peer.read_to_end(&mut sink))
        .await
        .expect("the server kept a silent peer past its handshake deadline")
        .ok();
    assert!(
        sink.starts_with(b"SSH-2.0-"),
        "the server speaks first: {:?}",
        String::from_utf8_lossy(&sink)
    );
    assert!(
        started.elapsed() >= Duration::from_millis(800),
        "closed after {:?}, before the deadline",
        started.elapsed()
    );
    state.remove_server(sid).await;
}
