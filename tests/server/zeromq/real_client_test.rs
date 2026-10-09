//! Independent ZeroMQ peers against NetGet's server, failing rather than skipping when absent:
//! pyzmq 27.2.0 (libzmq 4.3.5) as REQ, DEALER-to-ROUTER and PUSH-to-PULL, and go-zeromq/zmq4
//! v0.17.0 (pure Go) as REQ and PUSH. `tests/client/zeromq/install_peers.py` installs both and
//! prints NETGET_ZEROMQ_PYTHON and NETGET_ZEROMQ_GO_PEER.
use super::wire_test::{handlers, logged, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/zeromq/install_peers.py <root> and export what it prints"))
}

const PEER_PY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/client/zeromq/peer.py");

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

async fn pyzmq(args: &[&str]) -> Value {
    let mut all = vec!["-I", PEER_PY];
    all.extend_from_slice(args);
    run(env_path("NETGET_ZEROMQ_PYTHON"), &all).await
}

#[tokio::test]
async fn pyzmq_req_dealer_and_push() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let a = addr.to_string();
    assert_eq!(
        pyzmq(&["req", &a, "hello", "multi part"]).await,
        json!({"reply": ["ECHO", "hello", "multi part", "REP", "REQ", "-"]})
    );
    state.remove_server(id).await;
    let (state, id, addr) = start(handlers(), json!({"socket_type": "router"})).await;
    assert_eq!(
        pyzmq(&["dealer", &addr.to_string(), "client-a"]).await,
        json!({"reply": ["ECHO", "ping", "ROUTER", "DEALER", "client-a"]})
    );
    assert_eq!(
        pyzmq(&["req", &addr.to_string(), "via router"]).await,
        json!({"reply": ["ECHO", "via router", "ROUTER", "REQ", "-"]}),
        "a REQ peer of a ROUTER gets its envelope back"
    );
    state.remove_server(id).await;
    let (state, id, addr) = start(handlers(), json!({"socket_type": "pull"})).await;
    assert_eq!(
        pyzmq(&["push", &addr.to_string(), "job-1", "job-2"]).await,
        json!({"pushed": 2})
    );
    assert!(logged(&state, id, r#""frames":["job-2"]"#).await);
    assert!(logged(&state, id, r#""peer_socket_type":"PUSH""#).await);
    state.remove_server(id).await;
}

#[tokio::test]
async fn go_zeromq_req_and_push() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let out = run(
        env_path("NETGET_ZEROMQ_GO_PEER"),
        &["req", &addr.to_string(), "from", "go"],
    )
    .await;
    let reply = out["reply"].as_array().expect("a reply");
    assert_eq!(
        &reply[..5],
        &[
            json!("ECHO"),
            json!("from"),
            json!("go"),
            json!("REP"),
            json!("REQ")
        ]
    );
    // zmq4 always announces an Identity: a random UUID.
    assert_eq!(reply[5].as_str().map(str::len), Some(36), "{out}");
    state.remove_server(id).await;
    let (state, id, addr) = start(handlers(), json!({"socket_type": "pull"})).await;
    assert_eq!(
        run(
            env_path("NETGET_ZEROMQ_GO_PEER"),
            &["push", &addr.to_string(), "go-job"]
        )
        .await,
        json!({"pushed": 1})
    );
    assert!(logged(&state, id, r#""frames":["go-job"]"#).await);
    state.remove_server(id).await;
}
