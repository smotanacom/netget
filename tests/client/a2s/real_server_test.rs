//! NetGet's A2S client against woozymasta/a2s v0.4.0's UDP server, unchanged, through the Go
//! peer in `peer/` (`install_peers.py` builds it; NETGET_A2S_PEER names it). Fails rather than
//! skips without it.
use super::session_test::{client, wait_log};
use serde_json::json;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn netget_queries_woozymasta_server() {
    let peer = std::env::var_os("NETGET_A2S_PEER").map(PathBuf::from).expect("NETGET_A2S_PEER is required: run tests/client/a2s/install_peers.py <root> and export what it prints");
    let mut child = tokio::process::Command::new(peer)
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the A2S server peer");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut ready))
        .await
        .expect("A2S peer did not start")
        .unwrap();
    let addr = ready
        .trim()
        .strip_prefix("READY ")
        .expect("READY line")
        .to_string();
    let (state, id) = client(addr, json!([{"type":"a2s_query","query":"info"}])).await;
    let info = wait_log(&state, id, "Independent A2S").await;
    assert!(
        info.contains(r#""map":"cp_badlands""#)
            && info.contains(r#""app_id":440"#)
            && info.contains(r#""max_players":24"#),
        "{info}"
    );
    state
        .send_to_client(
            id,
            json!({"type":"a2s_query","query":"players"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let players = wait_log(&state, id, r#""name":"bob""#).await;
    assert!(
        players.contains(r#""duration_secs":90.0"#) || players.contains(r#""duration_secs":90"#),
        "{players}"
    );
    state
        .send_to_client(
            id,
            json!({"type":"a2s_query","query":"rules"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    wait_log(&state, id, r#""mp_timelimit":"30""#).await;
    state.remove_client(id).await;
    child.kill().await.unwrap();
}
