//! The connection cap on a real, running SOCKS5 proxy, driven from the wire.
//!
//! `HANDSHAKE_TIMEOUT_SECS` is 30 seconds and it bounds *one* peer: a stranger who connects and
//! says nothing holds a socket, a task and an `AppState` row for half a minute, and nothing
//! bounded how many such strangers there could be at once. A proxy is the worst case for that
//! in this tree, because an *established* connection holds two sockets — the client's and the
//! one this server opened to the target on its behalf — so an uncapped accept loop lets a
//! stranger spend this process's descriptors two at a time.
//!
//! Three claims:
//!
//! 1. `MAX_CONNECTIONS` peers are admitted.
//! 2. The next one is refused with `05 FF` — RFC 1928 §3's method-selection reply carrying
//!    `NO ACCEPTABLE METHODS` — and then closed. That message is the only thing a SOCKS5 server
//!    may send without having read anything: it echoes nothing from the greeting, so unlike a
//!    reply carrying a request id it cannot be mis-matched against something the peer never
//!    sent, and §3 requires the client to close on `X'FF'`.
//! 3. Closing an admitted connection **frees exactly one slot**. A permit dropped before the
//!    connection ends un-caps the server silently; one never released wedges it shut after
//!    `MAX_CONNECTIONS` peers have ever connected.
//!
//! **How this was proved to fail without the cap**: replace the `accept_bounded` call in
//! `src/server/socks5/mod.rs` with a bare `listener.accept().await` (and drop the permit from
//! the connection task). The over-cap peer is then admitted, so it sends nothing and waits for
//! *our* greeting, and the test fails on the `05 FF` read timing out.
//!
//! The server is model-free: an empty instruction really is model-free, where `None` is
//! replaced by a default one. SOCKS5 is client-speaks-first, so a peer that says nothing
//! provokes no model call. Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features socks5 --test server -- socks5::connection_bounds --test-threads=100

#![cfg(feature = "socks5")]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// `src/server/socks5/mod.rs::MAX_CONNECTIONS`. Deliberately duplicated rather than imported:
/// if the constant moves, this test should be re-read rather than silently follow it.
const MAX_CONNECTIONS: usize = 256;

/// `src/server/socks5/mod.rs::CONNECTION_CAP_REFUSAL`, byte for byte: version 5, method 0xFF.
const CONNECTION_CAP_REFUSAL: &[u8] = &[0x05, 0xFF];

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
    panic!("SOCKS5 server #{} never bound a port", id.as_u32());
}

async fn start_server(state: &AppState) -> (ServerId, u16) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "socks5".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create socks5 server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port)
}

/// Wait until the accept loop has taken `n` connections out of the listen backlog. A
/// `connect()` succeeds as soon as the kernel queues it, so without this the over-cap peer
/// races the accept loop and the test measures scheduling rather than the cap.
///
/// This server registers the connection at the top of `handle_connection`, before its first
/// read, so the count reflects admitted peers rather than peers that have spoken.
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
    panic!("the server admitted only {seen} of {n} connections");
}

#[tokio::test]
async fn the_connection_past_the_cap_gets_no_acceptable_methods_and_the_slot_comes_back() {
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
        .expect(
            "the connection past the cap was neither answered nor closed — it was admitted and \
             is waiting for a greeting, so there is no cap",
        )
        .expect("read to EOF");
    assert_eq!(
        refusal, CONNECTION_CAP_REFUSAL,
        "a refused peer must get RFC 1928 §3's method-selection reply with X'FF' and nothing \
         else, then EOF. Got {refusal:02x?}"
    );

    drop(held.pop().expect("one held connection"));

    let mut admitted = false;
    for _ in 0..100 {
        let mut candidate = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect after freeing a slot");
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_millis(300), candidate.read(&mut buf)).await {
            // Neither bytes nor EOF: SOCKS5 is client-speaks-first, so a connection still open
            // and still silent after the window is one that was admitted and is waiting for our
            // greeting. A refused peer gets two bytes and a close immediately instead.
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
         held past the life of the connection, which wedges the proxy shut"
    );
}
