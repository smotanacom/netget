use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_snap7_server_read_write_and_address_error() {
    let (mut peer, a) = peer_server("s7comm").await;
    let s = state();
    let id = client_in(&s, a.to_string(), "s7comm", quiet(), json!({}))
        .await
        .unwrap();
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"s7comm_read","area":"db","db":1,"start":0,"count":3})
        )
        .await["values"],
        json!([42, 43, 44])
    );
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"s7comm_write","area":"db","db":1,"start":0,"values":[5,6,7]})
        )
        .await["success"],
        true
    );
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"s7comm_read","area":"db","db":1,"start":0,"count":3})
        )
        .await["values"],
        json!([5, 6, 7])
    );
    assert_eq!(
        send(
            &s,
            id,
            json!({"type":"s7comm_read","area":"db","db":2,"start":0,"count":1})
        )
        .await["success"],
        false
    );
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
