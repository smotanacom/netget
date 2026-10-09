use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_cpppo_adapter_discovery_and_attributes() {
    let (mut peer, a) = peer_server("ethernet_ip").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "ethernet_ip", quiet(), json!({}))
        .await
        .unwrap();
    let identity = send(&s, id, json!({"type":"ethernet_ip_discover"})).await;
    assert_eq!(identity["vendor"], 1);
    assert_eq!(send(&s,id,json!({"type":"ethernet_ip_get","class":1,"instance":1,"attribute":1,"value_type":"uint16"})).await["value"],1);
    assert_eq!(send(&s,id,json!({"type":"ethernet_ip_set","class":1,"instance":1,"attribute":1,"value_type":"uint16","value":42})).await["success"],true);
    assert_eq!(send(&s,id,json!({"type":"ethernet_ip_get","class":1,"instance":1,"attribute":1,"value_type":"uint16"})).await["value"],42);
    assert_eq!(send(&s,id,json!({"type":"ethernet_ip_get","class":1,"instance":1,"attribute":255,"value_type":"uint16"})).await["success"],false);
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
