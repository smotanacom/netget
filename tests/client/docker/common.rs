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
pub async fn refused_start(state: &AppState, addr: String, params: Value) -> String {
    let (tx, _) = mpsc::unbounded_channel();
    ClientForm {
        protocol: "docker".into(),
        remote_addr: Some(addr),
        startup_params: Some(params),
        event_handlers: Some(vec![static_handler("*", json!([]))]),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect_err("startup must refuse before client becomes available")
    .to_string()
}
pub async fn client(
    state: &AppState,
    addr: String,
    params: Value,
    handlers: Vec<Value>,
) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "docker".into(),
        remote_addr: Some(addr),
        startup_params: Some(params),
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
    let id = client(state, addr, json!({}), vec![static_handler("*", json!([]))]).await;
    event(state, id, "docker_connected", 0).await;
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
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.id > after && e.event_type == name)
                .min_by_key(|e| e.id)
            {
                return (e.id, e.request.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match result {
        Ok(event) => event,
        Err(_) => panic!(
            "Docker event {name} after {after}; events/errors: {:?}",
            state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .take(12)
                .map(|e| (e.id, &e.event_type, e.request.get("error")))
                .collect::<Vec<_>>()
        ),
    }
}
pub async fn send(state: &AppState, id: ClientId, mut action: Value) {
    if action.get("type").is_none() {
        action["type"] = json!("docker_request");
    }
    let outcome = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Executed { .. }
        ),
        "{outcome:?}"
    );
}
pub async fn request(state: &AppState, id: ClientId, action: Value) -> Value {
    let after = latest(state, id).await;
    send(state, id, action).await;
    event(state, id, "docker_response", after).await.1
}

pub async fn rejected(state: &AppState, id: ClientId, action: Value, needle: &str) {
    let result = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    match result {
        netget::state::client_handles::ClientSendOutcome::Rejected { error } => {
            assert!(error.contains(needle), "{error}")
        }
        other => panic!("{other:?}"),
    }
}
pub async fn failed(state: &AppState, id: ClientId, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let netget::state::ClientStatus::Error(e) =
                state.get_client(id).await.unwrap().status
            {
                assert!(e.contains(needle), "{e}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
