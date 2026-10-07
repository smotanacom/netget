use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_gtk_gnutella_connects() {
    let s = h::state();
    let(id,addr)=h::server_in(&s,"gnutella",vec![json!({"event_pattern":"gnutella_request","handler":{"type":"static","actions":[{"type":"gnutella_reply","ip":"127.0.0.1","port":6346,"files":1,"kilobytes":1}]}})],json!({})).await;
    assert_eq!(h::peer("gnutella", "client", addr).await["ok"], true);
    let rows = h::logs(
        &s,
        netget::state::AccessLogOwner::Server(id.as_u32()),
        "gnutella_request",
        1,
    )
    .await;
    assert_eq!(rows[0].request["operation"], "ping");
    s.remove_server(id).await;
}
#[test]
fn descriptor_injection_rejected() {
    use netget::server::gnutella::codec::validate;
    assert!(validate(&json!({"type":"gnutella_query","query":"bad\0text"})).is_err());
    assert!(validate(
        &json!({"type":"gnutella_push","servent_id":"bad","ip":"127.0.0.1","port":6346})
    )
    .is_err());
}

#[tokio::test]
async fn malformed_negotiation_and_owner_shutdown() {
    h::malformed_and_owner_shutdown("gnutella", b"GNUTELLA CONNECT/0.4\r\n\r\n").await;
}

#[tokio::test]
async fn operator_disconnect_interrupts_a_pending_manual_answer() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let s = h::state();
    let (id, addr) = h::server_in(
        &s,
        "gnutella",
        vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":600}})],
        json!({}),
    )
    .await;
    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"GNUTELLA CONNECT/0.6\r\nX-Ultrapeer: False\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        response.push(peer.read_u8().await.unwrap());
    }
    assert!(response.starts_with(b"GNUTELLA/0.6 200 OK"));
    peer.write_all(b"GNUTELLA/0.6 200 OK\r\n\r\n")
        .await
        .unwrap();
    peer.write_all(&[
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0,
    ])
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while s.list_intercepts().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let conn = s
        .get_server(id)
        .await
        .unwrap()
        .connections
        .values()
        .next()
        .unwrap()
        .id
        .as_u32();
    s.send_to_peer(
        id,
        conn,
        json!({"type":"disconnect"}),
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), peer.read_u8())
            .await
            .unwrap()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    assert!(s.list_intercepts().await.is_empty());
    s.remove_server(id).await;
}
