use crate::helpers::ics::*;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn policy() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"dnp3_request","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\nif e['operation']=='control':a={'type':'dnp3_control_result','status':'success'}\nelse:a={'type':'dnp3_measurements','points':[{'kind':'binary','index':0,'class':0,'value':True},{'kind':'analog','index':0,'class':0,'value':12.5},{'kind':'counter','index':0,'class':0,'value':42},{'kind':'binary','index':0,'class':1,'value':False,'timestamp_ms':123456}]}\nprint(json.dumps({'actions':[a]}))"}}),
    ]
}
#[tokio::test(flavor = "multi_thread")]
async fn independent_opendnp3_master_typed_points_events_and_control() {
    let s = state();
    let (id, a) = server_in(&s, "dnp3", policy(), json!({})).await;
    assert_eq!(peer("dnp3", "client", a).await["event_time"], 123456);
    let entries = logs(
        &s,
        netget::state::AccessLogOwner::Server(id.as_u32()),
        "dnp3_request",
        2,
    )
    .await;
    assert_eq!(entries[1].request["operation"], "control");
    s.remove_server(id).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn crc_error_and_stop_release_links() {
    use netget::server::{dnp3::codec::*, ics_support::ScannerSession};
    let s = state();
    let (id, a) = server_in(&s, "dnp3", quiet(), json!({})).await;
    let mut c = tokio::net::TcpStream::connect(a).await.unwrap();
    let mut corrupt = link(0xc9, 10, 1, &[]);
    corrupt[8] ^= 1;
    c.write_all(&corrupt).await.unwrap();
    let mut buf = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut live = tokio::net::TcpStream::connect(a).await.unwrap();
    Scanner::default().open(&mut live).await.unwrap();
    s.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), live.read(&mut buf))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(tokio::net::TcpListener::bind(a).await.is_ok());
}
#[test]
fn controls_reject_overflow_and_empty_class_polls() {
    use netget::server::dnp3::codec::validate;
    assert!(validate(&json!({"type":"dnp3_poll","classes":[]})).is_err());
    assert!(validate(
        &json!({"type":"dnp3_control","index":65536,"code":3,"count":1,"on_ms":0,"off_ms":0})
    )
    .is_err());
    assert!(validate(
        &json!({"type":"dnp3_control","index":0,"code":3,"count":0,"on_ms":0,"off_ms":0})
    )
    .is_err());
}
