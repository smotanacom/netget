//! The dashboard's `[ send ]` path on an HTTP/3 client: `AppState::send_to_client` injects an
//! action from outside the client's own task. Zero LLM calls - the client's LLM points at an
//! unreachable URL.
//!
//! **No wire assertion here, deliberately.** There is no NetGet HTTP/3 *server* — the `http3`
//! feature builds the client only (see the note in `src/protocol/server_registry.rs`) — so
//! there is nothing of our own to send a QUIC request to, and pointing the client at a closed
//! UDP port would assert a quinn handshake timeout rather than anything about this feature.
//! What is pinned down instead is the contract the command channel owns: the handle exists as
//! soon as `connect()` returns, an unknown action is `Rejected` rather than swallowed, an
//! injected `disconnect` ends the loop and drops the handle, and every injected action lands
//! in the client's access log.
//!
//! A successful `send_http3_request` reports `Executed { detail: "http3_request GET /p -> 200
//! (N byte body)" }`, never `Sent`: h3/quinn own the datagrams and report no wire byte count
//! for the request, so a number there would be invented.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features http3 --test client -- http3::command_channel --test-threads=100

#![cfg(feature = "http3")]

use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId, ClientStatus};
use tokio::sync::mpsc;

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_client_handle(state: &AppState, id: ClientId) {
    for _ in 0..1_000 {
        if state.has_client_handle(id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "HTTP/3 client #{} never registered a command handle",
        id.as_u32()
    );
}

async fn wait_for_log_containing(state: &AppState, owner: AccessLogOwner, needle: &str) {
    for _ in 0..1_000 {
        for entry in state.list_access_logs_for(Some(owner), None).await {
            if serde_json::to_string(&entry)
                .unwrap_or_default()
                .contains(needle)
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("no access-log entry for {owner:?} containing {needle:?}");
}

#[tokio::test]
async fn http3_client_accepts_injected_actions() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    // Nothing is contacted at connect time: the HTTP/3 client only records the target and
    // opens a fresh QUIC connection per request.
    let client_id = ClientForm {
        protocol: "http3".to_string(),
        remote_addr: Some("127.0.0.1:1".to_string()),
        instruction: Some("test client".to_string()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create http3 client");

    // The handle must exist as soon as connect() returns, or the dashboard greys out
    // [ send ] on a client that is up.
    wait_for_client_handle(&state, client_id).await;

    // An action the protocol does not know is rejected, not silently swallowed.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "no_such_action"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_to_client unknown action");
    assert!(
        matches!(outcome, ClientSendOutcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    // Recorded on the client like LLM-produced traffic.
    wait_for_log_containing(
        &state,
        AccessLogOwner::Client(client_id.as_u32()),
        "injected_action",
    )
    .await;

    // An injected disconnect ends the command loop and drops the handle.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "disconnect"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_to_client disconnect");
    assert!(
        matches!(outcome, ClientSendOutcome::Disconnected),
        "expected Disconnected, got {outcome:?}"
    );

    for _ in 0..1_000 {
        let status = state.get_client(client_id).await.map(|c| c.status);
        if matches!(status, Some(ClientStatus::Disconnected))
            && !state.has_client_handle(client_id).await
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "client should be Disconnected with no command handle; status={:?} has_handle={}",
        state.get_client(client_id).await.map(|c| c.status),
        state.has_client_handle(client_id).await
    );
}
