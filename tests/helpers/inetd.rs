//! Shared by the inetd server and client suites: start one of NetGet's inetd servers with
//! static or script handlers and no reachable model.
use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;

pub async fn start(
    protocol: &str,
    handlers: Vec<Value>,
    params: Value,
) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: protocol.into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Run the service".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

pub fn answer(event: &str, action: Value) -> Vec<Value> {
    vec![json!({"event_pattern": event, "handler": {"type":"static","actions":[action]}})]
}
