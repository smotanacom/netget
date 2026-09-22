//! The connection cap on a real, running WebRTC server, driven from the wire.
//!
//! `SIGNALLING_HANDSHAKE_TIMEOUT` (10s) bounds how long *one* peer holds a socket before it has
//! upgraded, and `max_peers` bounds how many *offers* are accepted — but `max_peers` counts
//! peers that have already upgraded, sent an SDP offer and been approved by a handler, so a
//! stranger who merely connects is nowhere near it. Until September 2026 nothing bounded the
//! number of pre-admission signalling connections at all, which is the population an attacker
//! actually controls.
//!
//! Three claims:
//!
//! 1. `MAX_CONNECTIONS` peers are admitted, each completing a real RFC 6455 upgrade.
//!    Handshaking them rather than leaving them silent is what keeps claim 3 honest: a peer
//!    that only opens a socket is evicted by `SIGNALLING_HANDSHAKE_TIMEOUT` after ten seconds, which would
//!    free slots on its own and let the slot-return assertion pass for a reason that has
//!    nothing to do with the permit.
//! 2. The next one is refused with `HTTP/1.1 503` and `Retry-After`. This server's own refusal for a peer over
//!    `max_peers` is a `Rejected` signalling frame, and it is unreachable here: that is a
//!    WebSocket text message, and a peer refused at the accept has not upgraded, so it has no
//!    frame parser running. What it *is* in the middle of is an ordinary HTTP/1.1 GET
//!    (RFC 6455 §4.1), so 503 is inside the protocol rather than beside it.
//! 3. Closing an admitted connection **frees exactly one slot**. A permit dropped before the
//!    connection ends un-caps the server silently; one never released wedges it shut after
//!    `MAX_CONNECTIONS` peers have ever connected. The connection task here spawns a writer,
//!    but `.await`s it on its only exit path, so the single permit it holds covers both.
//!
//! **How this was proved to fail without the cap**: replace the `accept_bounded` call in
//! `src/server/webrtc/mod.rs` with a bare `listener.accept().await` (and drop the permit from
//! the connection task). The over-cap peer is then upgraded and the test fails on reading 101
//! where it demanded 503.
//!
//! The server is model-free: an empty instruction really is model-free, where `None` is
//! replaced by a default one. The WebSocket upgrade itself consults nothing, and no signalling
//! frame is ever sent, so 256 connections cost no model calls at all. Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features webrtc --test server -- webrtc::connection_bounds --test-threads=100

#![cfg(feature = "webrtc")]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// `src/server/webrtc/mod.rs::MAX_CONNECTIONS`. Deliberately duplicated rather than imported: if
/// the constant moves, this test should be re-read rather than silently follow it.
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
    panic!("WebRTC server #{} never bound a port", id.as_u32());
}

async fn start_server(state: &AppState) -> (ServerId, u16) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "webrtc".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create webrtc server");
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
    // Read only as far as the end of the status line; stopping there leaves an upgraded
    // connection's frames untouched.
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

    // Every one of them is an upgraded session with no deadline on it, so none can have been
    // evicted by the handshake bound: whatever slot appears below came from the permit.
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
        "the cap never freed its slot after an upgraded connection ended — the permit is being \
         held past the life of the connection, which wedges the server shut"
    );
}
