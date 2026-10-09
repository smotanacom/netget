#![allow(dead_code)]
use netget::{
    cli::management::ClientForm,
    state::{AccessLogOwner, AppState, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
pub fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}
pub fn static_handler(event: &str, actions: Value) -> Value {
    json!({"event_pattern":event,"handler":{"type":"static","actions":actions}})
}
pub async fn client(state: &AppState, addr: String, handlers: Vec<Value>) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "nsq".into(),
        remote_addr: Some(addr),
        startup_params: Some(json!({"heartbeat_interval_ms":1000})),
        event_handlers: Some(handlers),
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
        while !state.has_client_handle(id).await {
            if state.get_client(id).await.is_some_and(|client| {
                matches!(client.status, netget::state::ClientStatus::Error(_))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    id
}
pub async fn connected_client(state: &AppState, addr: String) -> ClientId {
    let id = client(state, addr, vec![static_handler("*", json!([]))]).await;
    event(state, id, "nsq_connected", 0).await;
    id
}
pub async fn latest(state: &AppState, id: ClientId) -> u64 {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| e.id)
        .max()
        .unwrap_or(0)
}
pub async fn event(state: &AppState, id: ClientId, name: &str, after: u64) -> (u64, Value) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for e in state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
            {
                if e.id > after && e.event_type == name {
                    return (e.id, e.request.clone());
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("NSQ event {name} after {after}"))
}
pub async fn send(state: &AppState, id: ClientId, mut action: Value) {
    action["type"] = json!("nsq_request");
    let outcome = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Sent { .. }
        ),
        "{outcome:?}"
    );
}
pub async fn request(state: &AppState, id: ClientId, action: Value) -> Value {
    let after = latest(state, id).await;
    send(state, id, action).await;
    event(state, id, "nsq_response", after).await.1
}
