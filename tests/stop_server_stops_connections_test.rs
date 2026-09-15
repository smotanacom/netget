//! Stopping a server must stop its **in-flight connections**, not just its listener.
//!
//! The root `CLAUDE.md` recorded this as a known systemic issue: "Per-connection tasks are
//! untracked, so `stop_server` does not cancel in-flight connections." A measurement on
//! 15 September 2026 put numbers on it — 301 `tokio::spawn` calls across `src/server/*/mod.rs`
//! against 159 `register_server_task` calls, in 102 protocols. The shape is consistent: the
//! accept loop is the handle a protocol remembers to register, and the connection it just
//! accepted is the one it forgets.
//!
//! What that costs is worse than a leak. A stopped server released its listening socket, so it
//! *looked* stopped — the port was free, the instance was gone from state — while every
//! connection already open kept reading, kept calling the model and kept answering. The
//! operator had no way to tell, and no way to stop it short of killing the process.
//!
//! `AppState::spawn_server_task` is the fix: spawn and register in one call, so the easy path
//! is the correct one. These tests are the contract, asserted in-process against the live
//! `AppState` rather than through a subprocess, so they can see the handle vector itself.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp \
//!       --test stop_server_stops_connections_test -- --test-threads=100

#![cfg(feature = "tcp")]

use std::sync::Arc;
use std::time::Duration;

use netget::llm::ollama_client::OllamaClient;
use netget::protocol::{server_registry, SpawnContext};
use netget::state::app_state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Start a real TCP server through the registry, on an OS-chosen port, with no model behind it.
///
/// The instruction is empty on purpose: `ServerForm::create` substitutes a default instruction
/// when one is `None`, and any non-empty instruction makes `operator_wants_dynamic` true — so a
/// server built carelessly here would consult the model and this test would be measuring the
/// LLM path instead of the connection path. `CLAUDE.md` records two tests that documented
/// "zero LLM calls" while doing the opposite.
async fn start_tcp_server() -> (Arc<AppState>, netget::state::ServerId, u16) {
    let state = Arc::new(AppState::new());
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<String>();
    // Drain, or the unbounded channel simply grows; nothing here asserts on status text.
    tokio::spawn(async move { while status_rx.recv().await.is_some() {} });

    // `add_server` allocates the real id and overwrites whatever was passed in, so the
    // placeholder here is never used.
    let server_id = state
        .add_server(netget::state::server::ServerInstance::new(
            netget::state::ServerId::new(0),
            0,
            "TCP".to_string(),
            String::new(),
        ))
        .await;

    let protocol = server_registry::registry()
        .get("TCP")
        .expect("the tcp feature is enabled for this test");

    #[allow(deprecated)]
    let ctx = SpawnContext {
        listen_addr: "127.0.0.1:0".parse().expect("a literal address"),
        mac_address: None,
        interface: None,
        host: Some("127.0.0.1".to_string()),
        port: Some(0),
        llm_client: OllamaClient::new("http://127.0.0.1:1".to_string()),
        state: state.clone(),
        status_tx,
        server_id,
        startup_params: None,
    };

    let addr = protocol.spawn(ctx).await.expect("the server must start");
    (state, server_id, addr.port())
}

/// How many background tasks the server currently owns.
async fn task_count(state: &AppState, id: netget::state::ServerId) -> usize {
    state.server_task_count(id).await
}

/// Poll a condition to a deadline. A fixed sleep is what produced this repository's
/// load-flakiness; `CLAUDE.md` is explicit about waiting on the condition instead.
async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// The headline contract: a peer holding an open connection reads EOF once the server stops.
///
/// Asserted from the peer's side on purpose. Internal bookkeeping can say a task was aborted
/// while the socket stays open — only the peer can tell you the connection is really gone, and
/// the peer is who was being lied to.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_server_disconnects_a_live_peer() {
    let (state, server_id, port) = start_tcp_server().await;

    let mut peer = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the running server");
    peer.write_all(b"hello\n").await.expect("write to the peer");

    // The connection must be live before the stop, or this test passes for the wrong reason:
    // a peer that never connected also reads EOF.
    let baseline = 1; // the accept loop
    let live = wait_until(Duration::from_secs(10), || async {
        task_count(&state, server_id).await > baseline
    })
    .await;
    assert!(
        live,
        "no per-connection task was ever registered (count={}), so there is nothing for stop \
         to abort and the rest of this test would prove nothing",
        task_count(&state, server_id).await
    );

    state.remove_server(server_id).await;

    // A live-but-idle connection produces no bytes; an aborted one produces EOF or a reset.
    // Reading is the only thing that distinguishes them, and the timeout is what makes a
    // failure observable rather than a hang.
    let mut buf = [0u8; 64];
    match tokio::time::timeout(Duration::from_secs(10), peer.read(&mut buf)).await {
        Ok(Ok(0)) => {}
        Ok(Ok(n)) => panic!(
            "the stopped server sent {n} more bytes: {:?}",
            &buf[..n.min(64)]
        ),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Ok(Err(e)) => panic!("unexpected error reading after stop: {e}"),
        Err(_) => panic!(
            "the connection was still open 10s after the server stopped. The listener was \
             released but the per-connection task was never registered, so it is still \
             running: still reading, still able to call the model, on a server the operator \
             stopped."
        ),
    }
}

/// Registering per connection must not grow the handle vector without bound.
///
/// `register_server_task` prunes finished handles for exactly this reason. Without the
/// pruning, a long-lived server accumulates one `JoinHandle` for every connection it has ever
/// accepted — which turns the fix above into a slow leak, and is why the naive "just register
/// everything" change needs this half asserted too.
#[tokio::test(flavor = "multi_thread")]
async fn per_connection_registration_does_not_accumulate_handles() {
    let (state, server_id, port) = start_tcp_server().await;

    let held = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let grew = wait_until(Duration::from_secs(10), || async {
        task_count(&state, server_id).await > 1
    })
    .await;
    assert!(grew, "holding a connection open registered no task");
    let with_one = task_count(&state, server_id).await;

    // Thirty connections opened and closed. Their tasks finish, so the next registration
    // prunes them and the count must not climb by thirty.
    for _ in 0..30 {
        let s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        drop(s);
    }
    let _probe = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");

    let settled = wait_until(Duration::from_secs(10), || async {
        task_count(&state, server_id).await < with_one + 20
    })
    .await;
    let after = task_count(&state, server_id).await;
    assert!(
        settled,
        "the handle vector grew to {after} after 31 short connections (was {with_one} with \
         one live). Finished handles are not being pruned, so a long-lived server accumulates \
         a JoinHandle for every connection it has ever accepted."
    );

    drop(held);
    state.remove_server(server_id).await;
}

/// Stopping must release the listening port, every time.
///
/// Cheap, and it is the half that `register_server_task` was originally written for — the
/// accept loop's own handle. Keeping it beside the connection test means a change that fixes
/// one by breaking the other cannot land quietly.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_server_releases_its_port() {
    let (state, server_id, port) = start_tcp_server().await;
    state.remove_server(server_id).await;

    let rebound = wait_until(Duration::from_secs(10), || async {
        tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .is_ok()
    })
    .await;

    assert!(
        rebound,
        "port {port} was still bound 10s after the server was removed; the accept loop's \
         JoinHandle was dropped rather than aborted, and dropping only detaches a task"
    );
}
