//! NetGet's Cap'n Proto RPC client against two independent servers, failing rather than
//! skipping when absent: pycapnp (the C++ runtime) and capnproto.org/go/capnp v3, both serving
//! directory.capnp. The connect handler's `store` is echoed back by each server exactly as it
//! decoded it; injected calls cover a superclass method, results with a union, group and
//! defaults, and an exception. Peers from `tests/server/capnp_rpc/install_peers.py`.
use super::session_test::{call, check_connect_store, client, require_capnp, SCHEMA};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/capnp_rpc/install_peers.py <root> and export what it prints")
    })
}

/// Start a peer server and return it with the address its READY line names.
async fn peer(program: PathBuf, args: &[&str]) -> (tokio::process::Child, String) {
    let mut child = tokio::process::Command::new(&program)
        .args(args)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut ready))
        .await
        .expect("peer did not start")
        .unwrap();
    let addr = ready
        .trim()
        .strip_prefix("READY ")
        .expect("READY line")
        .to_string();
    let addr = if addr.contains(':') {
        addr
    } else {
        format!("127.0.0.1:{addr}")
    };
    (child, addr)
}

async fn exercise(addr: String, pong: &str) {
    let (state, id) = client(addr, json!([])).await;
    check_connect_store(&state, id).await;
    let r = call(&state, id, "ping", json!({})).await;
    assert_eq!(r["results"]["pong"], pong, "{r}");
    let r = call(&state, id, "add", json!({"a": -40, "b": 82})).await;
    assert_eq!(r["results"], json!({"sum": 42}), "{r}");
    let r = call(&state, id, "lookup", json!({"name": "readme"})).await;
    let e = &r["results"]["entry"];
    assert_eq!(e["name"], "readme", "{r}");
    assert_eq!(e["kind"], "file");
    assert_eq!(e["target"], "/docs/readme");
    assert_eq!(e["owner"], json!({"uid": 501, "gid": 0}));
    assert_eq!(e["priority"], 5);
    assert_eq!(e["hidden"], true);
    assert_eq!(e["tags"], json!(["doc", "readme"]));
    let r = call(&state, id, "lookup", json!({"name": "missing"})).await;
    assert_eq!(r["results"], Value::Null);
    assert!(
        r["exception"]["reason"]
            .as_str()
            .unwrap()
            .contains("no entry named missing"),
        "{r}"
    );
    let r = call(&state, id, "fail", json!({"why": "testing"})).await;
    assert!(
        r["exception"]["reason"]
            .as_str()
            .unwrap()
            .contains("refused: testing"),
        "{r}"
    );
    assert_eq!(r["exception"]["kind"], "failed");
    state.remove_client(id).await;
}

#[tokio::test]
async fn netget_against_pycapnp_server() {
    require_capnp();
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/capnp_rpc/pycapnp_peer.py"
    );
    let (mut child, addr) = peer(
        env_path("NETGET_CAPNP_PYTHON"),
        &["-I", script, "server", SCHEMA],
    )
    .await;
    exercise(addr, "pong from pycapnp").await;
    child.kill().await.unwrap();
}

#[tokio::test]
async fn netget_against_go_capnp_server() {
    require_capnp();
    let (mut child, addr) = peer(env_path("NETGET_CAPNP_GO_PEER"), &["server"]).await;
    exercise(addr, "pong from go-capnp").await;
    child.kill().await.unwrap();
}
