use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_uhub_negotiation() {
    let (mut peer, addr) = h::peer_server("adc").await;
    let s = h::state();
    let id = h::client_in(&s, addr.to_string(), "adc", h::quiet(), json!({}))
        .await
        .unwrap();
    assert_eq!(
        h::send(&s, id, json!({"type":"adc_chat","message":"Hello"})).await["sent"],
        true
    );
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
