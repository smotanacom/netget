//! Independent Minecraft clients against NetGet's server, each failing rather than skipping
//! when absent: mcstatus 14.2.0 (Python: the modern status and ping, and its legacy client)
//! and node-minecraft-protocol 1.68.0 (JavaScript: its ping, and a real `createClient` login
//! that must end on NetGet's Disconnect). `tests/client/minecraft/install_peers.py` installs
//! both and prints NETGET_MINECRAFT_PYTHON, NETGET_MINECRAFT_NODE_MODULES and
//! NETGET_MINECRAFT_PEER.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/minecraft/install_peers.py <root> and export what it prints"))
}

async fn run(program: PathBuf, args: &[String], node_path: Option<PathBuf>) -> Value {
    let mut command = tokio::process::Command::new(&program);
    command.args(args).kill_on_drop(true);
    if let Some(path) = node_path {
        command.env("NODE_PATH", path);
    }
    let out = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("peer deadline")
        .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

const MCSTATUS: &str = "import json, sys\nfrom mcstatus import JavaServer, LegacyServer\nport = int(sys.argv[1])\ns = JavaServer('127.0.0.1', port).status()\nlatency = JavaServer('127.0.0.1', port).ping()\nl = LegacyServer('127.0.0.1', port).status()\nprint(json.dumps({'motd': s.motd.to_plain(), 'online': s.players.online, 'max': s.players.max, 'sample': [[p.name, p.id] for p in s.players.sample], 'version': s.version.name, 'protocol': s.version.protocol, 'ping_ok': latency >= 0, 'legacy': [l.motd.to_plain(), l.players.online, l.players.max, l.version.name, l.version.protocol]}))";

#[tokio::test]
async fn mcstatus_reads_status_ping_and_legacy() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let out = run(
        env_path("NETGET_MINECRAFT_PYTHON"),
        &[
            "-I".into(),
            "-c".into(),
            MCSTATUS.into(),
            addr.port().to_string(),
        ],
        None,
    )
    .await;
    assert_eq!(
        out,
        json!({
            "motd": "NetGet via 127.0.0.1",
            "online": 3,
            "max": 50,
            "sample": [["alice", "a6f39b1e-3a43-4e2b-9b0c-1d2f3a4b5c6d"], ["bob", "00000000-0000-0000-0000-000000000000"]],
            "version": "1.21.1",
            "protocol": 767,
            "ping_ok": true,
            "legacy": ["Legacy NetGet", 3, 50, "1.6.4", 74],
        })
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn node_minecraft_protocol_pings_and_is_refused_at_login() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let node_path = env_path("NETGET_MINECRAFT_NODE_MODULES");
    let peer = env_path("NETGET_MINECRAFT_PEER").display().to_string();
    let port = addr.port().to_string();
    let out = run(
        "node".into(),
        &[peer.clone(), "ping".into(), port.clone()],
        Some(node_path.clone()),
    )
    .await;
    assert_eq!(
        out["version"],
        json!({"name": "1.21.1", "protocol": 767}),
        "{out}"
    );
    assert_eq!(
        (
            out["players"]["online"].clone(),
            out["players"]["max"].clone()
        ),
        (json!(3), json!(50))
    );
    assert_eq!(out["description"], json!({"text": "NetGet via 127.0.0.1"}));
    assert!(out["latency"].is_number(), "nmp measured the pong: {out}");
    let out = run(
        "node".into(),
        &[peer, "login".into(), port, "steve".into()],
        Some(node_path),
    )
    .await;
    assert_eq!(
        out,
        json!({"event": "disconnect", "state": "login", "reason": {"text": "Sorry steve, whitelist only"}})
    );
    state.remove_server(id).await;
}
