use crate::helpers::quic_peer::*;
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn independent_aioquic_binary_multiplexing_and_server_cleanup() {
    let state = state();
    let cert = Certificate::new();
    let(id,addr)=server(&state,"quic",&cert,json!({}),vec![json!({"event_pattern":"quic_data_received","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'send_quic_data','data':e['data'],'encoding':e['encoding']}]}))"}}),empty_handler()]).await;
    let result = external_client(
        "quic",
        addr.port(),
        &cert,
        json!([{"hex":"00fffe01807fc328"},{"body":"concurrent"}]),
    )
    .await;
    assert_eq!(result[0]["hex"], "00fffe01807fc328");
    assert_eq!(result[1]["hex"], hex::encode("concurrent"));
    assert_ne!(result[0]["stream_id"], result[1]["stream_id"]);
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::net::UdpSocket::bind(addr).is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn raw_stream_credit_cancellation_and_active_server_removal() {
    use netget::llm::actions::protocol_trait::Protocol;
    use netget::protocol::StartupParams;
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(&state, "quic", &cert, json!({}), vec![empty_handler()]).await;
    let params = StartupParams::new(
        cert.trust(),
        netget::client::quic::actions::QuicClientProtocol.get_startup_parameters(),
    )
    .unwrap();
    assert!(
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 0)
            .await
            .is_err()
    );
    let (_endpoint, c) =
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"netget-quic", 0)
            .await
            .unwrap();
    let mut streams = Vec::new();
    for _ in 0..32 {
        let (mut send, recv) = c.0.open_bi().await.unwrap();
        send.write_all(b"pending").await.unwrap();
        streams.push((send, recv));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(150), c.0.open_bi())
            .await
            .is_err()
    );
    // Quinn batches MAX_STREAMS updates (more than one eighth of credit).
    for _ in 0..8 {
        let (mut send, mut recv) = streams.pop().unwrap();
        send.reset(3u32.into()).unwrap();
        recv.stop(3u32.into()).unwrap();
    }
    let (mut replacement, _response) = tokio::time::timeout(Duration::from_secs(2), c.0.open_bi())
        .await
        .unwrap()
        .unwrap();
    replacement.write_all(b"replacement").await.unwrap();
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(3), c.0.closed())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::net::UdpSocket::bind(addr).is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn close_this_stream_on_open_finishes_without_waiting_for_client_fin() {
    use netget::llm::actions::protocol_trait::Protocol;
    let state = state();
    let cert = Certificate::new();
    let(id,addr)=server(&state,"quic",&cert,json!({}),vec![json!({"event_pattern":"quic_stream_opened","handler":{"type":"static","actions":[{"type":"close_this_stream"}]}}),empty_handler()]).await;
    let params = netget::protocol::StartupParams::new(
        cert.trust(),
        netget::client::quic::actions::QuicClientProtocol.get_startup_parameters(),
    )
    .unwrap();
    let (_endpoint, c) =
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"netget-quic", 0)
            .await
            .unwrap();
    let (mut send, mut recv) = c.0.open_bi().await.unwrap();
    send.write_all(b"still-open").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), recv.read_to_end(1024))
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
    state.remove_server(id).await;
}
