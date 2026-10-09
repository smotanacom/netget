//! NetGet's router against StayRTR 0.6.4 — an independent Go cache — reading a VRP file that
//! the test rewrites. StayRTR raises the serial and sends Serial Notify; the router's
//! Rust-owned Serial Query must bring back exactly the change. Version 0 is negotiated too.
//! Fails, never skips, when the peer is absent.
use crate::helpers::rpki_rtr::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;

fn vrp_file(path: &std::path::Path, roas: Value) {
    let doc = json!({"metadata": {"vrps": roas.as_array().unwrap().len(), "bgpsec_pubkeys": 0}, "roas": roas});
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&doc).unwrap()).unwrap();
    std::fs::rename(tmp, path).unwrap();
}

struct StayRtr {
    child: tokio::process::Child,
    addr: String,
    file: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn stayrtr_with(roas: Value) -> StayRtr {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("vrps.json");
    vrp_file(&file, roas);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}");
    let child = tokio::process::Command::new(stayrtr())
        .args(["-bind", &addr, "-cache"])
        .arg(&file)
        .args([
            "-checktime=false",
            "-refresh",
            "1",
            "-metrics.addr",
            "",
            "-rtr.refresh",
            "900",
            "-loglevel",
            "error",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("start stayrtr");
    assert!(
        wait_until(Duration::from_secs(20), || {
            let addr = addr.clone();
            async move { tokio::net::TcpStream::connect(&addr).await.is_ok() }
        })
        .await,
        "stayrtr never listened on {addr}"
    );
    StayRtr {
        child,
        addr,
        file,
        _dir: dir,
    }
}

fn initial() -> Value {
    json!([
        {"prefix":"192.0.2.0/24","maxLength":24,"asn":"AS64496"},
        {"prefix":"2001:db8::/32","maxLength":48,"asn":64497}
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn router_syncs_from_stayrtr_and_follows_its_notify_to_the_delta() {
    let mut peer = stayrtr_with(initial()).await;
    let state = state();
    let cid = client_in(&state, peer.addr.clone(), quiet_router(), json!({}))
        .await
        .unwrap();
    let router = AccessLogOwner::Client(cid.as_u32());
    let first = logs(&state, router, "rpki_rtr_synchronized", 1).await[0]
        .request
        .clone();
    assert_eq!(first["kind"], "reset");
    assert_eq!(
        (first["announced"].as_u64(), first["withdrawn"].as_u64()),
        (Some(2), Some(0))
    );
    let mut prefixes: Vec<_> = first["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["prefix"].as_str().unwrap().to_owned(),
                r["max_length"].as_u64().unwrap(),
                r["asn"].as_u64().unwrap(),
            )
        })
        .collect();
    prefixes.sort();
    assert_eq!(
        prefixes,
        [
            ("192.0.2.0/24".to_owned(), 24, 64496),
            ("2001:db8::/32".to_owned(), 48, 64497)
        ]
    );
    assert_eq!(
        first["intervals"]["refresh"], 900,
        "StayRTR's -rtr.refresh reached the router"
    );
    let first_serial = first["serial"].as_u64().unwrap();

    vrp_file(
        &peer.file,
        json!([
            {"prefix":"2001:db8::/32","maxLength":48,"asn":64497},
            {"prefix":"198.51.100.0/22","maxLength":24,"asn":64498}
        ]),
    );
    let second = logs(&state, router, "rpki_rtr_synchronized", 2).await[1]
        .request
        .clone();
    assert_eq!(second["kind"], "incremental");
    assert_eq!(second["session_id"], first["session_id"]);
    assert_eq!(second["serial"].as_u64(), Some(first_serial + 1));
    assert_eq!(
        (second["announced"].as_u64(), second["withdrawn"].as_u64()),
        (Some(1), Some(1))
    );
    let records = second["records"].as_array().unwrap();
    assert!(records
        .iter()
        .any(|r| r["prefix"] == "198.51.100.0/22" && r["announcement"] == true));
    assert!(records
        .iter()
        .any(|r| r["prefix"] == "192.0.2.0/24" && r["announcement"] == false));
    state.remove_client(cid).await;
    let _ = peer.child.kill().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn router_speaks_version_0_to_stayrtr() {
    let mut peer = stayrtr_with(initial()).await;
    let state = state();
    let cid = client_in(
        &state,
        peer.addr.clone(),
        quiet_router(),
        json!({"version": 0, "refresh_interval_secs": 300}),
    )
    .await
    .unwrap();
    let sync = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "rpki_rtr_synchronized",
        1,
    )
    .await[0]
        .request
        .clone();
    assert_eq!(sync["announced"], 2);
    assert_eq!(
        sync["intervals"]["refresh"], 300,
        "version 0 carries no timers"
    );
    state.remove_client(cid).await;
    let _ = peer.child.kill().await;
}
