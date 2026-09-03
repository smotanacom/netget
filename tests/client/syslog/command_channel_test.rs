//! The dashboard's `[ send ]` path on a syslog client: `AppState::send_to_client` injects a
//! `send_syslog_message` and the RFC 5424 line really leaves the socket.
//!
//! This client has no read loop at all - it is fire-and-forget on both transports - so the
//! commands are drained by a task of their own. Both transports are covered, because the
//! byte count differs: UDP reports the datagram, TCP reports the message plus its framing
//! newline.
//!
//! Zero LLM calls: every client event is routed to a static handler that answers with no
//! actions, and the client's LLM points at an unreachable URL as a second belt.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features syslog --test client -- syslog::command_channel --test-threads=100

use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
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

/// Regression guard for "register the channel before the connected-event LLM call".
async fn wait_for_client_handle(state: &AppState, id: ClientId) {
    for _ in 0..1_000 {
        if state.has_client_handle(id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "Syslog client #{} never registered a command handle",
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

fn no_llm_handlers() -> Option<Vec<serde_json::Value>> {
    Some(vec![serde_json::json!({
        "event_pattern": "*",
        "handler": { "type": "static", "actions": [] }
    })])
}

#[tokio::test]
async fn injected_syslog_message_reaches_the_wire_over_udp() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let collector = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind collector");
    let collector_addr = collector.local_addr().unwrap();

    let client_id = ClientForm {
        protocol: "syslog".to_string(),
        remote_addr: Some(collector_addr.to_string()),
        instruction: Some("test client".to_string()),
        event_handlers: no_llm_handlers(),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create syslog client");

    wait_for_client_handle(&state, client_id).await;

    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({"type": "not_a_syslog_action"}),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client rejected action");
    assert!(
        matches!(outcome, ClientSendOutcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "send_syslog_message",
                "facility": "local0",
                "severity": "info",
                "message": "dashboard-marker"
            }),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client syslog message");
    let bytes_sent = match outcome {
        ClientSendOutcome::Sent { bytes_sent } => bytes_sent,
        other => panic!("expected Sent, got {other:?}"),
    };

    let mut buf = vec![0u8; 4096];
    let (n, _from) = tokio::time::timeout(Duration::from_secs(5), collector.recv_from(&mut buf))
        .await
        .expect("no syslog datagram arrived")
        .expect("recv_from");
    let line = String::from_utf8_lossy(&buf[..n]).to_string();

    assert_eq!(n, bytes_sent, "reported byte count differs from the wire");
    // local0 (16) * 8 + info (6) = 134
    assert!(
        line.starts_with("<134>1 "),
        "expected an RFC 5424 line with PRI 134, got {line:?}"
    );
    assert!(
        line.ends_with("dashboard-marker"),
        "expected the injected message text, got {line:?}"
    );

    wait_for_log_containing(
        &state,
        AccessLogOwner::Client(client_id.as_u32()),
        "injected_action",
    )
    .await;

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
        if !state.has_client_handle(client_id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("command handle should be gone after an injected disconnect");
}

#[tokio::test]
async fn injected_syslog_message_reaches_the_wire_over_tcp() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind listener");
    let collector_addr = listener.local_addr().unwrap();
    let accepted = tokio::spawn(async move { listener.accept().await });

    let client_id = ClientForm {
        protocol: "syslog".to_string(),
        remote_addr: Some(collector_addr.to_string()),
        instruction: Some("test client".to_string()),
        startup_params: Some(serde_json::json!({"protocol": "tcp"})),
        event_handlers: no_llm_handlers(),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx.clone(),
    )
    .await
    .expect("create syslog client over tcp");

    let (mut stream, _peer) = accepted.await.expect("accept task").expect("accept");

    wait_for_client_handle(&state, client_id).await;

    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "send_syslog_message",
                "facility": "local0",
                "severity": "info",
                "message": "dashboard-marker-tcp"
            }),
            Duration::from_secs(10),
        )
        .await
        .expect("send_to_client syslog message");
    let bytes_sent = match outcome {
        ClientSendOutcome::Sent { bytes_sent } => bytes_sent,
        other => panic!("expected Sent, got {other:?}"),
    };

    // Read until the framing newline rather than once: TCP is a stream, so a single
    // `read` may return a prefix of the message. A one-shot read here passed most of
    // the time and then reported 72 wire bytes against 73 sent -- a split, not a
    // miscount.
    let mut line_bytes: Vec<u8> = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 512];
        loop {
            let read = stream.read(&mut buf).await.expect("read");
            assert!(read > 0, "syslog stream closed before the newline arrived");
            line_bytes.extend_from_slice(&buf[..read]);
            if line_bytes.ends_with(b"\n") {
                break;
            }
        }
    })
    .await
    .expect("no complete syslog line arrived");
    let n = line_bytes.len();
    let line = String::from_utf8_lossy(&line_bytes).to_string();

    // TCP frames each message with a trailing newline, and the reported count says so.
    assert_eq!(n, bytes_sent, "reported byte count differs from the wire");
    assert!(
        line.starts_with("<134>1 ") && line.ends_with("dashboard-marker-tcp\n"),
        "expected a newline-framed RFC 5424 line, got {line:?}"
    );
}
