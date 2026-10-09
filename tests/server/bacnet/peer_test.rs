use crate::helpers::ics::*;
use serde_json::json;
#[tokio::test(flavor = "multi_thread")]
async fn independent_bacpypes_discovery_read_write_errors() {
    let s = state();
    let policy = vec![
        json!({"event_pattern":"bacnet_request","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'bacnet_reply'}\nif e['instance']==999:a.update(error_class=1,error_code=31)\nelif e['operation']=='write':a['accepted']=True\nelse:a.update(value_type='real',value=12.5)\nprint(json.dumps({'actions':[a]}))"}}),
    ];
    let (id, a) = server_in(&s, "bacnet", policy, json!({})).await;
    assert_eq!(peer("bacnet", "client", a).await["error"], true);
    s.remove_server(id).await;
    rebind_udp(a).await;
}
#[tokio::test(flavor = "multi_thread")]
async fn segmented_unknown_and_malformed() {
    use netget::server::bacnet::codec::*;
    let s = state();
    let (id, a) = server_in(&s, "bacnet", quiet(), json!({})).await;
    let c = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    c.connect(a).await.unwrap();
    let mut b = [0; 480];
    c.send(&wrap(&[8, 5, 7, 0, 0, 12])).await.unwrap();
    let n = c.recv(&mut b).await.unwrap();
    assert_eq!(unwrap(&b[..n]).unwrap(), [0x71, 7, 4]);
    c.send(&wrap(&[0, 5, 8, 5])).await.unwrap();
    let n = c.recv(&mut b).await.unwrap();
    assert_eq!(unwrap(&b[..n]).unwrap(), [0x60, 8, 9]);
    c.send(&[0x81, 10, 255, 255, 1, 0, 0]).await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), c.recv(&mut b))
            .await
            .is_err()
    );
    s.remove_server(id).await;
    rebind_udp(a).await;
}
