//! The declared `jid` / `password` startup parameters must actually reach the client.
//!
//! They were declared in `get_startup_parameters()`, read in `parse_connection_info` under a
//! comment saying "try to get from startup params first", and threaded straight into
//! `tokio_xmpp::Client::new` — but read from `ClientInstance::protocol_data`, which
//! `cli/client_startup.rs` leaves as `Value::Null`. Only a client that writes its own
//! `protocol_data` ever populates it, and this one writes `jid` there *after* it has already
//! connected. So both were permanently `None` at the one moment they were wanted, every
//! connection fell through to parsing `remote_addr` as `user@domain@password`, and a caller
//! who supplied the declared parameters instead was refused outright with "Invalid XMPP
//! address format". The values arrive on `ConnectContext::startup_params`; that is what the
//! client reads now.
//!
//! **Why a bare `remote_addr` is the whole proof.** `"host:5222"` contains no `@` at all, so
//! the fallback cannot produce either half: it needs three `@`-separated parts and bails.
//! A client that connects at all therefore got *both* its JID and its password from the
//! startup parameters — there is nowhere else they could have come from.
//!
//! No LLM call is asserted: the client's model endpoint is unreachable (127.0.0.1:1) and its
//! connected-event call runs in its own task, which is free to fail. Nothing here waits on a
//! real XMPP server either — `tokio_xmpp::Client::new` is lazy, so `connect()` returns before
//! any DNS or TLS is attempted, and the JID the client recorded is what is under test.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features xmpp --test client -- xmpp::startup_params --test-threads=100

#![cfg(feature = "xmpp")]

use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::ClientId;
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

/// The JID the client actually connected as, as it recorded it on itself.
async fn recorded_jid(state: &AppState, id: ClientId) -> Option<String> {
    for _ in 0..1_000 {
        let jid = state
            .get_client(id)
            .await
            .and_then(|c| c.get_protocol_field("jid").cloned())
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        if jid.is_some() {
            return jid;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    None
}

#[tokio::test]
async fn jid_and_password_come_from_the_startup_parameters() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    // No `@` anywhere in the address: the `user@domain@password` fallback cannot fire.
    let client_id = ClientForm {
        protocol: "xmpp".to_string(),
        remote_addr: Some("xmpp.netget.invalid:5222".to_string()),
        instruction: Some("startup parameter probe".to_string()),
        startup_params: Some(serde_json::json!({
            "jid": "alice@xmpp.netget.invalid",
            "password": "s3cret-from-startup-params",
        })),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect(
        "an XMPP client given jid+password as startup parameters must connect; \
         before the fix both were read from protocol_data (always null) and this \
         failed with \"Invalid XMPP address format\"",
    );

    assert_eq!(
        recorded_jid(&state, client_id).await.as_deref(),
        Some("alice@xmpp.netget.invalid"),
        "the client must connect as the JID the caller supplied"
    );
}

/// A parameter supplied on its own is still honoured: the startup `jid` wins over the one
/// `remote_addr` encodes, and only the password falls back.
#[tokio::test]
async fn a_startup_jid_overrides_the_one_encoded_in_remote_addr() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let client_id = ClientForm {
        protocol: "xmpp".to_string(),
        remote_addr: Some("bob@xmpp.netget.invalid@from-remote-addr".to_string()),
        instruction: Some("startup parameter probe".to_string()),
        startup_params: Some(serde_json::json!({
            "jid": "alice@xmpp.netget.invalid",
        })),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create xmpp client");

    assert_eq!(
        recorded_jid(&state, client_id).await.as_deref(),
        Some("alice@xmpp.netget.invalid"),
        "the declared `jid` parameter must win over the JID encoded in remote_addr"
    );
}
