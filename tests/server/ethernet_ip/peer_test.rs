use crate::helpers::ics::*;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn policy() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"ethernet_ip_request","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'ethernet_ip_reply'}\nif e['attribute']==9:a['status']=20\nelif e['operation']=='get':a.update(value_type='uint16',value=42)\nelse:a['accepted']=e['value']==42\nprint(json.dumps({'actions':[a]}))"}}),
    ]
}
#[tokio::test(flavor = "multi_thread")]
async fn independent_cpppo_get_set_and_errors() {
    let s = state();
    let (id, a) = server_in(&s, "ethernet_ip", policy(), json!({})).await;
    assert_eq!(peer("ethernet_ip", "client", a).await["read"], true);
    let entries = logs(
        &s,
        netget::state::AccessLogOwner::Server(id.as_u32()),
        "ethernet_ip_request",
        3,
    )
    .await;
    assert_eq!(entries[1].request["value"], 42);
    s.remove_server(id).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn udp_discovery_invalid_session_and_stop() {
    use netget::server::ethernet_ip::codec::*;
    let s = state();
    let (id, a) = server_in(&s, "ethernet_ip", quiet(), json!({})).await;
    let u = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let query = encapsulate(0x63, 0, [1; 8], 0, &[]);
    u.send_to(&query, a).await.unwrap();
    let mut buf = [0; 512];
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), u.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(n > 40);
    assert_eq!(buf[12..20], [1; 8]);
    let mut c = tokio::net::TcpStream::connect(a).await.unwrap();
    c.write_all(&encapsulate(0x6f, 123, [2; 8], 0, &[]))
        .await
        .unwrap();
    let f = read(&mut c).await.unwrap();
    assert_eq!(&f[8..12], &0x64u32.to_le_bytes());
    s.remove_server(id).await;
    let mut one = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), c.read(&mut one))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(tokio::net::TcpListener::bind(a).await.is_ok());
    assert!(tokio::net::UdpSocket::bind(a).await.is_ok());
}
#[test]
fn schema_values_and_paths_are_checked() {
    use netget::server::ethernet_ip::codec::validate;
    assert!(validate(&json!({"type":"ethernet_ip_get","class":65536,"instance":1,"attribute":1,"value_type":"uint16"})).is_err());
    assert!(validate(&json!({"type":"ethernet_ip_set","class":1,"instance":1,"attribute":1,"value_type":"uint16","value":65536})).is_err());
}
