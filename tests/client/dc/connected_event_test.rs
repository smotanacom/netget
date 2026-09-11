//! The DC client's `dc_connected` handler must be able to answer with an action.
//!
//! `handle_connected_event` passed `&client_state.lock().await.memory` straight into
//! `call_llm_for_client`. A temporary in a `match` scrutinee lives until the end of the whole
//! `match`, so the `MutexGuard` was still held inside the arms — and `apply_dc_action`'s first
//! line locks the same non-reentrant tokio `Mutex` for the nickname. So **any** action returned
//! for this event deadlocked the read loop on the first message of the session: no `$Key`, no
//! `$ValidateNick`, no `$MyINFO`, the handshake never completing, and the client still reporting
//! `Connected`. `dc_connected`'s own advertised answer is `{"type": "wait_for_more"}` — a
//! non-empty action list — so the documented reply was enough to trigger it.
//!
//! No existing test reached that arm: `e2e_test.rs` points at a port with nothing listening and
//! `command_channel_test.rs` points the model at `http://127.0.0.1:1`, so
//! `call_llm_for_client` always returned `Err` and the `Err` arm takes no lock.
//!
//! This test reaches it with **zero LLM calls**, through a *static client event handler* — those
//! are dispatched by `try_execute_client_event_handler` inside `call_llm_for_client`, before the
//! budget debit, and return through exactly the same `Ok` arm.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features dc --test client -- dc::connected_event --test-threads=100

#![cfg(feature = "dc")]

use std::time::Duration;

use netget::cli::management::{ClientForm, ServerForm};
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use tokio::sync::mpsc;

async fn new_state() -> AppState {
    // Unreachable LLM on purpose: every answer in this test comes from a static handler, so a
    // single real model call would be a bug the test should notice as a timeout.
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..1_000 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("DC server #{} never bound a port", id.as_u32());
}

async fn wait_for_log_containing(state: &AppState, owner: AccessLogOwner, needle: &str) -> bool {
    for _ in 0..600 {
        for entry in state.list_access_logs_for(Some(owner), None).await {
            if serde_json::to_string(&entry)
                .unwrap_or_default()
                .contains(needle)
            {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    false
}

#[tokio::test]
async fn dc_connected_handler_can_answer_with_an_action() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    // Our own hub, answering everything with a static $HubName so it needs no model either.
    let server_id = ServerForm {
        protocol: "dc".to_string(),
        port: Some(0),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "*",
            "handler": {
                "type": "static",
                "actions": [ { "type": "send_dc_hubname", "name": "StaticHub" } ]
            }
        })]),
        ..Default::default()
    }
    .create(&state, tx.clone())
    .await
    .expect("create dc server");
    let port = wait_for_port(&state, server_id).await;

    // The client answers `dc_connected` with a real wire verb. Before the fix this locked up
    // inside `handle_connected_event` and nothing further was ever written.
    let client_id = ClientForm {
        protocol: "dc".to_string(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        instruction: Some("greet the hub".to_string()),
        startup_params: Some(serde_json::json!({"nickname": "alice"})),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "dc_client_connected",
            "handler": {
                "type": "static",
                "actions": [
                    { "type": "send_dc_chat", "message": "greeting-marker" }
                ]
            }
        })]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create dc client");

    let owner = AccessLogOwner::Server(server_id.as_u32());

    // The action the handler returned reached the hub.
    assert!(
        wait_for_log_containing(&state, owner, "greeting-marker").await,
        "the dc_connected handler's action never reached the hub: handle_connected_event is \
         holding the client_state guard across the call again (see this file's header)"
    );

    // And the handshake continued past that call. `$Key`, `$ValidateNick` and `$MyINFO` are
    // written *after* the match block, so a deadlock inside it swallowed the whole handshake —
    // this is the half that shows the client was not merely slow.
    assert!(
        wait_for_log_containing(&state, owner, "$ValidateNick").await,
        "the client never sent $ValidateNick, so it never returned from the dc_connected call"
    );

    let _ = client_id;
}
