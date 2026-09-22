//! The connection cap on a real, running WebSocket server, driven from the wire.
//!
//! `HANDSHAKE_TIMEOUT_SECS` (15s) bounds how long a peer may hold a connection *before* it has
//! sent a request head. Nothing bounds an upgraded connection at all, and nothing should — a
//! WebSocket is a session the client is entitled to hold open in silence, which is the whole
//! point of the protocol. That makes the cap the only bound this server has on its total rather
//! than the second of two, and until September 2026 there was none.
//!
//! Three claims:
//!
//! 1. `MAX_CONNECTIONS` peers are admitted, each completing a real RFC 6455 upgrade. Handshaking
//!    them rather than leaving them silent is what keeps claim 3 honest: a peer that only opens
//!    a socket is evicted after fifteen seconds, which would free slots on its own and let the
//!    slot-return assertion pass for a reason that has nothing to do with the permit.
//! 2. The next one is refused with `HTTP/1.1 503` and `Retry-After`. A WebSocket client is an
//!    HTTP client first — RFC 6455 §4.1 makes the opening handshake an ordinary GET and §4.2.2
//!    has the server answer anything it will not upgrade with a normal HTTP response — so 503
//!    is inside the protocol rather than beside it, and every client surfaces a non-101 status
//!    as a failed handshake carrying that code. A close frame would be wrong twice over: it
//!    belongs to a connection that was never upgraded, and the peer has no frame parser running.
//! 3. Closing an admitted connection **frees exactly one slot**, and this is the claim the
//!    server's own structure puts at risk. One upgraded WebSocket here is *two* tasks, and the
//!    read loop waits only five seconds for the writer before giving up on it, so the permit is
//!    an `Arc` held by both halves and the slot comes back when the later of the two ends.
//!    Holding it in the reader alone would release the slot while a writer was still draining.
//!
//! **How this was proved to fail without the cap**: replace the `accept_bounded` call in
//! `src/server/websocket/mod.rs` with a bare `listener.accept().await` (and drop the permit from
//! the connection task). The over-cap peer is then upgraded and the test fails on reading 101
//! where it demanded 503.
//!
//! The server is model-free. The handshake is answered by a static `accept_websocket` rule and
//! everything after it by a zero-action `*` rule, so 256 upgrades cost no model calls at all —
//! without that, each one would be an LLM round-trip against a dead endpoint. Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features websocket --test server -- websocket::connection_bounds --test-threads=100

#![cfg(feature = "websocket")]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// `src/server/websocket/mod.rs::MAX_CONNECTIONS`. Deliberately duplicated rather than
/// imported: if the constant moves, this test should be re-read rather than silently follow it.
const MAX_CONNECTIONS: usize = 256;

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
    panic!("WebSocket server #{} never bound a port", id.as_u32());
}

async fn start_server(state: &AppState) -> (ServerId, u16) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "websocket".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(vec![
            serde_json::json!({
                "event_pattern": "websocket_handshake",
                "handler": {"type": "static", "actions": [{"type": "accept_websocket"}]}
            }),
            // Everything after the upgrade is answered with nothing. First-match-wins, so this
            // must come second or it would swallow the handshake too.
            serde_json::json!({
                "event_pattern": "*",
                "handler": {"type": "static", "actions": []}
            }),
        ]),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create websocket server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port)
}

/// The status line of the server's answer to one opening handshake, and the socket it came on.
///
/// 101 means the peer was admitted and upgraded; 503 means it was refused at the accept. The
/// socket is returned so an admitted connection can be *held*, which is what fills the cap.
async fn attempt_handshake(port: u16) -> (u16, TcpStream) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the listener must still accept — a cap is not a closed socket");
    // A fixed nonce keeps this reproducible; RFC 6455 only requires 16 random bytes, base64'd.
    let request = format!(
        "GET / HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write handshake");
    stream.flush().await.expect("flush handshake");

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Read only as far as the end of the status line; the rest of the head is not what this
    // test is about, and stopping here keeps an upgraded connection's frames untouched.
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte))
            .await
            .expect("the server neither upgraded nor refused this handshake")
            .expect("read the status line");
        assert!(n == 1, "the server closed without answering the handshake");
        if byte[0] == b'\n' {
            break;
        }
        head.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&head).trim().to_string();
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("unparseable status line {line:?}"));
    (status, stream)
}

#[tokio::test]
async fn the_handshake_past_the_cap_gets_a_503_and_the_slot_comes_back() {
    let state = new_state().await;
    let (_server_id, port) = start_server(&state).await;

    let mut held = Vec::with_capacity(MAX_CONNECTIONS);
    for i in 0..MAX_CONNECTIONS {
        let (status, stream) = attempt_handshake(port).await;
        assert_eq!(
            status, 101,
            "handshake {i} of the cap was not upgraded (status {status}); the cap must admit \
             MAX_CONNECTIONS peers before it refuses any"
        );
        held.push(stream);
    }

    let (status, _over) = attempt_handshake(port).await;
    assert_eq!(
        status, 503,
        "the handshake past the cap was upgraded instead of refused, so there is no cap"
    );

    // Every one of the 256 is an upgraded session with no deadline on it, so none can have been
    // evicted by HANDSHAKE_TIMEOUT_SECS: whatever slot appears below came from the permit — and
    // from *both* halves of it, since the writer task holds its own clone.
    assert_eq!(
        held.len(),
        MAX_CONNECTIONS,
        "the held connections must all still be open for the next assertion to mean anything"
    );
    drop(held.pop().expect("one held connection"));

    let mut admitted = false;
    for _ in 0..100 {
        let (status, _stream) = attempt_handshake(port).await;
        if status == 101 {
            admitted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        admitted,
        "the cap never freed its slot after an upgraded connection ended — a permit held past \
         the life of the connection (by the reader or by the writer task) wedges the server shut"
    );
}
