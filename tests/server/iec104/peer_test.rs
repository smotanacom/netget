use crate::helpers::ics::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test(flavor = "multi_thread")]
async fn independent_lib60870_controlling_station() {
    let s = state();
    let policy = vec![
        json!({"event_pattern":"iec104_request","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'iec104_command_result','accepted':True} if e['operation']=='command' else {'type':'iec104_measurements','points':[{'kind':'binary','ioa':1,'value':True},{'kind':'analog','ioa':2,'value':12.5}]}\nprint(json.dumps({'actions':[a]}))"}}),
    ];
    let (id, a) = server_in(&s, "iec104", policy, json!({})).await;
    assert_eq!(peer("iec104", "client", a).await["command"], true);
    s.remove_server(id).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn invalid_sequence_and_lifecycle() {
    use netget::server::iec104::codec::u_frame;
    let s = state();
    let (id, a) = server_in(&s, "iec104", quiet(), json!({})).await;
    let mut c = tokio::net::TcpStream::connect(a).await.unwrap();
    c.write_all(&u_frame(7)).await.unwrap();
    let mut b = [0; 6];
    c.read_exact(&mut b).await.unwrap();
    assert_eq!(b, u_frame(11).as_slice());
    c.write_all(&[0x68, 13, 2, 0, 0, 0, 100, 1, 6, 0, 1, 0, 0, 0, 0])
        .await
        .unwrap();
    assert_eq!(c.read(&mut b).await.unwrap(), 0);
    let mut live = tokio::net::TcpStream::connect(a).await.unwrap();
    live.write_all(&u_frame(7)).await.unwrap();
    live.read_exact(&mut b).await.unwrap();
    s.remove_server(id).await;
    assert_eq!(live.read(&mut b).await.unwrap(), 0);
    assert!(tokio::net::TcpListener::bind(a).await.is_ok());
}
#[test]
fn command_limits() {
    assert!(netget::server::iec104::codec::validate(
        &json!({"type":"iec104_command","common_address":1,"ioa":16777216,"value":true})
    )
    .is_err());
}
#[test]
fn send_window_and_invalid_acknowledgment() {
    use netget::server::{
        ics_support::DeviceSession,
        iec104::codec::{u_frame, Device},
    };
    let mut d = Device::default();
    d.receive(&u_frame(7)).unwrap();
    let first = [0x68, 14, 0, 0, 0, 0, 100, 1, 6, 0, 1, 0, 0, 0, 0, 20];
    let (_, request) = d.receive(&first).unwrap();
    let points = (0..8)
        .map(|ioa| json!({"kind":"binary","ioa":ioa,"value":true}))
        .collect::<Vec<_>>();
    let reply = json!({"type":"iec104_measurements","points":points});
    d.answer(&request.unwrap(), Some(&reply)).unwrap();
    assert!(d.receive(&[0x68, 4, 1, 0, 26, 0]).is_err()); // acknowledges thirteen with only ten pending
    let mut next = first;
    next[2] = 2;
    let (_, request) = d.receive(&next).unwrap();
    assert!(d.answer(&request.unwrap(), Some(&reply)).is_err()); // k=12, no ack available
}
#[tokio::test(flavor = "multi_thread")]
async fn t1_closes_unacknowledged_telemetry() {
    use netget::server::{
        ics_support::DeviceSession,
        iec104::codec::{u_frame, Device},
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut d = Device::default();
    d.receive(&u_frame(7)).unwrap();
    let (_, r) = d
        .receive(&[0x68, 14, 0, 0, 0, 0, 100, 1, 6, 0, 1, 0, 0, 0, 0, 20])
        .unwrap();
    d.answer(
        &r.unwrap(),
        Some(&json!({"type":"iec104_measurements","points":[]})),
    )
    .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(17), d.read(&mut stream))
            .await
            .unwrap()
            .is_err()
    );
}
