//! Independent Consul clients against NetGet's agent API, failing rather than skipping when
//! absent: the official consul CLI (HashiCorp's Go api package) and py-consul. Peers from
//! `install_peers.py`.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

pub fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/consul/install_peers.py <root> and export what it prints")
    })
}

async fn consul(addr: &str, args: &[&str]) -> (bool, String) {
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(env_path("NETGET_CONSUL_BIN"))
            .args(args)
            .env("CONSUL_HTTP_ADDR", addr)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("consul CLI deadline")
    .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text.trim().to_string())
}

#[tokio::test]
async fn consul_cli() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, _id, addr) = start(handlers(&dir.path().join("consul.json"))).await;
    let a = format!("127.0.0.1:{}", addr.port());
    let ok = |r: (bool, String)| {
        assert!(r.0, "{}", r.1);
        r.1
    };
    assert!(ok(consul(&a, &["kv", "put", "app/config", "hello world"]).await).contains("Success!"));
    assert_eq!(
        ok(consul(&a, &["kv", "get", "app/config"]).await),
        "hello world"
    );
    ok(consul(&a, &["kv", "put", "-flags=42", "app/flags", "x"]).await);
    let detailed = ok(consul(&a, &["kv", "get", "-detailed", "app/flags"]).await);
    assert!(
        detailed
            .lines()
            .any(|l| l.starts_with("Flags") && l.ends_with("42")),
        "{detailed}"
    );
    let modify: u64 = detailed
        .lines()
        .find_map(|l| l.strip_prefix("ModifyIndex"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        ok(consul(&a, &["kv", "get", "-keys", "app/"]).await),
        "app/config\napp/flags"
    );
    let recurse = ok(consul(&a, &["kv", "get", "-recurse", "app/"]).await);
    assert_eq!(recurse, "app/config:hello world\napp/flags:x");
    let stale = consul(
        &a,
        &["kv", "put", "-cas", "-modify-index=1", "app/flags", "nope"],
    )
    .await;
    assert!(!stale.0 && stale.1.contains("CAS failed"), "{}", stale.1);
    let current = format!("-modify-index={modify}");
    ok(consul(&a, &["kv", "put", "-cas", &current, "app/flags", "y"]).await);
    let denied = consul(&a, &["kv", "put", "locked/x", "y"]).await;
    assert!(
        !denied.0 && denied.1.contains("Permission denied"),
        "{}",
        denied.1
    );
    ok(consul(&a, &["kv", "delete", "app/flags"]).await);
    let missing = consul(&a, &["kv", "get", "app/flags"]).await;
    assert!(
        !missing.0 && missing.1.contains("No key exists"),
        "{}",
        missing.1
    );
    ok(consul(
        &a,
        &[
            "services",
            "register",
            "-name=web",
            "-id=web1",
            "-port=8080",
            "-tag=v1",
        ],
    )
    .await);
    let tagged = ok(consul(&a, &["catalog", "services", "-tags"]).await);
    assert_eq!(tagged.split_whitespace().collect::<Vec<_>>(), ["web", "v1"]);
    ok(consul(&a, &["services", "deregister", "-id=web1"]).await);
    assert!(ok(consul(&a, &["catalog", "services"]).await).starts_with("No services match"));
}

#[tokio::test]
async fn py_consul() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, _id, addr) = start(handlers(&dir.path().join("consul.json"))).await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/consul/py_consul_peer.py"
    );
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(env_path("NETGET_CONSUL_PYTHON"))
            .args(["-I", script, "127.0.0.1", &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("py-consul deadline")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let out: Value = serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(out["put"], true);
    assert_eq!(out["get"]["value"], "hello from python", "{out}");
    assert_eq!(out["get"]["flags"], 3);
    assert_eq!(
        (out["cas_stale"].clone(), out["cas_current"].clone()),
        (json!(false), json!(true))
    );
    assert_eq!(out["keys"], json!(["py/greeting", "py/other"]));
    assert_eq!(
        out["recursed"],
        json!({"py/greeting": "updated", "py/other": "x"})
    );
    assert_eq!(
        (out["delete"].clone(), out["gone"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(out["services"], json!({"api": ["py"]}));
    assert_eq!(
        out["catalog"],
        json!([{"id": "api1", "port": 9000, "address": "127.0.0.9"}])
    );
    assert_eq!(out["health"], json!([{"id": "api1", "status": "passing"}]));
    assert_eq!(out["after"], json!({}));
}
