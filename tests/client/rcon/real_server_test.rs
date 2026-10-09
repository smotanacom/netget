//! NetGet's RCON client against gorcon/rcon v1.4.0's rcontest server, unchanged, through the
//! Go peer in `peer/` (`install_peers.py` builds it; NETGET_RCON_PEER names it). rcontest
//! answers each command with one packet and does not mirror the Source sentinel, so the client
//! runs in the minecraft dialect here; Source multi-packet collection is covered by
//! `session_test.rs` against a fixture and against NetGet's own server.
use super::session_test::{client, wait_log};
use serde_json::json;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn netget_runs_commands_on_gorcon_rcontest() {
    let peer = std::env::var_os("NETGET_RCON_PEER")
        .map(PathBuf::from)
        .expect("NETGET_RCON_PEER is required: run tests/client/rcon/install_peers.py <root> and export what it prints");
    let mut child = tokio::process::Command::new(peer)
        .args(["server", "hunter2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the rcontest peer");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut ready))
        .await
        .expect("rcontest did not start")
        .unwrap();
    let addr = ready
        .trim()
        .strip_prefix("READY ")
        .expect("READY line")
        .to_string();
    let (state, id) = client(
        addr.clone(),
        json!({"password":"hunter2","dialect":"minecraft"}),
        json!([{"type":"rcon_command","command":"players"}]),
    )
    .await
    .unwrap();
    wait_log(
        &state,
        id,
        "There are 2 of a max of 20 players online: alice, bob",
    )
    .await;
    state
        .send_to_client(
            id,
            json!({"type":"rcon_command","command":"seed"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    wait_log(&state, id, "Seed: [-4172144997902289642]").await;
    state.remove_client(id).await;
    let refused = client(
        addr,
        json!({"password":"wrong","dialect":"minecraft"}),
        json!([]),
    )
    .await;
    assert!(
        refused
            .err()
            .is_some_and(|e| e.contains("refused the password")),
        "rcontest refused the wrong password"
    );
    child.kill().await.unwrap();
}
