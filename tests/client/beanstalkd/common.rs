use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
pub async fn client(state: &AppState, addr: String, actions: Value) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    let id=ClientForm{protocol:"beanstalkd".into(),remote_addr:Some(addr),instruction:Some("Inspect work queue".into()),event_handlers:Some(vec![json!({"event_pattern":"beanstalkd_connected","handler":{"type":"static","actions":actions}}),json!({"event_pattern":"beanstalkd_response","handler":{"type":"static","actions":[]}})]),..Default::default()}.create(state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(id).await {
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
                if e.id > after && e.event_type == "beanstalkd_response" {
                    return e.request["response"].clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Beanstalkd response event")
}
pub async fn request(state: &AppState, id: ClientId, mut action: Value) -> Value {
    let after = state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| e.id)
        .max()
        .unwrap_or(0);
    action["type"] = json!("beanstalkd_request");
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
