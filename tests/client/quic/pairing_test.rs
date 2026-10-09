//! Netget pairing complements, but does not replace, the independent aioquic peers.
use crate::helpers::quic_peer::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn netget_pair_exchanges_binary_then_disconnects_and_releases_server() {
    let state = state();
    let cert = Certificate::new();
    let(server_id,addr)=server(&state,"quic",&cert,json!({}),vec![json!({"event_pattern":"quic_data_received","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'send_quic_data','data':e['data'],'encoding':e['encoding']}]}))"}}),empty_handler()]).await;
    let id = client(
        &state,
        "quic",
        addr.to_string(),
        cert.trust(),
        vec![empty_handler()],
    )
    .await;
    let result = state
        .send_to_client(
            id,
            json!({"type":"send_quic_data","encoding":"hex","data":"00fffe01807fc328"}),
            Duration::from_secs(4),
        )
        .await
        .unwrap();
    assert!(matches!(result, ClientSendOutcome::Executed { .. }));
    wait_log(&state, id, "quic_data_received").await;
    let logs = state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await;
    assert!(logs
        .iter()
        .any(|entry| entry.event_type == "quic_data_received"
            && entry.request["data"] == "00fffe01807fc328"
            && entry.request["encoding"] == "hex"));
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let server = state.get_server(server_id).await.unwrap();
            if server
                .connections
                .values()
                .all(|c| c.status != netget::state::server::ConnectionStatus::Active)
                && !state.has_client_handle(id).await
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    state.remove_server(server_id).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::net::UdpSocket::bind(addr).is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn quic_payload_bounds_apply_before_decode_and_errors_remain_small() {
    use netget::llm::actions::{client_trait::Client, protocol_trait::Server};
    use netget::server::quic::actions::{
        decode_quic_payload, QuicProtocol, MAX_ENCODED_QUIC_BYTES,
    };
    let over = " ".repeat(MAX_ENCODED_QUIC_BYTES + 1);
    for encoding in ["utf8", "hex", "base64"] {
        let error = decode_quic_payload(&over, Some(encoding)).unwrap_err();
        assert!(error.to_string().contains("4 MiB"));
        assert!(error.to_string().len() < 100);
    }
    let edge = format!("{}00", " ".repeat(MAX_ENCODED_QUIC_BYTES - 2));
    assert_eq!(decode_quic_payload(&edge, Some("hex")).unwrap(), vec![0]);
    let invalid = "z".repeat(MAX_ENCODED_QUIC_BYTES);
    assert!(
        decode_quic_payload(&invalid, Some("hex"))
            .unwrap_err()
            .to_string()
            .len()
            < 400
    );
    let permitted = json!({"type":"send_quic_data","data":"x".repeat(1024*1024)});
    assert!(QuicProtocol.execute_action(permitted.clone()).is_ok());
    assert!(netget::client::quic::actions::QuicClientProtocol
        .execute_action(permitted)
        .is_ok());
    let decoded_over = json!({"type":"send_quic_data","data":"x".repeat(1024*1024+1)});
    assert!(QuicProtocol.execute_action(decoded_over.clone()).is_err());
    assert!(netget::client::quic::actions::QuicClientProtocol
        .execute_action(decoded_over)
        .is_err());
}
