//! An 18-byte unauthenticated request must not take the broker down.
//!
//! `kafka-protocol` pre-allocated every array from the peer-supplied count
//! (`Vec::with_capacity(n)` with no look at the bytes remaining), so a Metadata request
//! whose topics array declares 0x7fffffff entries asked for ~144 GiB before reading its
//! first (absent) element. That is an allocator failure, which aborts the whole process:
//! not a panic, so nothing caught it and the broker stayed "Running" in no one's memory.
//! vendor/kafka-protocol carries the bound; this test drives the bytes at a real broker.
//!
//! The model is a closed port and is never reached: the decode fails before any event.
//!
//! Run with:
//!   cargo test --no-default-features --features kafka --test server -- kafka::declared_array_length

#![cfg(feature = "kafka")]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
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

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..200 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("Kafka broker #{} never bound a port", id.as_u32());
}

/// size=14, api_key=3 (Metadata), api_version=1, correlation_id=0, client_id=null,
/// topics array length = 0x7fffffff.
const HOSTILE_METADATA: [u8; 18] = [
    0x00, 0x00, 0x00, 0x0e, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x7f, 0xff,
    0xff, 0xff,
];

#[tokio::test]
async fn a_request_declaring_two_billion_topics_leaves_the_broker_running() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "kafka".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .expect("create kafka broker");
    let port = wait_for_port(&state, server_id).await;

    let mut hostile = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    hostile.write_all(&HOSTILE_METADATA).await.unwrap();
    // The broker answers or closes; either way it must not hang and must not die.
    let mut sink = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), hostile.read_to_end(&mut sink)).await;

    // Still here: a fresh connection is accepted and a well-formed ApiVersions request is
    // answered (api_key 18 v0 has an empty body).
    let mut probe = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the broker must still accept connections");
    let api_versions: [u8; 14] = [
        0x00, 0x00, 0x00, 0x0a, 0x00, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2a, 0xff, 0xff,
    ];
    probe.write_all(&api_versions).await.unwrap();
    let mut head = [0u8; 8];
    tokio::time::timeout(Duration::from_secs(10), probe.read_exact(&mut head))
        .await
        .expect("the broker must still answer")
        .expect("a response frame");
    assert_eq!(
        &head[4..8],
        &[0, 0, 0, 0x2a],
        "the reply must carry the probe's correlation id"
    );
}
