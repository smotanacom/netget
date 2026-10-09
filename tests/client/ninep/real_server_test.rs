//! NetGet's 9P client against knusbaum/go9p v1.18.0's in-memory file server, unchanged,
//! through the Go peer in `peer/` (`install_peers.py` builds it; NETGET_NINEP_PEER names it).
//! Reads its files, creates, writes, appends and reads back what the independent server
//! stored, makes a directory and removes a file. Fails rather than skips without the peer.
use super::session_test::{client, op, wait_log};
use serde_json::json;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn netget_uses_go9p_file_server() {
    let peer = std::env::var_os("NETGET_NINEP_PEER").map(PathBuf::from).expect("NETGET_NINEP_PEER is required: run tests/client/ninep/install_peers.py <root> and export what it prints");
    let mut child = tokio::process::Command::new(peer)
        .arg("server")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the 9P peer");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(30), stdout.read_line(&mut ready))
        .await
        .expect("9P peer did not start")
        .unwrap();
    let addr = ready
        .trim()
        .strip_prefix("READY ")
        .expect("READY line")
        .to_string();
    let (state, id) = client(addr, json!([{"type":"ninep_ls","path":"/"}])).await;
    let event = wait_log(&state, id, r#""op":"ls""#).await;
    assert!(
        event.contains(r#""name":"hello.txt""#) && event.contains(r#""name":"sub""#),
        "{event}"
    );
    let r = op(&state, id, json!({"type":"ninep_cat","path":"/hello.txt"})).await;
    assert_eq!(r["data"], "hello from go9p\n");
    let r = op(
        &state,
        id,
        json!({"type":"ninep_cat","path":"/sub/nested.txt"}),
    )
    .await;
    assert_eq!(r["data"], "nested file\n");
    let r = op(&state, id, json!({"type":"ninep_stat","path":"/sub"})).await;
    assert_eq!(
        (r["stat"]["kind"].clone(), r["stat"]["owner"].clone()),
        (json!("dir"), json!("glenda"))
    );
    let r = op(
        &state,
        id,
        json!({"type":"ninep_write","path":"/new.txt","data":"first line\n","create":true}),
    )
    .await;
    assert_eq!(r["bytes_written"], 11, "{r}");
    let r = op(
        &state,
        id,
        json!({"type":"ninep_write","path":"/new.txt","data":"second line\n","append":true}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    let r = op(&state, id, json!({"type":"ninep_cat","path":"/new.txt"})).await;
    assert_eq!(
        r["data"], "first line\nsecond line\n",
        "the independent server stored both writes"
    );
    let r = op(&state, id, json!({"type":"ninep_mkdir","path":"/made"})).await;
    assert_eq!(r["ok"], true, "{r}");
    let r = op(&state, id, json!({"type":"ninep_ls","path":"/"})).await;
    let names: Vec<String> = r["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"made".to_string()) && names.contains(&"new.txt".to_string()),
        "{names:?}"
    );
    let r = op(&state, id, json!({"type":"ninep_remove","path":"/new.txt"})).await;
    assert_eq!(r["ok"], true, "{r}");
    let r = op(&state, id, json!({"type":"ninep_cat","path":"/new.txt"})).await;
    assert_eq!(r["ok"], false, "removed: {r}");
    state.remove_client(id).await;
    child.kill().await.unwrap();
}
