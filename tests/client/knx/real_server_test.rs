//! NetGet's KNX/IP client against knxd as its tunnelling gateway, failing rather than skipping
//! when absent. The client's connect handler switches 1/2/3 on and reads 1/2/4; knxtool,
//! listening on knxd's own socket, shows both arrived on knxd's bus from the tunnel's address,
//! and xknx — tunnelled into the same knxd — answers the read, which comes back to NetGet as a
//! decoded knx_telegram.
use super::session_test::{client, telegrams};
use crate::helpers::real_server::{InstallHint, RealServer};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/knx/install_peers.py <root> and export what it prints")
    })
}

#[tokio::test]
async fn netget_against_knxd() {
    // knxd as a KNXnet/IP tunnelling server on a probed UDP port, its own socket on another.
    let knxd = RealServer::builder(
        env_path("NETGET_KNXD").to_str().unwrap(),
        InstallHint {
            brew: "knxd",
            apt: "knxd knxd-tools",
        },
    )
    .args([
        "-e",
        "0.0.1",
        "-E",
        "0.0.2:8",
        "-n",
        "netget-test",
        "-T",
        "-S",
        "224.0.23.12:{port1}",
        "-i",
        "{port}",
        "-b",
        "dummy:",
    ])
    .extra_ports(1)
    .startup_timeout(Duration::from_secs(30))
    .start()
    .await
    .expect("start knxd");
    let url = format!("ip:{}", knxd.addr());
    let gateway = format!("127.0.0.1:{}", knxd.extra_ports[0]);
    let knxtool = env_path("NETGET_KNXTOOL");
    let mut listen = tokio::process::Command::new(&knxtool)
        .args(["groupsocketlisten", &url])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(listen.stdout.take().unwrap()).lines();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/server/knx/xknx_peer.py");
    let mut responder = tokio::process::Command::new(env_path("NETGET_KNX_PYTHON"))
        .args([
            "-I",
            script,
            "127.0.0.1",
            &knxd.extra_ports[0].to_string(),
            "respond",
        ])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut ready = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        BufReader::new(responder.stdout.take().unwrap()).read_line(&mut ready),
    )
    .await
    .expect("xknx responder did not start")
    .unwrap();
    assert_eq!(ready.trim(), "READY");

    let (state, id) = client(gateway).await;
    let mut got = Vec::new();
    let seen = tokio::time::timeout(Duration::from_secs(20), async {
        while let Ok(Some(line)) = lines.next_line().await {
            got.push(line);
            let all = got.join("\n");
            if all.contains("to 1/2/3: 01") && all.contains("Read from") && all.contains("to 1/2/4")
            {
                return;
            }
        }
    })
    .await;
    assert!(seen.is_ok(), "knxtool heard: {got:?}\nknxd: {}", knxd.log());
    // xknx's answer reached NetGet's client, decoded by the configured DPT.
    let response = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(t) = telegrams(&state, id)
                .await
                .into_iter()
                .find(|t| t["kind"] == "response")
            {
                break t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the read was answered");
    assert_eq!(response["destination"], "1/2/4");
    assert_eq!(response["value"], 19.25, "{response}");
    state.remove_client(id).await;
    listen.kill().await.unwrap();
    responder.kill().await.unwrap();
}
