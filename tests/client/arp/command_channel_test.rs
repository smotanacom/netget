//! The dashboard's `[ send ]` path on an ARP client: `AppState::send_to_client` injects an
//! action from outside the client's capture loop.
//!
//! Zero LLM calls - the client's LLM points at an unreachable URL, so its started-event call
//! fails and the client must tolerate that (part of what this verifies).
//!
//! # Why this test does not assert `Sent`
//!
//! ARP capture *and* injection go through libpcap, which needs root (or `/dev/bpf*` access on
//! macOS, `CAP_NET_RAW` on Linux). A test that required that would be skipped in every
//! ordinary run, so this one deliberately targets an interface that does not exist: the pcap
//! thread then never starts, and what is verified is everything the wiring can reach without
//! privilege -
//!
//! * the command handle is registered **before** the started-event LLM call (rule: a manual
//!   `*` routing rule can park that call for minutes);
//! * an injected action is executed through the protocol's own `execute_action`, so an
//!   unknown verb and an unbuildable frame both come back `Rejected` rather than being
//!   swallowed;
//! * a frame that cannot be handed to pcap reports `Executed` **with the reason**, never a
//!   fabricated `Sent`;
//! * the injection is recorded in the client's access log like LLM-produced traffic.
//!
//! The privileged half - `sendpacket` really putting 42 bytes on a wire, acknowledged back
//! through the same channel so the outcome can be `Sent { bytes_sent: 42 }` - is
//! [`injected_arp_request_is_transmitted`], `#[ignore]`d because it needs root.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features arp --test client -- arp::command_channel --test-threads=100

#![cfg(feature = "arp")]

use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId, ClientStatus};
use tokio::sync::mpsc;

/// An interface name no host has, so `Device::list()` cannot match it and the pcap thread
/// never opens a handle - deterministically, with or without privileges.
const MISSING_INTERFACE: &str = "netget-no-such-if0";

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
        "ARP client #{} never registered a command handle",
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
async fn injected_arp_actions_are_executed_and_reported_honestly() {
    let state = new_state().await;
    let (tx, mut rx) = mpsc::unbounded_channel();

    // For the ARP client, `remote_addr` is the interface to capture on.
    let client_id = ClientForm {
        protocol: "arp".to_string(),
        remote_addr: Some(MISSING_INTERFACE.to_string()),
        instruction: Some("test client".to_string()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create arp client");

    // Regression guard for "register the channel before the started-event LLM call".
    wait_for_client_handle(&state, client_id).await;

    // Wait until the pcap thread has definitively failed, so the outcome below is not a
    // race between "not injected" and "injected".
    wait_for_status_line(&mut rx, "failed to find device").await;

    // A well-formed request: built exactly as the LLM path builds it, then honestly
    // reported as not injected, naming the reason.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "send_arp_request",
                "sender_mac": "aa:bb:cc:dd:ee:ff",
                "sender_ip": "127.0.0.1",
                "target_ip": "127.0.0.2",
            }),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client send_arp_request");
    match &outcome {
        ClientSendOutcome::Executed { detail } => {
            assert!(
                detail.contains("not injected") && detail.contains("pcap"),
                "the reason must name the pcap failure, got {detail:?}"
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

    // Fields that cannot become a frame are rejected, not silently dropped.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "send_arp_reply",
                "sender_mac": "not-a-mac",
                "sender_ip": "127.0.0.1",
                "target_mac": "aa:bb:cc:dd:ee:ff",
                "target_ip": "127.0.0.2",
            }),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client malformed send_arp_reply");
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("send_arp_reply")),
        "expected Rejected, got {outcome:?}"
    );

    // A verb the protocol does not know is rejected too.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "send_rarp_request"}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client unknown verb");
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("send_rarp_request")),
        "expected Rejected, got {outcome:?}"
    );

    // `stop_capture` ends the command loop, drops the handle and marks the client
    // disconnected.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "stop_capture"}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client stop_capture");
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

/// The privileged half: with a real capture open, an injected ARP request is acknowledged by
/// the pcap injection thread and the outcome carries the real frame length (42 bytes: 14 of
/// Ethernet header + 28 of ARP).
///
/// `#[ignore]` because opening a pcap handle needs root on macOS (`/dev/bpf*`) and
/// `CAP_NET_RAW` on Linux. Run it deliberately:
///
/// ```text
/// sudo ./cargo-isolated.sh test --no-default-features --features arp --test client -- \
///     arp::command_channel::injected_arp_request_is_transmitted --ignored --exact
/// ```
#[tokio::test]
#[ignore = "requires root / CAP_NET_RAW to open a libpcap handle"]
async fn injected_arp_request_is_transmitted() {
    let loopback = if cfg!(target_os = "linux") {
        "lo"
    } else {
        "lo0"
    };

    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let client_id = ClientForm {
        protocol: "arp".to_string(),
        remote_addr: Some(loopback.to_string()),
        instruction: Some("test client".to_string()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create arp client");

    wait_for_client_handle(&state, client_id).await;

    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "send_arp_request",
                "sender_mac": "aa:bb:cc:dd:ee:ff",
                "sender_ip": "127.0.0.1",
                "target_ip": "127.0.0.2",
            }),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client send_arp_request");
    assert!(
        matches!(outcome, ClientSendOutcome::Sent { bytes_sent: 42 }),
        "expected Sent{{42}} from an acknowledged pcap injection, got {outcome:?}"
    );
}
