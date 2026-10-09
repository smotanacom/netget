use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_gtk_gnutella_peer() {
    let (mut peer, addr) = h::peer_server("gnutella").await;
    let s = h::state();
    let result = h::client_in(&s, addr.to_string(), "gnutella", h::quiet(), json!({})).await;
    if let Err(e) = result {
        drop(peer.stdin.take());
        let _ = peer.wait().await;
        panic!("GTK handshake: {e}");
    }
    let id = result.unwrap();
    let sent = h::send(&s, id, json!({"type":"gnutella_ping"})).await;
    assert_eq!(sent["sent"], true);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows = s
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                    None,
                )
                .await;
            if rows.iter().any(|r| {
                r.event_type == "gnutella_response"
                    && r.request["operation"] == "pong"
                    && r.request["guid"] == sent["guid"]
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("independent peer must answer our ping with the same GUID");
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
