//! NetGet's HL7 sender against python-hl7 0.4.5's MLLP server — an independent receiver,
//! unchanged — which answers ADT with AA, ORU with AE plus ERR and anything else with AR. The
//! receiver's own log of what it parsed is asserted alongside what the sender heard back.
//! Fails, never skips, when the peer is absent.
use crate::helpers::hl7::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test(flavor = "multi_thread")]
async fn sender_gets_python_hl7_receivers_acknowledgments() {
    let mut child = tokio::process::Command::new(python())
        .arg(peer_script())
        .arg("server")
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
        quiet_sender(),
        json!({"sending_application":"NETGET","receiving_application":"PYHL7"}),
    )
    .await;
    for m in [
        adt(),
        json!({"type":"hl7_send","message_type":"ORU^R01^ORU_R01","segments":[{"id":"PID","fields":["1","","12345^^^HOSP^MR","","Doe^John"]},{"id":"OBX","fields":["1","NM","GLU^Glucose","","5.4","mmol/L"]}]}),
        json!({"type":"hl7_send","message_type":"SIU^S12","segments":[]}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, m, Duration::from_secs(10))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let acks = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "hl7_ack_received",
        3,
    )
    .await;
    let codes: Vec<_> = acks
        .iter()
        .map(|a| a.request["code"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(codes, ["AA", "AE", "AR"]);
    assert_eq!(
        (
            acks[0].request["control_id"].as_str(),
            acks[2].request["control_id"].as_str()
        ),
        (Some("NGC1"), Some("NGC3"))
    );
    assert!(acks[1].request["segments"]
        .to_string()
        .contains("glucose out of range"));
    let mut received = Vec::new();
    while received.len() < 3 {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        received.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    assert_eq!(received[0]["type"], "ADT^A01^ADT_A01");
    assert_eq!(received[0]["control_id"], "NGC1");
    assert_eq!(received[0]["sender"], "NETGET");
    assert_eq!(received[0]["patient"], "Doe^John");
    assert_eq!(received[1]["segments"], json!(["MSH", "PID", "OBX"]));
    state.remove_client(cid).await;
    drop(child.stdin.take());
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}
