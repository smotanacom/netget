use crate::helpers::quic_peer::*;
use netget::{
    client::quic::{actions::QuicClientProtocol, exchange},
    llm::actions::protocol_trait::Protocol,
    protocol::StartupParams,
    state::client_handles::ClientSendOutcome,
};
use serde_json::json;
use std::time::Duration;
fn params(value: serde_json::Value) -> StartupParams {
    StartupParams::new(value, QuicClientProtocol.get_startup_parameters()).unwrap()
}
#[tokio::test]
async fn aioquic_binary_multiplexing_and_owned_local_address() {
    let mut peer = Peer::start("quic").await;
    let (endpoint, connection) = netget::utils::quic::connect(
        &peer.address(),
        Some(&params(peer.cert.trust())),
        b"netget-quic",
        0,
    )
    .await
    .unwrap();
    let local = endpoint.0.local_addr().unwrap();
    assert_ne!(local.port(), peer.port);
    assert!(std::net::UdpSocket::bind(local).is_err());
    let (first, second) = tokio::join!(
        exchange(
            &connection.0,
            &[0, 255, 254, 1, 128, 127, 195, 40],
            Duration::from_secs(3)
        ),
        exchange(&connection.0, b"second", Duration::from_secs(3))
    );
    let (a, one) = first.unwrap();
    let (b, two) = second.unwrap();
    assert_eq!(a, [0, 255, 254, 1, 128, 127, 195, 40]);
    assert_eq!(b, b"second");
    assert_ne!(one, two);
    drop(connection);
    drop(endpoint);
    tokio::time::timeout(Duration::from_secs(3), async {
        while std::net::UdpSocket::bind(local).is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    peer.close().await;
}
#[tokio::test]
async fn aioquic_injected_queries_followups_disconnect_and_invalid_action() {
    let mut peer = Peer::start("quic").await;
    let state = state();
    let id=client(&state,"quic",peer.address(),peer.cert.trust(),vec![json!({"event_pattern":"quic_data_received","handler":{"type":"script","language":"python","code":"import json,sys\nevent=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'send_quic_data','data':'followup'}] if event['data']=='first' else []}))"}}),empty_handler()]).await;
    let result = state
        .send_to_client(
            id,
            json!({"type":"send_quic_data","data":"first"}),
            Duration::from_secs(4),
        )
        .await
        .unwrap();
    assert!(matches!(result, ClientSendOutcome::Executed { .. }));
    wait_log(&state, id, "followup").await;
    assert!(state
        .send_to_client(
            id,
            json!({"type":"send_quic_data","data":"zz","encoding":"hex"}),
            Duration::from_secs(2)
        )
        .await
        .is_err());
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(2))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    peer.close().await;
}
#[tokio::test]
async fn aioquic_rejects_untrusted_wrong_hostname_and_wrong_alpn() {
    let mut peer = Peer::start("quic").await;
    let untrusted = params(json!({"server_name":"localhost"}));
    assert!(
        netget::utils::quic::connect(&peer.address(), Some(&untrusted), b"netget-quic", 0)
            .await
            .is_err()
    );
    let mut bad = peer.cert.trust();
    bad["server_name"] = json!("wrong.example");
    assert!(
        netget::utils::quic::connect(&peer.address(), Some(&params(bad)), b"netget-quic", 0)
            .await
            .is_err()
    );
    assert!(netget::utils::quic::connect(
        &peer.address(),
        Some(&params(peer.cert.trust())),
        b"h3",
        0
    )
    .await
    .is_err());
    peer.close().await;
}
#[tokio::test]
async fn aioquic_oversize_missing_fin_and_reset_leave_other_streams_usable() {
    let mut peer = Peer::start("quic").await;
    let (_endpoint, connection) = netget::utils::quic::connect(
        &peer.address(),
        Some(&params(peer.cert.trust())),
        b"netget-quic",
        0,
    )
    .await
    .unwrap();
    for data in [b"oversized".as_slice(), b"no-fin", b"reset"] {
        assert!(exchange(&connection.0, data, Duration::from_millis(400))
            .await
            .is_err());
        assert_eq!(
            exchange(&connection.0, b"still-live", Duration::from_secs(2))
                .await
                .unwrap()
                .0,
            b"still-live"
        );
    }
    peer.close().await;
}
