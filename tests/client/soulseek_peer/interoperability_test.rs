use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_aioslsk_server() {
    let (mut peer, addr) = h::peer_server("soulseek_peer").await;
    let state = h::state();
    let id = h::client_in(
        &state,
        addr.to_string(),
        "soulseek_peer",
        h::quiet(),
        json!({}),
    )
    .await
    .unwrap();
    let list = h::send(&state, id, json!({"type":"soulseek_peer_shares"})).await;
    assert_eq!(list["directories"][0]["files"][0]["name"], "hello.txt");
    let info = h::send(&state, id, json!({"type":"soulseek_peer_info"})).await;
    assert_eq!(info["description"], "NetGet");
    state.remove_client(id).await;
    peer.kill().await.unwrap();
}
