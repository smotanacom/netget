use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_ncdc_file_list_download() {
    let s = h::state();
    let(id,addr)=h::server_in(&s,"adc_peer",vec![json!({"event_pattern":"adc_peer_request","handler":{"type":"static","actions":[{"type":"adc_peer_reply","file_list_xml":"<?xml version=\"1.0\" encoding=\"utf-8\"?><FileListing Version=\"1\" Base=\"/\" Generator=\"NetGet\"><Directory Name=\"share\"><File Name=\"hello.txt\" Size=\"5\" TTH=\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"/></Directory></FileListing>"}]}})],json!({})).await;
    assert_eq!(h::peer("adc_peer", "client", addr).await["ok"], true);
    s.remove_server(id).await;
}

#[tokio::test]
async fn malformed_negotiation_and_owner_shutdown() {
    h::malformed_and_owner_shutdown("adc_peer", b"CSUP ADINVALID\n").await;
}
#[tokio::test]
async fn peer_transfer_ranges_and_refusals() {
    use netget::server::{
        adc_peer::codec::Scanner,
        p2p_support::{ScannerSession, Stream},
    };
    let s = h::state();
    let(id,addr)=h::server_in(&s,"adc_peer",vec![json!({"event_pattern":"adc_peer_request","handler":{"type":"static","actions":[{"type":"adc_peer_reply","data_base64":"SGVsbG8="}]}})],json!({})).await;
    let mut stream: Stream = Box::new(tokio::net::TcpStream::connect(addr).await.unwrap());
    let mut scanner = Scanner::default();
    scanner.open(&mut stream).await.unwrap();
    let result = scanner
        .exchange(
            &mut stream,
            &json!({"type":"adc_peer_get","identifier":"TTH/example","offset":1,"length":3}),
        )
        .await
        .unwrap();
    assert_eq!(result["data_base64"], "ZWxs");
    let refused = scanner
        .exchange(
            &mut stream,
            &json!({"type":"adc_peer_get","identifier":"missing","offset":100,"length":1}),
        )
        .await
        .unwrap();
    assert_eq!(refused["error"], "151");
    s.remove_server(id).await;
}
