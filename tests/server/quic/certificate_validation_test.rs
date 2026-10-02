//! Invalid QUIC SANs fail before binding; no QUIC peer or model is required.
use netget::llm::actions::protocol_trait::{Protocol, Server};
use netget::protocol::{SpawnContext, StartupParams};
use netget::server::quic::actions::QuicProtocol;
use netget::state::{AppState, ServerId};
use std::sync::Arc;

#[tokio::test]
async fn quic_rejects_non_string_san_entries() {
    let protocol = QuicProtocol::new();
    for value in [
        serde_json::json!(7),
        serde_json::Value::Null,
        serde_json::json!({}),
    ] {
        let (status_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let result = protocol
            .spawn(SpawnContext {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                mac_address: None,
                interface: None,
                host: Some("127.0.0.1".into()),
                port: Some(0),
                llm_client: netget::llm::OllamaClient::new("http://127.0.0.1:1"),
                state: Arc::new(AppState::new_with_options(
                    false,
                    "http://127.0.0.1:1".into(),
                )),
                status_tx,
                server_id: ServerId::new(999),
                startup_params: Some(
                    StartupParams::new(
                        serde_json::json!({"san_dns_names": ["localhost", value]}),
                        protocol.get_startup_parameters(),
                    )
                    .unwrap(),
                ),
            })
            .await;
        assert!(format!("{:#}", result.unwrap_err()).contains("san_dns_names[1]"));
    }
}
