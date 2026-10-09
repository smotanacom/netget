//! NetGet's LwM2M device against the Eclipse Leshan 2.0.0-M15 server demo (Java/Californium,
//! independent, unchanged), driven through Leshan's own REST API: the registration as Leshan
//! sees it, reads in text and SenML JSON answered by the device policy, a write read back, an
//! execute, a 4.04, an observation whose notification Leshan reports, and deregistration.
//! Fails, never skips.
use crate::helpers::lwm2m::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn device_against_leshan_server() {
    let leshan = start_leshan_server().await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/clients", leshan.extra_ports[3]);
    let http = reqwest::Client::new();
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let cid = client_in(
        &state,
        leshan.addr(),
        json!({"endpoint": "netget-dev", "lifetime": 30, "objects": ["/1/0", "/3/0", "/3303/0"]}),
        device_policy(&dir.path().join("device.json")),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    wait_for(&state, owner, "lwm2m_registered", |_| true).await;

    let clients: Value = http.get(&api).send().await.unwrap().json().await.unwrap();
    let me = clients
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["endpoint"] == "netget-dev")
        .unwrap_or_else(|| panic!("{clients}"));
    assert!(me["objectLinks"].to_string().contains("/3303/0"), "{me}");

    let get = |path: &str, format: &str| {
        let url = format!("{api}/netget-dev{path}?timeout=10&format={format}");
        let http = http.clone();
        async move {
            http.get(url)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let manufacturer = get("/3/0/0", "TEXT").await;
    assert_eq!(
        (
            manufacturer["status"].as_str(),
            manufacturer["content"]["value"].as_str()
        ),
        (Some("CONTENT(205)"), Some("NetGet Device")),
        "{manufacturer}"
    );
    let temperature = get("/3303/0", "SENML_JSON").await;
    assert!(
        temperature["content"].to_string().contains("21.5"),
        "{temperature}"
    );
    let write: Value = http
        .put(format!("{api}/netget-dev/3/0/14?timeout=10&format=TEXT"))
        .json(&json!({"id": 14, "kind": "singleResource", "value": "+05", "type": "STRING"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(write["status"], "CHANGED(204)", "{write}");
    assert_eq!(get("/3/0/14", "TEXT").await["content"]["value"], "+05");
    let exec: Value = http
        .post(format!("{api}/netget-dev/3/0/4?timeout=10"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(exec["status"], "CHANGED(204)", "{exec}");
    assert_eq!(get("/3/0/9", "TEXT").await["status"], "NOT_FOUND(404)");
    let req = wait_for(&state, owner, "lwm2m_execute_request", |_| true).await;
    assert_eq!(req["path"], "/3/0/4");

    // Leshan observes the temperature; NetGet's device notifies and Leshan's event stream says so.
    let observe: Value = http
        .post(format!(
            "{api}/netget-dev/3303/0/5700/observe?timeout=10&format=SENML_JSON"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(observe["status"], "CONTENT(205)", "{observe}");
    let mut events = http
        .get(format!(
            "http://127.0.0.1:{}/api/event?ep=netget-dev",
            leshan.extra_ports[3]
        ))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    let r = state.send_to_client(cid, json!({"type": "lwm2m_notify", "path": "/3303/0/5700", "values": [{"path": "/3303/0/5700", "value": 23.25}]}), Duration::from_secs(10)).await.unwrap();
    assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{r:?}");
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        // Leshan streams its CoAP log of the incoming notification (which carries the value)
        // as one event and the NOTIFICATION as another; stopping at the first can miss the
        // second.
        while !(seen.contains("NOTIFICATION") && seen.contains("23.25")) {
            match events.chunk().await {
                Ok(Some(c)) => seen.push_str(&String::from_utf8_lossy(&c)),
                _ => break,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Leshan reported no notification: {seen}"));
    assert!(
        seen.contains("NOTIFICATION") && seen.contains("23.25"),
        "{seen}"
    );

    assert!(matches!(
        state
            .send_to_client(cid, json!({"type": "disconnect"}), Duration::from_secs(20))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let clients: Value = http.get(&api).send().await.unwrap().json().await.unwrap();
        if !clients.to_string().contains("netget-dev") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Leshan still lists the device: {clients}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    state.remove_client(cid).await;
}
