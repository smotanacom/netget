//! The dashboard's `[ send ]` path on a DataLink client: `AppState::send_to_client` injects
//! an action from outside the client's pcap loop.
//!
//! Zero LLM calls - the client's LLM points at an unreachable URL and this client makes no
//! connected-event call at all.
//!
//! # Why this test does not assert `Sent`
//!
//! Raw frame injection goes through libpcap, which needs root (or `/dev/bpf*` access on
//! macOS, `CAP_NET_RAW` on Linux). A test that required that would be skipped in every
//! ordinary run, so this one deliberately targets an interface that does not exist: the pcap
//! loop then never opens a handle, and what is verified is everything the wiring can reach
//! without privilege -
//!
//! * the command handle is registered before the pcap task starts;
//! * an injected action is executed through the protocol's own `execute_action`, so an
//!   unknown verb and invalid hex both come back `Rejected` rather than being swallowed;
//! * a frame that cannot be handed to pcap reports `Executed` **with the reason**, never a
//!   fabricated `Sent`;
//! * the injection is recorded in the client's access log like LLM-produced traffic.
//!
//! The privileged half - `sendpacket` really putting the frame on a wire, acknowledged back
//! through the same channel so the outcome can be `Sent { bytes_sent }` - is
//! [`injected_frame_is_transmitted`], `#[ignore]`d because it needs root.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features datalink --test client -- datalink::command_channel --test-threads=100

#![cfg(feature = "datalink")]

use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId, ClientStatus};
use tokio::sync::mpsc;

/// An interface name no host has, so `Device::list()` cannot match it and the pcap loop
/// never opens a handle - deterministically, with or without privileges.
const MISSING_INTERFACE: &str = "netget-no-such-if0";

/// A 56-byte Ethernet frame (broadcast destination, ARP ethertype, zero payload). Only
/// its length matters here - the outcome must report it back.
const FRAME_HEX: &str = "ffffffffffff0011223344550806000100000000000000000000000000000000000000000000000000000000000000000000000000000000";

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, false, "http://127.0.0.1:1".to_string());
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
        "DataLink client #{} never registered a command handle",
        id.as_u32()
    );
}

/// Wait for a status line containing `needle` (lower-cased comparison).
async fn wait_for_status_line(rx: &mut mpsc::UnboundedReceiver<String>, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(line)) => {
                if line.to_lowercase().contains(needle) {
                    return;
                }
            }
            Ok(None) => panic!("status channel closed before {needle:?} was seen"),
            Err(_) => panic!("timed out waiting for a status line containing {needle:?}"),
        }
    }
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
async fn injected_datalink_actions_are_executed_and_reported_honestly() {
    let state = new_state().await;
    let (tx, mut rx) = mpsc::unbounded_channel();

    let client_id = ClientForm {
        protocol: "datalink".to_string(),
        // Unused by this client (the interface is a startup parameter) but required by the
        // form; the pcap loop is what actually talks to the network.
        remote_addr: Some("127.0.0.1:0".to_string()),
        instruction: Some("test client".to_string()),
        startup_params: Some(serde_json::json!({"interface": MISSING_INTERFACE})),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create datalink client");

    wait_for_client_handle(&state, client_id).await;

    // Wait until the pcap loop has definitively failed, so the outcome below is not a race
    // between "not injected" and "injected".
    wait_for_status_line(&mut rx, "failed to find device").await;

    // A well-formed frame: decoded exactly as the LLM path decodes it, then honestly
    // reported as not injected, naming the reason.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "inject_frame", "frame_hex": FRAME_HEX}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client inject_frame");
    match &outcome {
        ClientSendOutcome::Executed { detail } => {
            assert!(
                (detail.contains("not injected") || detail.contains("not confirmed injected"))
                    && detail.contains("pcap"),
                "the reason must say the frame did not go out and name pcap, got {detail:?}"
            );
            assert!(
                detail.contains(&format!("{}-byte", FRAME_HEX.len() / 2)),
                "the reason should say how big the frame was, got {detail:?}"
            );
        }
        other => panic!("expected Executed with a reason, got {other:?}"),
    }

    // Recorded on the client like LLM-produced traffic.
    wait_for_log_containing(
        &state,
        AccessLogOwner::Client(client_id.as_u32()),
        "injected_action",
    )
    .await;

    // Undecodable hex is rejected by the protocol, not silently dropped.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "inject_frame", "frame_hex": "zzzz"}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client bad hex");
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.to_lowercase().contains("hex")),
        "expected Rejected, got {outcome:?}"
    );

    // A verb the protocol does not know is rejected too.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "inject_vlan_frame"}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client unknown verb");
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("inject_vlan_frame")),
        "expected Rejected, got {outcome:?}"
    );

    // `disconnect` ends the command loop, drops the handle and marks the client
    // disconnected.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "disconnect"}),
            Duration::from_secs(10),
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

/// The privileged half: with a real capture open, an injected frame is acknowledged by the
/// pcap loop and the outcome carries the real frame length.
///
/// `#[ignore]` because opening a pcap handle needs root on macOS (`/dev/bpf*`) and
/// `CAP_NET_RAW` on Linux. Run it deliberately:
///
/// ```text
/// sudo ./cargo-isolated.sh test --no-default-features --features datalink --test client -- \
///     datalink::command_channel::injected_frame_is_transmitted --ignored --exact
/// ```
#[tokio::test]
#[ignore = "requires root / CAP_NET_RAW to open a libpcap handle"]
async fn injected_frame_is_transmitted() {
    let loopback = if cfg!(target_os = "linux") {
        "lo"
    } else {
        "lo0"
    };

    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let client_id = ClientForm {
        protocol: "datalink".to_string(),
        remote_addr: Some("127.0.0.1:0".to_string()),
        instruction: Some("test client".to_string()),
        startup_params: Some(serde_json::json!({"interface": loopback})),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create datalink client");

    wait_for_client_handle(&state, client_id).await;

    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "inject_frame", "frame_hex": FRAME_HEX}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client inject_frame");
    assert!(
        matches!(outcome, ClientSendOutcome::Sent { bytes_sent } if bytes_sent == FRAME_HEX.len() / 2),
        "expected Sent from an acknowledged pcap injection, got {outcome:?}"
    );
}
