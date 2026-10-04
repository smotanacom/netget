//! NetGet's DICOM client against pynetdicom 3.0.4 as an SCP (independent, unchanged): an
//! association with a wrong called AE title is refused, then C-ECHO, C-STORE (the SCP prints the
//! instance it decoded, pixel bytes included), C-FIND (pending matches gathered into one
//! response) and A-RELEASE. Fails, never skips.
use crate::helpers::dicom::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_pynetdicom_scp() {
    let scp = start_scp().await.unwrap();
    let state = state();
    let wrong = client_in(&state, scp.addr(), json!({"called_ae": "WRONG"})).await;
    assert!(wrong
        .unwrap_err()
        .to_string()
        .contains("association rejected"));

    let cid = client_in(&state, scp.addr(), json!({"called_ae": "PACS", "calling_ae": "MODALITY", "storage_classes": ["1.2.840.10008.5.1.4.1.1.7"]}))
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let associated = logs(&state, owner, "dicom_associated", 1).await;
    assert_eq!(
        associated[0].request["accepted"].as_array().unwrap().len(),
        4,
        "{}",
        associated[0].request
    );
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(30));
    for a in [
        json!({"type": "dicom_echo"}),
        json!({"type": "dicom_store", "sop_class_uid": "1.2.840.10008.5.1.4.1.1.7", "sop_instance_uid": "1.2.826.0.1.3680043.10.1408.400.1", "dataset": {
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
            "00100020": {"vr": "LO", "Value": ["P001"]},
            "0020000D": {"vr": "UI", "Value": ["1.2.826.0.1.3680043.10.1408.401"]},
            "00080060": {"vr": "CS", "Value": ["OT"]},
            "7FE00010": {"vr": "OB", "hex": "0a0b0c0d"}
        }}),
        json!({"type": "dicom_store", "sop_class_uid": "1.2.840.10008.5.1.4.1.1.7", "sop_instance_uid": "1.2.826.0.1.3680043.10.1408.400.2", "dataset": {
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Full^Disk"}]},
            "00100020": {"vr": "LO", "Value": ["FULL"]}
        }}),
        json!({"type": "dicom_find", "level": "STUDY", "identifier": {"00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe*"}]}, "0020000D": {"vr": "UI"}}}),
        json!({"type": "dicom_find", "model": "patient_root", "level": "PATIENT", "identifier": {"00100010": {"vr": "PN", "Value": [{"Alphabetic": "Nobody"}]}}}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let r: Vec<Value> = logs(&state, owner, "dicom_response", 5)
        .await
        .into_iter()
        .map(|r| r.request)
        .collect();
    assert_eq!(
        (r[0]["operation"].as_str(), r[0]["status"].as_str()),
        (Some("echo"), Some("0000"))
    );
    assert_eq!(
        (r[1]["status"].as_str(), r[1]["meaning"].as_str()),
        (Some("0000"), Some("success"))
    );
    assert_eq!(
        (r[2]["status"].as_str(), r[2]["meaning"].as_str()),
        (Some("A700"), Some("failure"))
    );
    let m = r[3]["matches"].as_array().unwrap();
    assert_eq!(m.len(), 1, "{}", r[3]);
    assert_eq!(m[0]["00100010"]["Value"][0]["Alphabetic"], "Doe^Jane");
    assert_eq!(
        m[0]["0020000D"]["Value"][0],
        "1.2.826.0.1.3680043.10.1408.401"
    );
    assert_eq!(
        (
            r[4]["status"].as_str(),
            r[4]["matches"].as_array().map(Vec::len)
        ),
        (Some("0000"), Some(0))
    );

    // What the SCP decoded, from its own output.
    scp.wait_for_log("\"pid\": \"FULL\"", Duration::from_secs(10))
        .await
        .unwrap();
    let log = scp.log();
    let stored: Value = log
        .lines()
        .find(|l| l.contains("\"pid\": \"P001\""))
        .map(|l| serde_json::from_str(l).unwrap())
        .unwrap();
    assert_eq!(stored["patient"], "Doe^Jane");
    assert_eq!(stored["pixels"], "0a0b0c0d");
    assert_eq!(stored["calling"], "MODALITY");
    assert_eq!(stored["stored"], "1.2.826.0.1.3680043.10.1408.400.1");

    assert!(matches!(
        send(json!({"type": "disconnect"})).await.unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(cid).await;
}
