use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_aioslsk_client_and_lifecycle() {
    let state = h::state();
    let (id,addr)=h::server_in(&state,"soulseek",vec![json!({"event_pattern":"soulseek_request","handler":{"type":"static","actions":[{"type":"soulseek_reply","accepted":true,"rooms":["NetGet"],"description":"NetGet","directories":[{"name":"share","files":[{"name":"hello.txt","size":5,"extension":"txt"}]}]}]}})],json!({})).await;
    let result = h::peer("soulseek", "client", addr).await;
    assert_eq!(result["ok"], true);
    // Oversized advertised length is rejected before body allocation.
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    let mut bad = tokio::net::TcpStream::connect(addr).await.unwrap();
    bad.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
    let mut b = [0];
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), bad.read(&mut b))
            .await
            .unwrap()
            .map_or(true, |n| n == 0)
    );
    let mut idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    state.remove_server(id).await;
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), idle.read(&mut b))
            .await
            .unwrap()
            .map_or(true, |n| n == 0)
    );
}
