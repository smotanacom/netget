//! Independent 9P2000 clients against NetGet's server, failing rather than skipping when
//! absent: 9fans.net/go's plan9/client and knusbaum/go9p's client, both through the Go peer
//! in `tests/client/ninep/peer` (`install_peers.py` builds it; NETGET_NINEP_PEER names it).
//! Each lists, reads, stats, pages a 300-entry directory, creates and writes, and reports the
//! server's refusals as text.
use super::wire_test::{handlers, start};
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

async fn peer(client: &str, addr: &str) -> Value {
    let program = std::env::var_os("NETGET_NINEP_PEER").map(PathBuf::from).expect("NETGET_NINEP_PEER is required: run tests/client/ninep/install_peers.py <root> and export what it prints");
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(program)
            .args([client, addr])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .expect("start the 9P peer");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

async fn handler_saw(
    state: &netget::state::app_state::AppState,
    id: netget::state::ServerId,
    needle: &str,
) -> bool {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .any(|e| e.contains(needle))
}

const ROOT: [&str; 6] = [
    "bin.dat",
    "docs",
    "many",
    "readme.txt",
    "readonly.txt",
    "scratch",
];

#[tokio::test]
async fn plan9port_9fans_client() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let out = peer("9fans", &addr.to_string()).await;
    assert_eq!(
        out,
        json!({
            "root": ROOT, "root_error": "",
            "readme": "hello from netget\n", "readme_error": "",
            "bin": "00ff10",
            "stat": {"name": "guide.md", "length": 8, "mode": 0o644, "uid": "glenda", "mtime": 1_700_000_000, "dir": false},
            "many": 300, "many_error": "",
            "written": 16, "write_error": "",
            "rename_error": "", "remove_error": "",
            "missing_error": "file does not exist",
            "denied_error": "permission denied",
        })
    );
    assert!(handler_saw(&state, id, "written by 9fans").await);
    assert!(
        handler_saw(&state, id, r#""name":"renamed.txt""#).await,
        "the rename reached the handler"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn knusbaum_go9p_client() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    let out = peer("go9p", &addr.to_string()).await;
    assert_eq!(
        out,
        json!({
            "root": ROOT,
            "readme": "hello from netget\n", "readme_error": "",
            "stat": {"name": "guide.md", "length": 8, "mode": 0o644, "uid": "glenda", "mtime": 1_700_000_000, "dir": false},
            "many": 300,
            "written": 15, "write_error": "",
            "missing_error": "file does not exist",
            "denied_error": "permission denied",
        })
    );
    assert!(handler_saw(&state, id, "written by go9p").await);
    state.remove_server(id).await;
}
