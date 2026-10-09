//! NetGet's charge point against python ocpp 2.1.0's central system — independent and
//! schema-validating, unchanged — over 1.6 and 2.0.1. The charge point's own handlers walk the
//! core workflow; the central system sends Reset after boot, which the charge point accepts.
//! Fails, never skips, when the peer is absent.
use crate::helpers::ocpp::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

async fn against_python_csms(version: &str, expected: &[&str]) {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .args(["csms", version])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let port = first["port"].as_u64().unwrap();
    let state = state();
    let cid = client_in(
        &state,
        format!("127.0.0.1:{port}"),
        charge_point_policy(version),
        json!({"charge_point_id":"CP-NG","ocpp_version":version}),
    )
    .await
    .unwrap();
    let mut seen: Vec<Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while (seen.iter().filter(|s| s.get("call").is_some()).count() < expected.len()
        || !seen.iter().any(|s| s.get("reset_status").is_some()))
        && tokio::time::Instant::now() < deadline
    {
        if let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await
        {
            seen.push(serde_json::from_str(&line).unwrap());
        }
    }
    let connected = seen
        .iter()
        .find(|s| s.get("connected").is_some())
        .expect("central system saw the connection");
    assert_eq!(
        (
            connected["connected"].as_str(),
            connected["subprotocol"].as_str()
        ),
        (
            Some("CP-NG"),
            Some(if version == "1.6" {
                "ocpp1.6"
            } else {
                "ocpp2.0.1"
            })
        )
    );
    let calls: Vec<&str> = seen.iter().filter_map(|s| s["call"].as_str()).collect();
    assert_eq!(calls, expected, "{seen:?}");
    assert_eq!(
        seen.iter().find_map(|s| s["reset_status"].as_str()),
        Some("Accepted"),
        "{seen:?}"
    );
    let router = AccessLogOwner::Client(cid.as_u32());
    let responses = logs(&state, router, "ocpp_call_response", expected.len()).await;
    assert_eq!(responses[0].request["payload"]["status"], "Accepted");
    assert_eq!(responses[0].request["payload"]["interval"], 10);
    let csms_calls = logs(&state, router, "ocpp_csms_call", 1).await;
    assert_eq!(csms_calls[0].request["action"], "Reset");
    state.remove_client(cid).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn charge_point_walks_16_against_python_central_system() {
    against_python_csms(
        "1.6",
        &[
            "BootNotification",
            "Heartbeat",
            "StatusNotification",
            "Authorize",
            "StartTransaction",
            "StopTransaction",
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn charging_station_walks_201_against_python_central_system() {
    against_python_csms(
        "2.0.1",
        &[
            "BootNotification",
            "Heartbeat",
            "StatusNotification",
            "Authorize",
            "TransactionEvent",
        ],
    )
    .await;
}
