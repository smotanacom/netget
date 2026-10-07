use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_opendnp3_outstation_static_events_and_direct_operate() {
    let (mut peer, a) = peer_server("dnp3").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "dnp3", quiet(), json!({}))
        .await
        .unwrap();
    let result = send(&s, id, json!({"type":"dnp3_poll","classes":[0]})).await;
    assert_eq!(result["success"], true);
    let points = result["points"].as_array().unwrap();
    assert!(points
        .iter()
        .any(|p| p["kind"] == "binary" && p["value"] == true));
    assert!(points
        .iter()
        .any(|p| p["kind"] == "analog" && p["value"] == 12.5));
    assert!(points
        .iter()
        .any(|p| p["kind"] == "counter" && p["value"] == 42));
    let events = send(&s, id, json!({"type":"dnp3_poll","classes":[1,2,3]})).await;
    assert!(events["points"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["event"] == true && p["timestamp_ms"] == 123456));
    let control = send(
        &s,
        id,
        json!({"type":"dnp3_control","index":0,"code":3,"count":1,"on_ms":0,"off_ms":0}),
    )
    .await;
    assert_eq!(control["success"], true);
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
