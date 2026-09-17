//! Drives the pair of connection bounds every HTTP-shaped netget server declares, from the wire.
//!
//! # What is being tested, and why it needed a shared driver
//!
//! Ten servers here are the same shape — a hyper `http1` connection per accepted socket, a
//! `service_fn` that calls the model — and until September 2026 all ten accepted without limit
//! and bounded no read in time. A peer that connected and said nothing held a socket, a task and
//! an `AppState` entry forever, pre-authentication, on a server that would happily accept a
//! hundred more. Each now declares three numbers in its own `mod.rs`, argued there:
//! `FIRST_BYTE_READ_TIMEOUT`, `IDLE_BETWEEN_REQUESTS_TIMEOUT` and `MAX_CONNECTIONS`.
//!
//! The assertions are identical across the ten, so they live here once and each protocol's test
//! file supplies only what differs: its `base_stack`, its numbers, and a request that its own
//! router turns into an event.
//!
//! # Three sockets, and each one fails a different way if a bound is removed
//!
//! [`assert_read_deadlines`] opens three connections to one server whose only routing rule is
//! `*` → `manual`, so any event it raises parks for a human (300s) instead of reaching a model:
//!
//! | socket | what it does | what must happen | which bound |
//! |---|---|---|---|
//! | `SILENT` | connects, sends nothing | closed soon after `FIRST_BYTE_READ_TIMEOUT` | the `TcpStream::peek` before hyper |
//! | `PARTIAL` | sends a request line and stalls | closed soon after `IDLE_BETWEEN_REQUESTS_TIMEOUT` | the `watch_idle` watchdog |
//! | `PARKED` | sends a whole request, which parks | **still open** past the idle bound | the `ConnectionActivity::busy` guard |
//!
//! `PARKED` is the assertion that stops the other two from being satisfiable by a server that
//! hangs up on everybody, and it is the one that matters most in this codebase: a `manual` rule
//! waits 300 seconds for a person (`src/state/intercepts.rs`), and a server that closed the
//! connection it was in the middle of answering would be worse than one with no bound at all.
//! `PARTIAL` is also the slowloris case — a peer that says just enough to get past the
//! first-byte check and then stops.
//!
//! # Verified by removal
//!
//! Each bound was taken out and the matching socket's assertion observed to fail before the
//! code was committed; the per-protocol test files record which one they saw.

#![allow(dead_code)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::common::{wait_for_server_listening, E2EResult};
use super::netget::NetGetConfig;
use super::server::{start_netget_server, NetGetServer};

/// `FIRST_BYTE_READ_TIMEOUT` as every one of the ten declares it.
///
/// Deliberately duplicated rather than imported: the constants are private to each protocol's
/// `mod.rs`, and a copy that has to be kept in step by hand is the point — if one of them moves,
/// this should be re-read rather than silently following.
pub const FIRST_BYTE_SECS: u64 = 30;

/// One protocol's declared bounds, plus the least it takes to make it raise an event.
pub struct HttpBoundsCase {
    /// The `base_stack` string `open_server` accepts for this protocol.
    pub base_stack: &'static str,
    /// A token unique enough for the mock to match this test's instruction on.
    pub label: &'static str,
    /// `MAX_CONNECTIONS`, exactly as the protocol's `mod.rs` declares it.
    pub max_connections: usize,
    /// `IDLE_BETWEEN_REQUESTS_TIMEOUT`, in seconds.
    pub idle_secs: u64,
    /// Extra `startup_params`, for the protocols that need one to start at all.
    pub startup_params: Option<serde_json::Value>,
    /// A complete HTTP request this protocol's router turns into an event, so that a `manual`
    /// rule parks it. A request that only 404s raises nothing and would leave the connection
    /// idle, which is a different test.
    pub event_request: &'static [u8],
}

