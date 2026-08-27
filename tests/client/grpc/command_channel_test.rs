//! The dashboard's `[ send ]` path on a gRPC client: `AppState::send_to_client` injects an
//! action from outside the connect task and the call goes out on the *live* tonic channel,
//! reaching a NetGet gRPC server of our own.
//!
//! Zero LLM calls: the server answers through a `*` static handler and the client's LLM
//! points at an unreachable URL, so its connected-event call fails and the connect path must
//! tolerate that. Verifying it does is part of the point.
//!
//! Needs `protoc` on PATH - the gRPC *server* compiles its `proto_schema` by shelling out to
//! it on every code path.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features grpc --test client -- grpc::command_channel --test-threads=100

#![cfg(feature = "grpc")]

use std::time::Duration;

use netget::cli::management::{ClientForm, ServerForm};
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId, ClientStatus, ServerId};
use tokio::sync::mpsc;

const CALCULATOR_PROTO: &str = "syntax = \"proto3\"; package calculator; \
     service Calculator { rpc Add(AddRequest) returns (AddResponse); } \
     message AddRequest { int32 a = 1; int32 b = 2; } \
     message AddResponse { int32 result = 1; }";

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, false, "http://127.0.0.1:1".to_string());
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
    panic!("gRPC server #{} never bound a port", id.as_u32());
}

async fn wait_for_client_handle(state: &AppState, id: ClientId) {
    for _ in 0..1_000 {
        if state.has_client_handle(id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "gRPC client #{} never registered a command handle",
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
async fn injected_grpc_call_reaches_our_own_server() {
    if which_protoc().is_none() {
        eprintln!("skipping: protoc is not on PATH and the NetGet gRPC server requires it");
        return;
    }

    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let server_id = ServerForm {
        protocol: "grpc".to_string(),
        port: Some(0),
        startup_params: Some(serde_json::json!({ "proto_schema": CALCULATOR_PROTO })),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "*",
            "handler": {
                "type": "static",
                "actions": [ { "type": "grpc_unary_response", "message": { "result": 8 } } ]
            }
        })]),
        ..Default::default()
    }
    .create(&state, tx.clone())
    .await
    .expect("create grpc server");
    let port = wait_for_port(&state, server_id).await;

    let client_id = ClientForm {
        protocol: "grpc".to_string(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        instruction: Some("test client".to_string()),
        startup_params: Some(serde_json::json!({ "proto_schema": CALCULATOR_PROTO })),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create grpc client");

    // Regression guard for "register the channel BEFORE the connected-event LLM call":
    // the handle must exist without anything having answered that call.
    wait_for_client_handle(&state, client_id).await;

    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "call_grpc_method",
                "service": "calculator.Calculator",
                "method": "Add",
                "request": { "a": 5, "b": 3 }
            }),
            Duration::from_secs(20),
        )
        .await
        .expect("send_to_client call_grpc_method");

    // `Sent` is honest here and nowhere else in this family: we build the gRPC frame
    // ourselves (5-byte header + protobuf payload) and hand exactly those bytes to the
    // channel, and the server answered them. AddRequest{a:5,b:3} encodes to 4 bytes
    // (0x08 0x05 0x10 0x03), so the frame is 9.
    assert!(
        matches!(outcome, ClientSendOutcome::Sent { bytes_sent: 9 }),
        "expected Sent{{9}}, got {outcome:?}"
    );

    // Recorded on the client like LLM-produced traffic, and received by the server.
    wait_for_log_containing(
        &state,
        AccessLogOwner::Client(client_id.as_u32()),
        "injected_action",
    )
    .await;
    wait_for_log_containing(&state, AccessLogOwner::Server(server_id.as_u32()), "Add").await;

    // An unknown verb is refused, not silently swallowed.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "definitely_not_a_grpc_action"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_to_client unknown");
    assert!(
        matches!(outcome, ClientSendOutcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    // An injected disconnect ends the command loop and takes the handle with it.
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

fn which_protoc() -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("protoc"))
            .find(|p| p.is_file())
    })
}
