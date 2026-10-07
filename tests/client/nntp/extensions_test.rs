use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_nntpserver_verified_tls_auth_and_post() {
    let (mut peer, addr, meta) = h::peer_server_details("nntp").await;
    let s = h::state();
    let id = h::client_in(
        &s,
        addr.to_string(),
        "nntp",
        h::quiet(),
        json!({"use_tls":true,"ca_path":meta["ca"],"server_name":"localhost"}),
    )
    .await
    .unwrap();
    let auth = h::send(
        &s,
        id,
        json!({"type":"nntp_authenticate","username":"reader","password":"secret"}),
    )
    .await;
    assert_eq!(auth["code"], 281);
    let post=h::send(&s,id,json!({"type":"nntp_post","headers":{"From":"reader@localhost","Newsgroups":"misc.test","Subject":"Test","Message-ID":"<test@localhost>"},"body":".first\n\n"})).await;
    assert_eq!(post["code"], 240);
    let outcome = s
        .send_to_client(
            id,
            json!({"type":"nntp_quit"}),
            std::time::Duration::from_secs(12),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        netget::state::client_handles::ClientSendOutcome::Disconnected
    ));
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
#[tokio::test]
async fn untrusted_tls_server_is_rejected() {
    let (mut peer, addr, _) = h::peer_server_details("nntp").await;
    let s = h::state();
    assert!(h::client_in(
        &s,
        addr.to_string(),
        "nntp",
        h::quiet(),
        json!({"use_tls":true,"server_name":"localhost"})
    )
    .await
    .is_err());
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
}
