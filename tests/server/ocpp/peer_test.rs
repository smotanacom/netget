//! python ocpp 2.1.0 — an independent, schema-validating OCPP implementation, unchanged —
//! as a charge point against NetGet's CSMS over OCPP 1.6 and 2.0.1: boot, heartbeat, status,
//! authorize, the transaction workflow, meter values, a NotSupported CALLERROR, and a
//! CSMS-initiated remote start it must accept. Fails, never skips, when absent.
use crate::helpers::ocpp::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

async fn walk(version: &str) {
    let state = state();
    let (sid, addr) = server_in(&state, csms_policy(), json!({})).await;
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .args(["cp", "127.0.0.1", &addr.port().to_string(), version])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut steps: Vec<Value> = Vec::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("charge point stalled")
            .unwrap()
            .expect("charge point exited");
        let v: Value = serde_json::from_str(&line).unwrap();
        let done = v["step"] == "done";
        steps.push(v);
        if done {
            break;
        }
    }
    let step = |n: &str| {
        steps
            .iter()
            .find(|s| s["step"] == n)
            .cloned()
            .unwrap_or_else(|| panic!("no {n} step: {steps:?}"))
    };
    assert_eq!(
        step("connected")["subprotocol"],
        if version == "1.6" {
            "ocpp1.6"
        } else {
            "ocpp2.0.1"
        }
    );
    assert_eq!(
        (
            step("boot")["status"].as_str(),
            step("boot")["interval"].as_u64()
        ),
        (Some("Accepted"), Some(10))
    );
    assert!(step("heartbeat")["current_time"].as_str().is_some());
    assert_eq!(step("authorize")["status"], "Accepted");
    if version == "1.6" {
        assert_eq!(step("start")["transaction_id"], 7);
        assert_eq!(
            step("datatransfer")["error"],
            "NotSupportedError",
            "the handler's CALLERROR reached the peer as NotSupported"
        );
    }
    let server = AccessLogOwner::Server(sid.as_u32());
    let calls = logs(
        &state,
        server,
        "ocpp_call",
        if version == "1.6" { 8 } else { 7 },
    )
    .await;
    let actions: Vec<_> = calls
        .iter()
        .map(|c| c.request["action"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        &actions[..3],
        ["BootNotification", "Heartbeat", "StatusNotification"]
    );
    assert_eq!(calls[0].request["charge_point_id"], "CP-PY");
    assert_eq!(calls[0].request["ocpp_version"], version);
    // CSMS-initiated call through the connection's peer handle.
    let conn = calls[0].connection_id.expect("connection id");
    let remote = if version == "1.6" {
        json!({"type":"ocpp_send_call","action":"RemoteStartTransaction","payload":{"idTag":"ABC123","connectorId":1}})
    } else {
        json!({"type":"ocpp_send_call","action":"RequestStartTransaction","payload":{"idToken":{"idToken":"ABC123","type":"ISO14443"},"remoteStartId":1}})
    };
    assert!(matches!(
        state
            .send_to_peer(sid, conn, remote, Duration::from_secs(10))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let reply = logs(&state, server, "ocpp_call_response", 1).await;
    assert_eq!(reply[0].request["payload"]["status"], "Accepted");
    let server_call: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(server_call["server_call"]
        .as_str()
        .unwrap()
        .contains("Start"));
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_ocpp_charge_point_runs_the_16_workflow_against_the_csms() {
    walk("1.6").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_ocpp_charging_station_runs_the_201_workflow_against_the_csms() {
    walk("2.0.1").await;
}
