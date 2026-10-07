use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_ncdc_file_list_download() {
    let s = h::state();
    let(id,addr)=h::server_in(&s,"dc_peer",vec![json!({"event_pattern":"dc_peer_request","handler":{"type":"static","actions":[{"type":"dc_peer_reply","file_list_xml":"<?xml version=\"1.0\" encoding=\"utf-8\"?><FileListing Version=\"1\" Base=\"/\" Generator=\"NetGet\"><Directory Name=\"share\"><File Name=\"hello.txt\" Size=\"5\" TTH=\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"/></Directory></FileListing>"}]}})],json!({})).await;
    assert_eq!(h::peer("dc_peer", "client", addr).await["ok"], true);
    s.remove_server(id).await;
}

#[tokio::test]
async fn malformed_negotiation_and_owner_shutdown() {
    h::malformed_and_owner_shutdown("dc_peer", b"$Key invalid|").await;
}
#[tokio::test]
async fn peer_transfer_ranges_and_hash_rejection() {
    use netget::server::{
        dc_peer::codec::Scanner,
        p2p_support::{ScannerSession, Stream},
    };
    let s = h::state();
    let(id,addr)=h::server_in(&s,"dc_peer",vec![json!({"event_pattern":"dc_peer_request","handler":{"type":"static","actions":[{"type":"dc_peer_reply","data_base64":"SGVsbG8="}]}})],json!({})).await;
    let mut stream: Stream = Box::new(tokio::net::TcpStream::connect(addr).await.unwrap());
    let mut scanner = Scanner::default();
    scanner.open(&mut stream).await.unwrap();
    let result = scanner
        .exchange(
            &mut stream,
            &json!({"type":"dc_peer_get","identifier":"TTH/example","offset":1,"length":3}),
        )
        .await
        .unwrap();
    assert_eq!(result["data_base64"], "ZWxs");
    assert!(scanner.exchange(&mut stream,&json!({"type":"dc_peer_get","identifier":"TTH/example","offset":0,"length":-1,"expected_tth":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"})).await.is_err());
    s.remove_server(id).await;
}

#[tokio::test]
async fn configured_hub_identity_and_nickname_injection() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let s = h::state();
    let (id, addr) =
        h::server_in(&s, "dc_peer", h::quiet(), json!({"nickname":"HubIdentity"})).await;
    let mut peer = BufReader::new(tokio::net::TcpStream::connect(addr).await.unwrap());
    let mut first = Vec::new();
    peer.read_until(b'|', &mut first).await.unwrap();
    assert_eq!(first, b"$MyNick HubIdentity|");
    s.remove_server(id).await;
    let error = h::client_in(
        &s,
        "127.0.0.1:1".into(),
        "dc_peer",
        h::quiet(),
        json!({"nickname":"bad|$MyNick injected"}),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("invalid NMDC nickname"),
        "{error}"
    );
}
