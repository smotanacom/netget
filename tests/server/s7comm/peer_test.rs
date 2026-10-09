use crate::helpers::ics::*;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn policy() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"s7comm_request","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na=[({'error':'address'} if i['db']==2 else {'values':list(range(42,42+i['count']))} if e['operation']=='read' else {'accepted':True}) for i in e['items']]\nprint(json.dumps({'actions':[{'type':'s7comm_reply','items':a}]}))"}}),
    ]
}
#[tokio::test(flavor = "multi_thread")]
async fn independent_snap7_reads_writes_and_errors() {
    let s = state();
    let (id, a) = server_in(&s, "s7comm", policy(), json!({})).await;
    let r = peer("s7comm", "client", a).await;
    assert_eq!(r["areas"], 4);
    let e = logs(
        &s,
        netget::state::AccessLogOwner::Server(id.as_u32()),
        "s7comm_request",
        6,
    )
    .await;
    assert_eq!(e[1].request["items"][0]["values"], json!([10, 11, 12]));
    s.remove_server(id).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn malformed_frame_releases_connection_and_stop_releases_port() {
    let s = state();
    let (id, a) = server_in(&s, "s7comm", quiet(), json!({})).await;
    let mut c = tokio::net::TcpStream::connect(a).await.unwrap();
    c.write_all(&[3, 0, 0xff, 0xff]).await.unwrap();
    let mut b = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), c.read(&mut b))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut live = tokio::net::TcpStream::connect(a).await.unwrap();
    use netget::server::ics_support::ScannerSession;
    netget::server::s7comm::codec::Scanner::default()
        .open(&mut live)
        .await
        .unwrap();
    s.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), live.read(&mut b))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let listener = tokio::net::TcpListener::bind(a).await.unwrap();
    drop(listener);
}
#[test]
fn invalid_actions_do_not_narrow_or_allow_empty_writes() {
    use netget::server::s7comm::codec::validate;
    for a in [
        json!({"type":"s7comm_read","area":"db","db":65536,"start":0,"count":1}),
        json!({"type":"s7comm_write","area":"db","db":1,"start":0,"values":[]}),
        json!({"type":"s7comm_write","area":"db","db":1,"start":0,"values":[256]}),
    ] {
        assert!(validate(&a).is_err());
    }
}