/// A netget server for this protocol whose only rule parks every event for a human.
///
/// `manual` rather than a static handler because the connection must be *busy* for minutes
/// without any model call: that is what separates "this connection is working" from "this
/// connection is silent", and it is the distinction the idle watchdog exists to make.
fn config(case: &HttpBoundsCase) -> NetGetConfig {
    let label = case.label;
    let base_stack = case.base_stack;
    let params = case.startup_params.clone();
    NetGetConfig::new(format!("Start the {label} connection-bounds server.")).with_mock(
        move |mock| {
            let mut action = serde_json::json!({
                "type": "open_server",
                "port": 0,
                "base_stack": base_stack,
                "instruction": "Connection bounds fixture",
                "event_handlers": [{
                    "event_pattern": "*",
                    "handler": { "type": "manual", "timeout_secs": 300 }
                }]
            });
            if let Some(extra) = params.clone() {
                action["startup_params"] = extra;
            }
            mock.on_instruction_containing(label)
                .respond_with_actions(serde_json::json!([action]))
                .expect_calls(1)
                .and()
        },
    )
}

/// Has the far end closed this socket within `secs`?
///
/// `Ok(0)` is a clean close and an `Err` is a reset; both are the server hanging up. Data is
/// not a close, and neither is the read timing out.
async fn closed_by_peer(stream: &mut TcpStream, secs: u64) -> bool {
    let mut buf = [0u8; 256];
    loop {
        match tokio::time::timeout(Duration::from_secs(secs), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            // The server said something (a 503, an error body). Keep reading: what this
            // function answers is whether the connection *ends*.
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

/// Wait up to `secs` for the far end to close, reporting how long it took.
async fn wait_for_close(stream: &mut TcpStream, secs: u64) -> Option<Duration> {
    let started = std::time::Instant::now();
    if closed_by_peer(stream, secs).await {
        Some(started.elapsed())
    } else {
        None
    }
}

/// The connection cap refuses the peer past the limit in HTTP's own vocabulary, and gives the
/// slot back when a connection ends.
///
/// The peers that fill the cap send a request line and stall rather than staying silent, so they
/// sit under the *idle* bound (a minute or more) instead of the 30-second first-byte one and
/// cannot expire underneath the assertions below.
pub async fn assert_connection_cap(case: &HttpBoundsCase) -> E2EResult<()> {
    let mut server = start_netget_server(config(case)).await?;
    wait_for_server_listening(&server, Duration::from_secs(60)).await?;
    let baseline = server.llm_call_count().await;

    let mut held: Vec<TcpStream> = Vec::with_capacity(case.max_connections);
    for i in 0..case.max_connections {
        let mut socket = TcpStream::connect(("127.0.0.1", server.port))
            .await
            .map_err(|e| {
                format!(
                    "connection {i} of {} was refused: {e}",
                    case.max_connections
                )
            })?;
        socket.write_all(b"GET / HTTP/1.1\r\n").await?;
        socket.flush().await?;
        held.push(socket);
    }

    // One more. `connect` itself still succeeds — the listen backlog completes the handshake
    // before the accept loop ever sees it — which is precisely why the refusal has to be
    // observable as bytes followed by a close.
    let mut over = TcpStream::connect(("127.0.0.1", server.port)).await?;
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), over.read_to_end(&mut seen))
        .await
        .map_err(|_| {
            format!(
                "the connection past the cap of {} was neither answered nor closed. Unbounded, \
                 it would simply have been served.",
                case.max_connections
            )
        })?
        .map_err(|e| format!("reading the refusal failed: {e}"))?;

    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.starts_with("HTTP/1.1 503 Service Unavailable"),
        "a refused peer must be told so in HTTP's own vocabulary, not dropped in silence; got \
         {text:?}"
    );
    assert!(
        text.contains("Retry-After:"),
        "the refusal must say the server is busy rather than broken, so a client backs off \
         instead of recording a permanent fault; got {text:?}"
    );
    server
        .wait_for_log("decision=fail_closed_connection_cap", 30)
        .await?;

    // Give a slot back and take it again. Two failures hide here: a permit released early (the
    // cap does nothing) and a permit never released (the server wedges shut after MAX peers have
    // *ever* connected, which is worse than no cap at all). A readmitted peer is one that is not
    // hung up on; this also doubles as the control that the cap did not take the listener with it.
    drop(held.pop().expect("the cap is not zero"));
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(mut candidate) = TcpStream::connect(("127.0.0.1", server.port)).await {
            candidate.write_all(b"GET / HTTP/1.1\r\n").await.ok();
            if !closed_by_peer(&mut candidate, 2).await {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(
                "the slot freed by a closed connection never came back: the connection \
                        permit is not being released"
                    .into(),
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    assert_eq!(
        server.llm_call_count().await,
        baseline,
        "none of the {} connections completed a request, so none of them may cost a model call",
        case.max_connections + 2
    );

    drop(held);
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

/// The two read deadlines, and the guarantee that neither of them can close a connection whose
/// answer is still being composed.
///
/// See this module's header for the three sockets and what each one proves.
pub async fn assert_read_deadlines(case: &HttpBoundsCase) -> E2EResult<()> {
    let mut server = start_netget_server(config(case)).await?;
    wait_for_server_listening(&server, Duration::from_secs(60)).await?;

    let started = std::time::Instant::now();
    let mut silent = TcpStream::connect(("127.0.0.1", server.port)).await?;

    let mut partial = TcpStream::connect(("127.0.0.1", server.port)).await?;
    partial.write_all(b"GET / HTTP/1.1\r\n").await?;
    partial.flush().await?;

    let mut parked = TcpStream::connect(("127.0.0.1", server.port)).await?;
    parked.write_all(case.event_request).await?;
    parked.flush().await?;

    // The whole point of PARKED is that it is busy, so prove it really parked rather than
    // assuming it: a request that only 404s raises no event and would be idle, not busy.
    server.wait_for_log("parked as intercept", 60).await?;

    // Well inside the first-byte bound: nothing has expired yet. Without this the two closes
    // below would also be satisfied by a server that hangs up on every connection immediately.
    tokio::time::sleep(Duration::from_secs(10).saturating_sub(started.elapsed())).await;
    for (label, socket) in [
        ("the silent peer", &mut silent),
        ("the stalled peer", &mut partial),
        ("the parked peer", &mut parked),
    ] {
        assert!(
            !closed_by_peer(socket, 1).await,
            "{label} was closed after ~10s, well inside every declared bound — the deadlines are \
             firing on connections they should not touch"
        );
    }

    // SILENT: nothing on the wire at all, so the `peek` before hyper is what ends it.
    let elapsed = wait_for_close(&mut silent, FIRST_BYTE_SECS + 30)
        .await
        .map(|_| started.elapsed());
    let elapsed = elapsed.ok_or_else(|| {
        format!(
            "a peer that connected and sent nothing was still holding a socket, a task and an \
             AppState entry {}s later. That is the free denial of service the first-byte bound \
             exists to close.",
            started.elapsed().as_secs()
        )
    })?;
    assert!(
        elapsed.as_secs() >= FIRST_BYTE_SECS - 5,
        "the silent peer was closed after {}s, before its {FIRST_BYTE_SECS}s bound — a client on \
         a slow link would be cut off too",
        elapsed.as_secs()
    );

    // PARTIAL: one request line and then silence — past the first-byte check, so only the idle
    // watchdog can end it. This is slowloris.
    let idle_deadline = case.idle_secs + 40;
    let elapsed = wait_for_close(
        &mut partial,
        idle_deadline.saturating_sub(started.elapsed().as_secs()),
    )
    .await
    .map(|_| started.elapsed())
    .ok_or_else(|| {
        format!(
            "a peer that sent a request line and then stalled was still connected {}s later, \
                 past its {}s idle bound",
            started.elapsed().as_secs(),
            case.idle_secs
        )
    })?;
    assert!(
        elapsed.as_secs() + 5 >= case.idle_secs,
        "the stalled peer was closed after {}s, before its {}s idle bound",
        elapsed.as_secs(),
        case.idle_secs
    );

    // PARKED: the request is waiting for a human, which is work in flight and must never read as
    // silence. The manual timeout is 300s, so at this point it is still parked.
    assert!(
        !closed_by_peer(&mut parked, 2).await,
        "the connection whose request is parked for a human was closed after {}s, past the {}s \
         idle bound — the watchdog is measuring wall-clock silence instead of reading \
         ConnectionActivity, so netget hangs up on the peer it is in the middle of answering",
        started.elapsed().as_secs(),
        case.idle_secs
    );

    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

/// Both halves in one call, for a protocol whose test file has nothing else to say.
pub async fn assert_all_bounds(case: &HttpBoundsCase) -> E2EResult<()> {
    assert_connection_cap(case).await?;
    assert_read_deadlines(case).await
}

/// Unused outside the two entry points above; named so the compiler keeps the import honest.
#[allow(unused)]
fn _assert_server_type(_: &NetGetServer) {}
