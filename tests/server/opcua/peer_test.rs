use crate::helpers::ics::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[tokio::test(flavor = "multi_thread")]
async fn independent_asyncua_browse_read_write_method_subscribe() {
    let s = state();
    let policy = vec![
        json!({"event_pattern":"opcua_request","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\na={'type':'opcua_reply'}\nif e['operation']=='read':a.update(value_type='double',value=12.5)\nelif e['operation']=='write':a['accepted']=True\nelse:a['outputs']=[{'value_type':'double','value':e['arguments'][0]['value']*2}]\nprint(json.dumps({'actions':[a]}))"}}),
    ];
    let (id, a) = server_in(&s, "opcua", policy, json!({})).await;
    assert_eq!(peer("opcua", "client", a).await["subscription"], true);
    s.remove_server(id).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if tokio::net::TcpListener::bind(a).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn malformed_hello_and_live_stop() {
    let s = state();
    let (id, a) = server_in(&s, "opcua", quiet(), json!({})).await;
    let mut bad = tokio::net::TcpStream::connect(a).await.unwrap();
    bad.write_all(b"HELF\x01\x00\x04\x00").await.unwrap();
    let mut b = [0; 1024];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), bad.read(&mut b))
        .await
        .unwrap();
    assert!(n.is_err() || n.unwrap() == 0 || b.starts_with(b"ERR"));
    let mut live = tokio::net::TcpStream::connect(a).await.unwrap();
    let endpoint = format!("opc.tcp://{a}/");
    let mut hello = b"HELF".to_vec();
    hello.extend((32 + endpoint.len() as u32).to_le_bytes());
    for v in [0u32, 65535, 65535, 262144, 16] {
        hello.extend(v.to_le_bytes());
    }
    hello.extend((endpoint.len() as u32).to_le_bytes());
    hello.extend(endpoint.as_bytes());
    live.write_all(&hello).await.unwrap();
    let mut ack = [0; 28];
    live.read_exact(&mut ack).await.unwrap();
    assert_eq!(&ack[..4], b"ACKF");
    s.remove_server(id).await;
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), live.read(&mut b))
        .await
        .unwrap();
    assert!(n.is_err() || n.unwrap() == 0);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if tokio::net::TcpListener::bind(a).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[test]
fn codec_rejects_header_bounds_before_waiting_for_body() {
    use ::opcua::{core::comms::tcp_codec::TcpCodec, types::encoding::DecodingOptions};
    use tokio_util::codec::Decoder;
    let mut options = DecodingOptions::default();
    options.max_message_size = 262144;
    for size in [1u32, 7, 262145, u32::MAX] {
        let mut header = b"HELF".to_vec();
        header.extend(size.to_le_bytes());
        let mut codec = TcpCodec::new(options.clone());
        assert!(codec
            .decode(&mut bytes::BytesMut::from(header.as_slice()))
            .is_err());
    }
    let mut codec = TcpCodec::new(options);
    let mut valid = b"HELF".to_vec();
    valid.extend(262144u32.to_le_bytes());
    assert!(codec
        .decode(&mut bytes::BytesMut::from(valid.as_slice()))
        .unwrap()
        .is_none());
}
#[tokio::test(flavor = "multi_thread")]
async fn connection_cap_and_all_live_sockets_close() {
    let s = state();
    let (id, a) = server_in(&s, "opcua", quiet(), json!({})).await;
    let endpoint = format!("opc.tcp://{a}/");
    let mut hello = b"HELF".to_vec();
    hello.extend((32 + endpoint.len() as u32).to_le_bytes());
    for v in [0u32, 65535, 65535, 262144, 16] {
        hello.extend(v.to_le_bytes());
    }
    hello.extend((endpoint.len() as u32).to_le_bytes());
    hello.extend(endpoint.as_bytes());
    let mut peers = vec![];
    for _ in 0..256 {
        let mut c = tokio::net::TcpStream::connect(a).await.unwrap();
        c.write_all(&hello).await.unwrap();
        let mut ack = [0; 28];
        tokio::time::timeout(std::time::Duration::from_secs(5), c.read_exact(&mut ack))
            .await
            .unwrap()
            .unwrap();
        peers.push(c);
    }
    let mut extra = tokio::net::TcpStream::connect(a).await.unwrap();
    extra.write_all(&hello).await.unwrap();
    let mut b = [0];
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), extra.read(&mut b))
            .await
            .is_err()
    );
    let connections = s.get_server(id).await.unwrap().connections;
    let first_addr = peers[0].local_addr().unwrap();
    let first = connections
        .values()
        .find(|c| c.remote_addr == first_addr)
        .unwrap()
        .id;
    assert!(s.has_peer_handle(id, first.as_u32()).await);
    let result = s
        .send_to_peer(
            id,
            first.as_u32(),
            json!({"type":"disconnect"}),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        netget::state::client_handles::ClientSendOutcome::Disconnected
    ));
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), peers[0].read(&mut b))
        .await
        .unwrap();
    assert!(n.is_err() || n.unwrap() == 0);
    s.remove_server(id).await;
    for mut c in peers {
        let n = tokio::time::timeout(std::time::Duration::from_secs(2), c.read(&mut b))
            .await
            .unwrap();
        assert!(n.is_err() || n.unwrap() == 0);
    }
}
