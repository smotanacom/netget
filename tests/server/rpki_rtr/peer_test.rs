//! Two independent routers against NetGet's cache: StayRTR 0.6.4's `rtrdump` (Go) over
//! version 1 and version 0, with reset and serial queries; and RTRlib 0.8.0's `rtrclient` (C),
//! which stays connected, so Serial Notify can drive an incremental update with a withdrawal.
//! Both unchanged; both fail, never skip, when absent.
use crate::helpers::rpki_rtr::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::time::Duration;

async fn rtrdump_run(args: &[String]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dump.json");
    let mut command = tokio::process::Command::new(rtrdump());
    command
        .args(args)
        .arg("-file")
        .arg(&out)
        .arg("-type")
        .arg("plain")
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("rtrdump timed out")
        .unwrap();
    assert!(
        result.status.success(),
        "rtrdump failed: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&std::fs::read(&out).expect("rtrdump wrote no file")).unwrap()
}

fn roas(dump: &Value) -> Vec<(String, u64, u64)> {
    let mut v: Vec<_> = dump["roas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["prefix"].as_str().unwrap().to_owned(),
                r["maxLength"].as_u64().unwrap(),
                r["asn"].as_u64().unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn rtrdump_resets_over_version_1_and_version_0() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        cache_policy(),
        json!({"session_id": SESSION, "refresh_interval_secs": 900}),
    )
    .await;
    for version in ["1", "0"] {
        let dump = rtrdump_run(&[
            "-connect".into(),
            addr.to_string(),
            "-rtr.version".into(),
            version.into(),
        ])
        .await;
        assert_eq!(dump["metadata"]["serial"], 7, "{dump}");
        assert_eq!(dump["metadata"]["sessionid"], u64::from(SESSION), "{dump}");
        assert_eq!(
            roas(&dump),
            [
                ("192.0.2.0/24".into(), 24, 64496),
                ("2001:db8::/32".into(), 48, 64497)
            ],
            "version {version}"
        );
    }
    let server = AccessLogOwner::Server(sid.as_u32());
    let resets = logs(&state, server, "rpki_rtr_reset_query", 2).await;
    assert_eq!(resets[0].request["version"], 1);
    assert_eq!(resets[1].request["version"], 0);
    assert_eq!(resets[0].request["session_id"], u64::from(SESSION));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rtrdump_serial_query_gets_the_delta_or_a_cache_reset() {
    let state = state();
    let (sid, addr) = server_in(&state, cache_policy(), json!({"session_id": SESSION})).await;
    let dump = rtrdump_run(&[
        "-connect".into(),
        addr.to_string(),
        "-serial".into(),
        "-serial.value".into(),
        "7".into(),
        "-session.id".into(),
        SESSION.to_string(),
    ])
    .await;
    assert_eq!(dump["metadata"]["serial"], 8, "{dump}");
    // rtrdump lists every prefix PDU it receives, announcement or withdrawal alike.
    assert_eq!(
        roas(&dump),
        [
            ("192.0.2.0/24".into(), 24, 64496),
            ("198.51.100.0/22".into(), 24, 64498)
        ]
    );
    // Another session id: Rust answers Cache Reset without asking the handler.
    let dump = rtrdump_run(&[
        "-connect".into(),
        addr.to_string(),
        "-serial".into(),
        "-serial.value".into(),
        "7".into(),
        "-session.id".into(),
        (SESSION + 1).to_string(),
    ])
    .await;
    assert!(dump["roas"].as_array().unwrap().is_empty(), "{dump}");
    let server = AccessLogOwner::Server(sid.as_u32());
    let queries = logs(&state, server, "rpki_rtr_serial_query", 1).await;
    assert_eq!(queries[0].request["router_serial"], 7);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        count(
            &state.list_access_logs_for(Some(server), None).await,
            "rpki_rtr_serial_query"
        ),
        1
    );
    state.remove_server(sid).await;
}

/// RTRlib prints `+ <prefix> <min> - <max> <asn>` for each change; it flushes per line only on
/// a terminal, so it runs on a pty.
fn spawn_rtrclient(port: u16) -> (std::process::Child, std::sync::mpsc::Receiver<String>) {
    let (pty, pts) = pty_process::blocking::open().unwrap();
    let child = pty_process::blocking::Command::new(rtrclient())
        .args(["-p", "tcp", "127.0.0.1", &port.to_string()])
        .spawn(pts)
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pty).lines() {
            let Ok(line) = line else { return };
            if tx.send(line.trim_end().to_owned()).is_err() {
                return;
            }
        }
    });
    (child, rx)
}

fn changes(
    rx: &std::sync::mpsc::Receiver<String>,
    want: usize,
) -> Vec<(char, String, u32, u32, u32)> {
    let mut out = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while out.len() < want {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let line = rx
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("rtrclient printed only {out:?}"));
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let [sign @ ("+" | "-"), prefix, min, "-", max, asn] = parts.as_slice() {
            out.push((
                sign.chars().next().unwrap(),
                prefix.to_string(),
                min.parse().unwrap(),
                max.parse().unwrap(),
                asn.parse().unwrap(),
            ));
        }
    }
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn rtrlib_synchronizes_then_follows_serial_notify_to_an_incremental_withdrawal() {
    let state = state();
    let (sid, addr) = server_in(&state, cache_policy(), json!({"session_id": SESSION})).await;
    let (mut child, rx) = tokio::task::spawn_blocking(move || spawn_rtrclient(addr.port()))
        .await
        .unwrap();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let first = {
        let rx = rx.clone();
        tokio::task::spawn_blocking(move || changes(&rx.lock().unwrap(), 2))
            .await
            .unwrap()
    };
    assert_eq!(
        first,
        [
            ('+', "192.0.2.0".into(), 24, 24, 64496),
            ('+', "2001:db8::".into(), 32, 48, 64497)
        ]
    );
    let server = AccessLogOwner::Server(sid.as_u32());
    let conn = logs(&state, server, "rpki_rtr_reset_query", 1).await[0]
        .connection_id
        .expect("connection id");
    let sent = state
        .send_to_peer(
            sid,
            conn,
            json!({"type":"rpki_rtr_serial_notify","serial":8}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let second = {
        let rx = rx.clone();
        tokio::task::spawn_blocking(move || changes(&rx.lock().unwrap(), 2))
            .await
            .unwrap()
    };
    assert_eq!(
        second,
        [
            ('+', "198.51.100.0".into(), 22, 24, 64498),
            ('-', "192.0.2.0".into(), 24, 24, 64496)
        ]
    );
    let queries = logs(&state, server, "rpki_rtr_serial_query", 1).await;
    assert_eq!(queries[0].request["router_serial"], 7);
    let _ = child.kill();
    let _ = child.wait();
    state.remove_server(sid).await;
}
