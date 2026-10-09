use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_bacpypes_device() {
    let (mut p, a) = peer_server("bacnet").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "bacnet", quiet(), json!({}))
        .await
        .unwrap();
    assert_eq!(
        send(&s, id, json!({"type":"bacnet_discover"})).await["device_id"],
        5678
    );
    let r = send(
        &s,
        id,
        json!({"type":"bacnet_read","object_type":2,"instance":1,"property":85}),
    )
    .await;
    assert_eq!(r["value"], 12.5);
    assert_eq!(send(&s,id,json!({"type":"bacnet_write","object_type":2,"instance":1,"property":85,"value_type":"real","value":23.5})).await["success"],true);
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"bacnet_read","object_type":2,"instance":1,"property":85})
        )
        .await["value"],
        23.5
    );
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"bacnet_read","object_type":2,"instance":999,"property":85})
        )
        .await["success"],
        false
    );
    s.remove_client(id).await;
    drop(p.stdin.take());
    assert!(p.wait().await.unwrap().success());
}
