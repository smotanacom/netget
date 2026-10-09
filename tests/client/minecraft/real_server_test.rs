//! NetGet's Minecraft client against node-minecraft-protocol 1.68.0's `createServer`,
//! unchanged (`install_peers.py` installs it; NETGET_MINECRAFT_NODE_MODULES and
//! NETGET_MINECRAFT_PEER name it). An offline-mode server answers status and the legacy ping,
//! accepts one login through compression and kicks another in the login state; an
//! online-mode server asks for encryption. Fails rather than skips without the peer.
use super::session_test::{client, send, wait_log};
use serde_json::json;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/minecraft/install_peers.py <root> and export what it prints"))
}

async fn nmp_server(online: bool) -> (tokio::process::Child, String) {
    let mut command = tokio::process::Command::new("node");
    command
        .arg(env_path("NETGET_MINECRAFT_PEER"))
        .arg("server")
        .env("NODE_PATH", env_path("NETGET_MINECRAFT_NODE_MODULES"))
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    if online {
        command.arg("online");
    }
    let mut child = command
        .spawn()
        .expect("start node (Node >= 18 must be on PATH)");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(60), stdout.read_line(&mut ready))
        .await
        .expect("the nmp server did not start")
        .unwrap();
    let addr = ready
        .trim()
        .strip_prefix("READY ")
        .unwrap_or_else(|| panic!("READY line, got {ready:?}"))
        .to_string();
    (child, addr)
}

#[tokio::test]
async fn netget_pings_and_joins_node_minecraft_protocol() {
    let (mut offline, addr) = nmp_server(false).await;
    let (state, id) = client(addr, json!([{"type":"minecraft_status"}])).await;
    let event = wait_log(&state, id, "Independent nmp server").await;
    for needle in [
        r#""max_players":33"#,
        r#""online_players":2"#,
        r#""version_name":"1.21.1""#,
        r#""protocol":767"#,
        r#""name":"steve""#,
        r#""id":"8667ba71-b85a-4004-af54-457a9734eed7""#,
    ] {
        assert!(event.contains(needle), "{needle} in {event}");
    }
    assert!(
        !event.contains(r#""latency_ms":null"#),
        "nmp answered the ping: {event}"
    );
    send(&state, id, json!({"type":"minecraft_legacy_status"})).await;
    let event = wait_log(&state, id, r#""legacy":true"#).await;
    assert!(
        event.contains("Independent nmp server") && event.contains(r#""max_players":33"#),
        "{event}"
    );
    // Accepted: nmp enables compression (threshold 256) and sends Login Success.
    send(
        &state,
        id,
        json!({"type":"minecraft_login","username":"alice"}),
    )
    .await;
    let event = wait_log(&state, id, r#""outcome":"accepted""#).await;
    assert!(
        event.contains(r#""username":"alice""#) && event.contains(r#""compression_threshold":256"#),
        "{event}"
    );
    // Kicked in the login state.
    send(
        &state,
        id,
        json!({"type":"minecraft_login","username":"banned_bob"}),
    )
    .await;
    let event = wait_log(&state, id, "You are banned: banned_bob").await;
    assert!(event.contains(r#""outcome":"disconnected""#), "{event}");
    state.remove_client(id).await;
    offline.kill().await.unwrap();

    let (mut online, addr) = nmp_server(true).await;
    let (state, id) = client(addr, json!([{"type":"minecraft_login","username":"carol"}])).await;
    wait_log(&state, id, r#""outcome":"encryption_required""#).await;
    state.remove_client(id).await;
    online.kill().await.unwrap();
}
