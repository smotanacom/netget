use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_aioslsk_server() {
    let (mut peer, addr) = h::peer_server("soulseek").await;
    let state = h::state();
    let id = h::client_in(&state, addr.to_string(), "soulseek", h::quiet(), json!({}))
        .await
        .unwrap();
    let login = h::send(
        &state,
        id,
        json!({"type":"soulseek_login","username":"netget","password":"test"}),
    )
    .await;
    assert_eq!(login["success"], true);
    let rooms = h::send(&state, id, json!({"type":"soulseek_rooms"})).await;
    assert_eq!(rooms["rooms"], json!(["NetGet"]));
    let chat = h::send(
        &state,
        id,
        json!({"type":"soulseek_chat","room":"NetGet","message":"Hi"}),
    )
    .await;
    assert_eq!(chat["message"], "Hi");
    state.remove_client(id).await;
    peer.kill().await.unwrap();
}
