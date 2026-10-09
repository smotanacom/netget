//! Two independent Socket.IO clients, unchanged, against NetGet's server: the reference
//! socket.io-client 4.8.4 (JavaScript) and python-socketio 5.17.0, each over long-polling
//! upgraded to WebSocket and over WebSocket only. Each connects, gets the greeting, emits
//! with an acknowledgement, receives the broadcast, acknowledges an event the server sends,
//! connects /admin with the right auth, is refused it with the wrong one and refused an
//! unknown namespace, and is disconnected by the server. Fails, never skips.
use crate::helpers::socketio::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

async fn run(mut cmd: tokio::process::Command) -> HashMap<String, Value> {
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.kill_on_drop(true).output())
        .await
        .expect("peer timed out")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| (v["step"].as_str().unwrap_or_default().to_owned(), v))
        .collect()
}

async fn server() -> (
    netget::state::app_state::AppState,
    netget::state::ServerId,
    std::net::SocketAddr,
) {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        chat_policy(),
        json!({"namespaces": ["/", "/admin"], "ping_interval_ms": 2000, "ping_timeout_ms": 2000}),
    )
    .await;
    (state, sid, addr)
}

#[tokio::test(flavor = "multi_thread")]
async fn reference_javascript_client_over_polling_upgrade_and_websocket() {
    let (state, sid, addr) = server().await;
    for mode in ["upgrade", "websocket-only"] {
        let mut cmd = tokio::process::Command::new("node");
        cmd.env("NODE_PATH", node_modules())
            .arg(js_peer())
            .arg(format!("http://{addr}/"))
            .arg(mode);
        let steps = run(cmd).await;
        assert_eq!(
            steps["transport"]["name"], "websocket",
            "{mode}: polling upgrades to WebSocket"
        );
        assert_eq!(steps["welcome"]["args"], json!(["netget", "/"]));
        assert_eq!(steps["ack"]["args"], json!("delivered"));
        assert_eq!(
            steps["broadcast"]["args"],
            json!(["broadcast", "hi from js"])
        );
        assert_eq!(
            steps["server_ack"]["args"],
            json!(["pong from js", "are you there?"])
        );
        assert!(steps["admin"]["id"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(steps["admin_denied"]["message"], "not authorized");
        assert_eq!(steps["unknown_namespace"]["message"], "Invalid namespace");
        assert_eq!(steps["server_disconnect"]["reason"], "io server disconnect");
    }
    let owner = AccessLogOwner::Server(sid.as_u32());
    let connects = logs(&state, owner, "socketio_connect", 2).await;
    assert!(
        connects.iter().any(|c| c.request["transport"] == "polling"),
        "the upgrade run connected over polling first"
    );
    assert!(connects
        .iter()
        .any(|c| c.request["transport"] == "websocket"));
    let acks = logs(&state, owner, "socketio_ack_received", 2).await;
    assert_eq!(acks[0].request["event"], "ping me");
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_socketio_client_over_polling_upgrade_and_websocket() {
    let (state, sid, addr) = server().await;
    for mode in ["upgrade", "websocket-only"] {
        let mut cmd = tokio::process::Command::new(python());
        cmd.arg(peer_script())
            .args(["client", &format!("http://{addr}")]);
        if mode == "websocket-only" {
            cmd.arg("websocket-only");
        }
        let steps = run(cmd).await;
        assert_eq!(steps["welcome"]["args"], json!(["netget", "/"]));
        assert_eq!(steps["ack"]["args"], json!(["delivered", "hi from python"]));
        assert_eq!(
            steps["broadcast"]["args"],
            json!(["broadcast", "hi from python"])
        );
        assert_eq!(
            steps["server_ack"]["args"],
            json!(["pong from python", "are you there?"])
        );
        assert_eq!(steps["admin"]["connected"], true);
        assert_eq!(steps["admin_denied"]["connected"], false);
        assert_eq!(steps["unknown_namespace"]["connected"], false);
        assert!(steps.contains_key("server_disconnect"));
    }
    let owner = AccessLogOwner::Server(sid.as_u32());
    let events = logs(&state, owner, "socketio_event", 6).await;
    assert!(events
        .iter()
        .any(|e| e.request["event"] == "chat message" && e.request["ack_requested"] == true));
    state.remove_server(sid).await;
}
