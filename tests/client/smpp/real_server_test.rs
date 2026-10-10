//! NetGet's SMPP client against Melrose Labs' SMSC simulator, the C++ SMSC gosmpp v0.3.1 ships
//! as example/smsc_simulator, compiled unchanged by `install_peers.py` (NETGET_SMPP_SMSC_SIM).
//! It answers a submit with a random 64-character id, echoes the text straight back as a
//! mobile-originated message, and sends the delivery receipt 3–12 s later. It listens on the
//! fixed port 2775, so this is the only test that starts it. Fails rather than skips without it.
use super::session_test::{client, send, wait_log};
use serde_json::json;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn netget_against_melrose_labs_smsc() {
    let sim = std::env::var_os("NETGET_SMPP_SMSC_SIM").map(PathBuf::from).expect("NETGET_SMPP_SMSC_SIM is required: run tests/client/smpp/install_peers.py <root> and export what it prints");
    let mut child = tokio::process::Command::new(sim)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start the SMSC simulator");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.contains("Listening for SMPP on port 2775") {
                return;
            }
        }
        panic!("the simulator exited before listening");
    })
    .await
    .expect("the simulator did not start (is port 2775 free?)");
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let (state, id) = client(
        "127.0.0.1:2775".into(),
        json!({"system_id":"netget","password":"pw"}),
        json!([{"type":"smpp_submit","source_addr":"NetGet","destination_addr":"447700900123","text":"hi from netget","registered_delivery":true}]),
    )
    .await
    .unwrap();
    wait_log(&state, id, r#""smsc_system_id":"MelroseLabsSMSC""#, 10).await;
    let result = wait_log(&state, id, r#""status":"ESME_ROK""#, 20).await;
    let message_id = result
        .split(r#""message_id":""#)
        .nth(1)
        .and_then(|r| r.split('"').next())
        .expect("a message id")
        .to_string();
    assert_eq!(message_id.len(), 64, "{result}");
    let mo = wait_log(&state, id, r#""source_addr":"FakeFrom""#, 20).await;
    assert!(
        mo.contains(r#""text":"hi from netget""#) && mo.contains(r#""is_receipt":false"#),
        "{mo}"
    );
    let receipt = wait_log(&state, id, r#""is_receipt":true"#, 30).await;
    assert!(
        receipt.contains(&format!(r#""id":"{message_id}""#)),
        "the receipt names the submit: {receipt}"
    );
    let link = send(&state, id, json!({"type":"smpp_enquire_link"})).await;
    assert!(format!("{link:?}").contains("ESME_ROK"), "{link:?}");
    state.remove_client(id).await;
    let refused = client(
        "127.0.0.1:2775".into(),
        json!({"system_id":"invalid","password":"pw"}),
        json!([]),
    )
    .await;
    assert!(refused.is_err(), "the simulator refuses system_id invalid");
    child.kill().await.unwrap();
}
