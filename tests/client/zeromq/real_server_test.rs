//! NetGet's ZeroMQ client against pyzmq 27.2.0 (libzmq 4.3.5) sockets, unchanged, through
//! `peer.py`: REQ to REP, DEALER to ROUTER (which sees the Identity), PUSH to PULL, and SUB to
//! an XPUB that publishes only once it sees the subscription — so a received "weather" and an
//! absent "sports" prove the subscription went over the wire. Fails rather than skips without
//! NETGET_ZEROMQ_PYTHON (`install_peers.py`).
use super::session_test::{client, send, wait_log};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout};

const PEER_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/client/zeromq/peer.py");

async fn bind(mode: &str) -> (Child, Lines<BufReader<ChildStdout>>, String) {
    let python = std::env::var_os("NETGET_ZEROMQ_PYTHON").map(PathBuf::from).expect("NETGET_ZEROMQ_PYTHON is required: run tests/client/zeromq/install_peers.py <root> and export what it prints");
    let mut child = tokio::process::Command::new(python)
        .args(["-I", PEER_PY, mode])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start pyzmq");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = next(&mut lines).await;
    let addr = ready["ready"].as_str().expect("ready line").to_string();
    (child, lines, addr)
}

async fn next(lines: &mut Lines<BufReader<ChildStdout>>) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("pyzmq peer deadline")
        .unwrap()
        .expect("a line from pyzmq");
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn netget_against_pyzmq_sockets() {
    let (_rep, mut lines, addr) = bind("rep").await;
    let (state, id) = client(
        addr,
        json!({}),
        json!([{"type":"zmq_send","frames":["hi","libzmq"]}]),
    )
    .await;
    wait_log(&state, id, r#""frames":["ECHO","hi","libzmq"]"#).await;
    assert_eq!(next(&mut lines).await, json!({"got": ["hi", "libzmq"]}));
    state.remove_client(id).await;

    let (_router, mut lines, addr) = bind("router").await;
    let (state, id) = client(
        addr,
        json!({"socket_type":"dealer","identity":"netget-dealer"}),
        json!([{"type":"zmq_send","frames":["job"]}]),
    )
    .await;
    wait_log(&state, id, r#""frames":["job","routed to netget-dealer"]"#).await;
    assert_eq!(
        next(&mut lines).await,
        json!({"identity": "netget-dealer", "got": ["job"]})
    );
    state.remove_client(id).await;

    let (_pull, mut lines, addr) = bind("pull").await;
    let (state, id) = client(
        addr,
        json!({"socket_type":"push"}),
        json!([{"type":"zmq_send","frames":["task","7"]}]),
    )
    .await;
    assert_eq!(next(&mut lines).await, json!({"got": ["task", "7"]}));
    state.remove_client(id).await;

    let (_pub, mut lines, addr) = bind("pub").await;
    let (state, id) = client(
        addr,
        json!({"socket_type":"sub"}),
        json!([{"type":"zmq_subscribe","topic":"weather"}]),
    )
    .await;
    assert_eq!(
        next(&mut lines).await,
        json!({"subscription": "01", "topic": "weather"})
    );
    wait_log(&state, id, r#""frames":["weather","sunny"]"#).await;
    wait_log(&state, id, r#""frames":["weather","rain"]"#).await;
    let sports = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await
        .iter()
        .any(|e| serde_json::to_string(e).unwrap().contains("goal"));
    assert!(!sports, "libzmq filtered the unsubscribed topic");
    let refused = send(&state, id, json!({"type":"zmq_send","frames":["x"]})).await;
    assert!(
        format!("{refused:?}").contains("cannot send"),
        "{refused:?}"
    );
    send(
        &state,
        id,
        json!({"type":"zmq_unsubscribe","topic":"weather"}),
    )
    .await;
    assert_eq!(
        next(&mut lines).await,
        json!({"subscription": "00", "topic": "weather"})
    );
    state.remove_client(id).await;
}
