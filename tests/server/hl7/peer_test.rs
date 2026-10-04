//! python-hl7 0.4.5's MLLP client — an independent implementation, unchanged — sends ADT,
//! ORU and QRY messages to NetGet's endpoint and checks every acknowledgment: code, the
//! echoed control id, the swapped receiver, ERR detail and response segments. Fails, never
//! skips, when the peer is absent.
use crate::helpers::hl7::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn python_hl7_client_gets_the_acknowledgment_the_handler_chose() {
    let state = state();
    let (sid, addr) = server_in(&state, endpoint_policy(), json!({})).await;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(python())
            .arg(peer_script())
            .args(["client", "127.0.0.1", &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("python-hl7 client timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let acks: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(acks.len(), 3);
    for (ack, sent) in acks.iter().zip(["PEER-1", "PEER-2", "PEER-3"]) {
        assert_eq!(
            ack["msa_control"], sent,
            "MSA-2 echoes the original control id"
        );
        assert_eq!(
            ack["ack_receiver"], "LAB",
            "the ACK is addressed back to the sender"
        );
    }
    assert_eq!(
        (
            acks[0]["ack_code"].as_str(),
            acks[0]["ack_type"].as_str(),
            acks[0]["msa_text"].as_str()
        ),
        (Some("AA"), Some("ACK^A01^ACK"), Some("admitted"))
    );
    assert_eq!(acks[1]["ack_code"], "AE");
    assert!(acks[1]["err"][0]
        .as_str()
        .unwrap()
        .contains("207^Application internal error^HL70357"));
    assert_eq!(acks[2]["ack_code"], "AA");
    assert_eq!(acks[2]["extra"], json!(["QRD", "PID"]));
    let rows = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "hl7_message",
        3,
    )
    .await;
    assert_eq!(rows[0].request["message_type"], "ADT^A01^ADT_A01");
    assert_eq!(rows[0].request["control_id"], "PEER-1");
    let pid = rows[0].request["segments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "PID")
        .unwrap();
    assert_eq!(
        pid["fields"][4], "Doe^John",
        "fields keep their component structure"
    );
    assert_eq!(rows[2].request["version"], "2.3");
    state.remove_server(sid).await;
}
