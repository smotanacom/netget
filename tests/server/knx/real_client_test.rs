//! Independent KNXnet/IP tunnelling clients against NetGet's gateway, failing rather than
//! skipping when absent: xknx (Python) and knxd (C++), which connects to NetGet as its own
//! tunnelling uplink (`-b ipt:`) and is driven with knxtool. Peers from `install_peers.py`.
use super::wire_test::{handlers, start};
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

pub fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/knx/install_peers.py <root> and export what it prints")
    })
}

async fn saw(state: &AppState, id: ServerId, needle: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .iter()
                .any(|e| serde_json::to_string(e).unwrap().contains(needle))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn xknx_client() {
    let (state, id, gw) = start(handlers(), json!({})).await;
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/server/knx/xknx_peer.py");
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(env_path("NETGET_KNX_PYTHON"))
            .args(["-I", script, "127.0.0.1", &gw.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("xknx deadline")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let out: Value = serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)));
    assert!(
        out["address"].as_str().unwrap().starts_with("1.1."),
        "{out}"
    );
    assert_eq!(out["temperature"], 21.5, "{out}");
    assert_eq!(out["text"], "NetGet");
    assert_eq!(out["unanswered"], true);
    // The handler's feedback for the switch reached xknx from the gateway's address.
    let heard = out["heard"].as_array().unwrap();
    assert!(
        heard.iter().any(|t| t["destination"] == "1/2/10"
            && t["source"] == "1.1.250"
            && t["kind"] == "GroupValueWrite"
            && t["value"] == 1),
        "{out}"
    );
    assert!(saw(&state, id, "\"destination\":\"1/2/3\"").await);
}

#[tokio::test]
async fn knxd_tunnelling_uplink_driven_by_knxtool() {
    let (state, id, gw) = start(handlers(), json!({})).await;
    let knxd = crate::helpers::real_server::RealServer::builder(
        env_path("NETGET_KNXD").to_str().unwrap(),
        crate::helpers::real_server::InstallHint {
            brew: "knxd",
            apt: "knxd knxd-tools",
        },
    )
    .args([
        "-e".to_string(),
        "0.0.1".into(),
        "-E".into(),
        "0.0.2:2".into(),
        "-i".into(),
        "{port}".into(),
        "-b".into(),
        format!("ipt:127.0.0.1:{}", gw.port()),
    ])
    .startup_timeout(Duration::from_secs(30))
    .start()
    .await
    .expect("start knxd against NetGet");
    let url = format!("ip:{}", knxd.addr());
    let knxtool = env_path("NETGET_KNXTOOL");
    let mut listen = tokio::process::Command::new(&knxtool)
        .args(["groupsocketlisten", &url])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(listen.stdout.take().unwrap()).lines();
    tokio::time::sleep(Duration::from_millis(500)).await;
    for args in [
        vec!["groupswrite", &url, "1/2/3", "1"],
        vec!["groupread", &url, "1/2/4"],
    ] {
        let st = tokio::process::Command::new(&knxtool)
            .args(&args)
            .status()
            .await
            .unwrap();
        assert!(st.success(), "{args:?}");
    }
    let mut got = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(20), async {
        while let Ok(Some(line)) = lines.next_line().await {
            got.push(line.clone());
            let joined = got.join("\n");
            if joined.contains("Write from 1.1.250 to 1/2/10: 01")
                && joined.contains("Response from 1.1.250 to 1/2/4: 0C 33")
            {
                return;
            }
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "knxtool heard: {got:?}\nknxd: {}",
        knxd.log()
    );
    assert!(saw(&state, id, "\"destination\":\"1/2/3\"").await);
    listen.kill().await.unwrap();
}
