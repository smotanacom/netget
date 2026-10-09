use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_lib60870_controlled_station() {
    let (_p, a) = peer_server("iec104").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "iec104", quiet(), json!({}))
        .await
        .unwrap();
    let r = send(
        &s,
        id,
        json!({"type":"iec104_interrogate","common_address":1}),
    )
    .await;
    assert_eq!(r["success"], true);
    assert_eq!(r["points"].as_array().unwrap().len(), 2);
    let r = send(
        &s,
        id,
        json!({"type":"iec104_command","common_address":1,"ioa":1,"value":true}),
    )
    .await;
    assert_eq!(r["success"], true);
    s.remove_client(id).await;
}
