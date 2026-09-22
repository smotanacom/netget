//! The connection cap on a real, running MQTT broker, driven from the wire.
//!
//! An MQTT connection is a *session*, not a request: a subscriber holds its socket open for the
//! life of the application, so this broker's live-connection count is its client count rather
//! than its arrival rate. Until September 2026 that count had no ceiling — the accept loop
//! admitted every connection offered to it, before any CONNECT.
//!
//! Three claims, and the second is where the version question lands:
//!
//! 1. `MAX_CONNECTIONS` peers are admitted.
//! 2. The next one is refused with **CONNACK return code 3, "Server unavailable"** — the one
//!    code MQTT defines for "I am working, come back later", and the same one this broker
//!    already sends when a handler refuses a CONNECT. The framing is 3.1.1's, which is not a
//!    guess about the peer: `build_connack` is this broker's single CONNACK site and has no
//!    version parameter, so a peer that could have completed a session here is a peer that
//!    accepts this shape. 3.1.1 §3.2 then requires the server to close, which it does.
//! 3. Closing an admitted connection **frees exactly one slot**. This is the claim that matters
//!    most here, because one MQTT connection is *two* tasks — the session and the writer it
//!    spawns — so the permit is an `Arc` held by both and released by whichever ends last.
//!    Holding it in the session alone would release the slot while a writer was still draining.
//!
//! **How this was proved to fail without the cap**: replace the `accept_bounded` call in
//! `src/server/mqtt/mod.rs` with a bare `listener.accept().await` (and drop the permit). The
//! over-cap peer is then admitted and, having sent no CONNECT, is answered with nothing — so
//! the `read_to_end` times out and the test fails on "was neither answered nor closed".
//!
//! The broker is model-free: an empty instruction really is model-free, where `None` is replaced
//! by a default one. MQTT is client-speaks-first, so a peer that never sends CONNECT provokes no
//! model call. Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features mqtt --test server -- mqtt::connection_bounds --test-threads=100

#![cfg(feature = "mqtt")]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// `src/server/mqtt/mod.rs::MAX_CONNECTIONS`. Deliberately duplicated rather than imported: if
/// the constant moves, this test should be re-read rather than silently follow it.
const MAX_CONNECTIONS: usize = 256;

/// `src/server/mqtt/mod.rs::CONNECTION_CAP_REFUSAL`: CONNACK, remaining length 2, no session
/// present, return code 3 ("Server unavailable", 3.1.1 §3.2.2.3).
const CONNECTION_CAP_REFUSAL: &[u8] = &[0x20, 0x02, 0x00, 0x03];

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
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("MQTT broker #{} never bound a port", id.as_u32());
}

async fn start_server(state: &AppState) -> (ServerId, u16) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "mqtt".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create mqtt broker");
    let port = wait_for_port(state, server_id).await;
    (server_id, port)
}

/// Wait until the accept loop has taken `n` connections out of the listen backlog. A
/// `connect()` succeeds as soon as the kernel queues it, so without this the over-cap peer
/// races the accept loop and the test measures scheduling rather than the cap.
async fn wait_for_admitted(state: &AppState, id: ServerId, n: usize) {
    for _ in 0..600 {
        if let Some(s) = state.get_server(id).await {
            if s.connections.len() >= n {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let seen = state
        .get_server(id)
        .await
        .map(|s| s.connections.len())
        .unwrap_or(0);
    panic!("the broker admitted only {seen} of {n} connections");
}

#[tokio::test]
async fn the_connection_past_the_cap_gets_connack_server_unavailable_and_the_slot_comes_back() {
    let state = new_state().await;
    let (server_id, port) = start_server(&state).await;

    let mut held = Vec::with_capacity(MAX_CONNECTIONS);
    for i in 0..MAX_CONNECTIONS {
        held.push(
            TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap_or_else(|e| panic!("connection {i} of the cap failed: {e}")),
        );
    }
    wait_for_admitted(&state, server_id, MAX_CONNECTIONS).await;

    let mut over = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the listener must still accept — a cap is not a closed socket");
    let mut refusal = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), over.read_to_end(&mut refusal))
        .await
        .expect("the connection past the cap was neither answered nor closed")
        .expect("read the refusal");
    assert_eq!(
        refusal, CONNECTION_CAP_REFUSAL,
        "the peer over the cap must read CONNACK 3 and then EOF; a silent drop leaves a client \
         unable to tell a full broker from a crashed one. Got {refusal:02x?}"
    );

    // Releasing one admitted connection must free exactly one slot — and an MQTT connection is
    // two tasks, so this is what catches a permit released by the session while the writer is
    // still alive (un-caps the broker) or never released at all (wedges it shut).
    drop(held.pop().expect("one held connection"));

    let mut admitted = false;
    for _ in 0..100 {
        let mut candidate = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect after freeing a slot");
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_millis(300), candidate.read(&mut buf)).await {
            // MQTT is client-speaks-first: silence means this peer was admitted and the broker
            // is waiting for its CONNECT.
            Err(_) => {
                admitted = true;
                break;
            }
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    assert!(
        admitted,
        "the cap never freed its slot after an admitted connection ended — the permit is being \
         held past the life of the connection, which wedges the broker shut"
    );
}
