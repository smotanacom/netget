//! Independent Cap'n Proto RPC clients against NetGet's server, failing rather than skipping
//! when absent: pycapnp (the C++ runtime) and capnproto.org/go/capnp v3, both over
//! directory.capnp, each calling every method — a superclass method, a struct with a union, a
//! group, defaults, Data and nested lists, an exception, and five calls in flight at once.
//! `tests/server/capnp_rpc/install_peers.py` prints NETGET_CAPNP_PYTHON and NETGET_CAPNP_GO_PEER.
use super::wire_test::{handler_saw, handlers, start, SCHEMA};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

pub fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/capnp_rpc/install_peers.py <root> and export what it prints")
    })
}

pub const PY_PEER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/server/capnp_rpc/pycapnp_peer.py"
);

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&program)
            .args(args)
            .stdin(std::process::Stdio::null())
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

/// What both clients must have read back from NetGet.
fn check(out: &Value, client_tag: &str) {
    assert_eq!(out["ping"], "pong from netget", "{out}");
    assert_eq!(out["add"], 42);
    let lookup = &out["lookup"];
    assert_eq!(lookup["name"], "readme");
    assert_eq!(lookup["size"], 1234);
    assert_eq!(lookup["kind"], "file");
    assert_eq!(lookup["tags"], json!(["doc", "readme"]));
    assert_eq!(
        lookup["priority"], 5,
        "an unset field reads as its schema default"
    );
    assert_eq!(lookup["hidden"], true, "a Bool defaulting to true");
    assert_eq!(lookup["owner"], json!({"uid": 501, "gid": 0}));
    assert_eq!(lookup["which"], "target");
    assert_eq!(lookup["target"], "/docs/readme");
    assert_eq!(lookup["scores"], json!([0.5]));
    let stored = &out["stored"];
    assert_eq!(stored["name"], "src");
    assert_eq!(stored["kind"], "directory");
    assert_eq!(stored["tags"], json!(["code", client_tag]));
    assert_eq!(stored["owner"], json!({"uid": 1000, "gid": 100}));
    assert_eq!(stored["target"], "/srv/src");
    assert_eq!(stored["scores"], json!([1.5, -2.25]));
    assert_eq!(stored["hidden"], false);
    let child = &stored["children"][0];
    assert_eq!(child["which"], "blob");
    assert_eq!(child["blob"], "00ff6869");
    assert_eq!(child["priority"], -3);
    assert_eq!(out["count"], 2);
    assert!(
        out["fail"]
            .as_str()
            .unwrap()
            .contains("refused: on purpose"),
        "{out}"
    );
    assert_eq!(out["pipelined"], json!([0, 2, 4, 6, 8]));
}

#[tokio::test]
async fn pycapnp_client() {
    let (state, id, addr) = start(handlers(), SCHEMA).await;
    let out = run(
        env_path("NETGET_CAPNP_PYTHON"),
        &["-I", PY_PEER, "client", &addr.to_string(), SCHEMA],
    )
    .await;
    check(&out, "rust");
    assert_eq!(out["fail_type"], "FAILED");
    assert!(handler_saw(&state, id, "\"name\":\"main.rs\"").await);
}

#[tokio::test]
async fn go_capnp_client() {
    let (state, id, addr) = start(handlers(), SCHEMA).await;
    let out = run(
        env_path("NETGET_CAPNP_GO_PEER"),
        &["client", &addr.to_string()],
    )
    .await;
    check(&out, "go");
    assert!(handler_saw(&state, id, "\"name\":\"main.go\"").await);
}
