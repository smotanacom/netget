//! pynetdicom 3.0.4 (independent, unchanged) as an SCU against NetGet's SCP, backed by the
//! script archive: association refused for a wrong called AE title (by Rust), for a calling AE
//! the service rejects, and transiently when the service gives no decision; then C-ECHO,
//! C-STOREs (one refused by the service), C-FINDs matched and projected by Rust, and A-RELEASE.
//! Fails, never skips.
use crate::helpers::dicom::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn pynetdicom_scu_against_netget_scp() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(&state, scp_policy(&dir.path().join("db.json")), json!({})).await;
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(peer_python())
            .arg(peer_script())
            .args(["scu", "127.0.0.1", &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("pynetdicom timed out")
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "pynetdicom failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut steps: HashMap<String, Vec<Value>> = HashMap::new();
    for line in stdout.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        steps
            .entry(v["step"].as_str().unwrap().to_owned())
            .or_default()
            .push(v);
    }
    let one = |s: &str| steps[s][0].clone();
    for s in ["wrong_called", "refused", "undecided"] {
        assert_eq!(one(s)["rejected"], true, "{s}: {}", one(s));
    }
    let assoc = one("associated");
    assert_eq!(assoc["established"], true);
    assert_eq!(assoc["accepted"].as_array().unwrap().len(), 4, "{assoc}");
    assert_eq!(
        assoc["transfer"],
        json!(["1.2.840.10008.1.2.1"]),
        "Explicit VR Little Endian preferred"
    );
    assert_eq!(one("echo")["status"], 0);
    let stores = &steps["store"];
    assert_eq!(
        (stores[0]["status"].as_u64(), stores[1]["status"].as_u64()),
        (Some(0), Some(0))
    );
    assert_eq!(stores[2]["status"], 0xA700);
    assert_eq!(stores[2]["comment"], "archive full");

    let wildcard = one("find_wildcard");
    assert_eq!(wildcard["final"], 0);
    let mut names: Vec<&str> = wildcard["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["PatientName"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["Doe^Jane", "Roe^Rich"]);
    assert!(wildcard["rows"][0]["StudyDate"]
        .as_str()
        .unwrap()
        .starts_with("2026"));
    let range = one("find_range");
    assert_eq!(
        range["rows"],
        json!([{"StudyDate": "20261015", "PatientName": "Roe^Rich", "PatientID": "P002"}])
    );
    let patient = one("find_patient");
    assert_eq!(
        patient["rows"],
        json!([{"PatientID": "P001", "PatientName": "Doe^Jane"}])
    );
    assert_eq!(one("find_none")["rows"], json!([]));
    assert_eq!(one("find_none")["final"], 0);
    assert_eq!(one("released")["released"], true);

    // The service saw the stored instance as DICOM JSON, pixel data by length only.
    let owner = AccessLogOwner::Server(sid.as_u32());
    let stored = logs(&state, owner, "dicom_store", 3).await;
    let ds = &stored[0].request["dataset"];
    assert_eq!(ds["00100010"]["Value"][0]["Alphabetic"], "Doe^Jane");
    assert_eq!(ds["7FE00010"], json!({"vr": "OB", "length": 4}));
    assert_eq!(
        stored[0].request["sop_class_uid"],
        "1.2.840.10008.5.1.4.1.1.7"
    );
    let finds = logs(&state, owner, "dicom_find", 4).await;
    assert_eq!(finds[2].request["model"], "patient_root");
    assert_eq!(finds[2].request["level"], "PATIENT");
    state.remove_server(sid).await;
}
