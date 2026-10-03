use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
pub async fn client(state: &AppState, addr: String, pem: &str, actions: Value) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "gemini".into(),
        remote_addr: Some(addr),
        startup_params:Some(json!({"server_name":"localhost","custom_ca_cert_pem":pem})),
        instruction: Some("Read a capsule".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"gemini_connected","handler":{"type":"static","actions":actions}}),
            json!({"event_pattern":"gemini_response","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state
            .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
            .await
            .iter()
            .any(|e| e.event_type == "gemini_connected")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    id
}
pub async fn response(state: &AppState, id: ClientId, after: u64) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for e in state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
            {
                if e.id > after && e.event_type == "gemini_response" {
                    return e.request["response"].clone();
                }
            }
            if let Some(client) = state.get_client(id).await {
                if let netget::state::ClientStatus::Error(error) = client.status {
                    panic!("Gemini session error: {error}");
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Gemini response event")
}
pub async fn request(state: &AppState, id: ClientId, mut action: Value) -> Value {
    let after = state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| e.id)
        .max()
        .unwrap_or(0);
    action["type"] = json!("gemini_request");
    let outcome = state
        .send_to_client(id, action, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Sent { .. }
        ),
        "{outcome:?}"
    );
    response(state, id, after).await
}
pub fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

pub fn certificate() -> (String, Vec<u8>, Vec<u8>, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    (
        cert.pem(),
        cert.der().to_vec(),
        key.serialize_der(),
        key.serialize_pem(),
    )
}
pub fn acceptor(cert: Vec<u8>, key: Vec<u8>) -> tokio_rustls::TlsAcceptor {
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![rustls::pki_types::CertificateDer::from(cert)],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key).into(),
    )
    .unwrap();
    tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config))
}
pub async fn error(state: &AppState, id: ClientId) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(c) = state.get_client(id).await {
                if let netget::state::ClientStatus::Error(error) = c.status {
                    return error;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Expected failed Gemini session")
}
