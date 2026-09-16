//! The dashboard's "message this peer" / "disconnect this peer" path on a Cassandra/CQL
//! connection: `AppState::send_to_peer` injects an action into one live connection.
//!
//! What this proves, and what it deliberately does not:
//!
//! - The peer handle exists **before the peer has said anything**. CQL is client-speaks-first,
//!   so a `*` manual rule parks the connection's very first event; the operator being asked to
//!   answer it must be able to reach the connection while it waits.
//! - `[ disconnect this peer ]` really hangs up: the injected `close_connection` half-closes
//!   the write side and the client socket reads EOF.
//! - An injected **wire verb** is reported as `Executed`, not `Sent`. Every Cassandra action
//!   returns `ActionResult::Custom` rather than `ActionResult::Output`, because a CQL response
//!   frame carries the *stream id of the request it answers* and an injection has no request.
//!   `peer_support` has no bytes to write, and says so. This asserts that honest outcome
//!   rather than pretending a frame went out.
//! - The session's own path still works alongside the injection, through the same shared write
//!   half, and its bytes are counted in both directions.
//!
//! Zero LLM calls: the server's own answer comes from a `*` static handler and the injected
//! actions never touch the model. Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features cassandra --test server -- cassandra::peer_inject --test-threads=100

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Native protocol v4 request header byte.
const CQL_V4_REQUEST: u8 = 0x04;
/// OPTIONS opcode (v4 §4.1.2).
const CQL_OPCODE_OPTIONS: u8 = 0x05;
/// SUPPORTED opcode (v4 §4.2.4).
const CQL_OPCODE_SUPPORTED: u8 = 0x06;

async fn new_state() -> AppState {
    // Port 1 is never listening, so a stray model call would fail loudly rather than reach
    // anything. Nothing here should produce one.
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..100 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("Cassandra server #{} never bound a port", id.as_u32());
}

/// The first connection that has a peer handle registered.
async fn wait_for_peer_handle(state: &AppState, id: ServerId) -> u32 {
    for _ in 0..100 {
        if let Some(s) = state.get_server(id).await {
            for conn in s.connections.values() {
                if state.has_peer_handle(id, conn.id.as_u32()).await {
                    return conn.id.as_u32();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "Cassandra server #{} never registered a peer handle",
        id.as_u32()
    );
}

/// A bare OPTIONS frame: 9-byte header, empty body.
fn options_frame(stream_id: i16) -> Vec<u8> {
    let mut frame = vec![CQL_V4_REQUEST, 0x00];
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.push(CQL_OPCODE_OPTIONS);
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame
}

/// Read one whole CQL frame (header + declared body).
async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 9];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut header))
        .await
        .expect("a CQL frame header within 10s")
        .expect("read CQL frame header");
    let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    assert!(len < 1_000_000, "implausible CQL body length {len}");
    let mut body = vec![0u8; len];
    if len > 0 {
        tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut body))
            .await
            .expect("a CQL frame body within 10s")
            .expect("read CQL frame body");
    }
    let mut frame = header.to_vec();
    frame.extend_from_slice(&body);
    frame
}

#[tokio::test]
async fn injected_cassandra_action_is_executed_and_close_sends_eof() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let server_id = ServerForm {
        protocol: "cassandra".to_string(),
        port: Some(0),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "*",
            "handler": {
                "type": "static",
                "actions": [ { "type": "cassandra_supported", "options": {} } ]
            }
        })]),
        ..Default::default()
    }
    .create(&state, tx.clone())
    .await
    .expect("create cassandra server");
    let port = wait_for_port(&state, server_id).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    // A CQL server says nothing until the driver speaks, so the handle must exist before any
    // traffic at all. Nothing has been written on this socket yet.
    let conn = wait_for_peer_handle(&state, server_id).await;

    // A wire verb, injected from outside the connection task. Cassandra's actions are
    // stream-id-bound `Custom` results, so this is executed and reported — it cannot be
    // framed, because no request is being answered.
    let outcome = state
        .send_to_peer(
            server_id,
            conn,
            serde_json::json!({"type": "cassandra_ready"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_to_peer");
    match &outcome {
        ClientSendOutcome::Executed { detail } => assert!(
            detail.contains("cassandra_ready"),
            "the outcome should name the action that ran, got {detail:?}"
        ),
        other => panic!("expected Executed (no bytes can be framed), got {other:?}"),
    }

    // The protocol's own path still works over the same shared write half.
    stream
        .write_all(&options_frame(7))
        .await
        .expect("write OPTIONS");
    let frame = read_frame(&mut stream).await;
    assert_eq!(
        frame[4], CQL_OPCODE_SUPPORTED,
        "OPTIONS must be answered with SUPPORTED, got opcode 0x{:02X}",
        frame[4]
    );
    assert_eq!(
        i16::from_be_bytes([frame[2], frame[3]]),
        7,
        "the reply must carry the request's stream id"
    );

    // Counted in both directions. Nothing updated these before the peer handle went in.
    let server = state.get_server(server_id).await.expect("server");
    let conn_state = server
        .connections
        .values()
        .find(|c| c.id.as_u32() == conn)
        .expect("connection tracked");
    assert_eq!(
        conn_state.bytes_received, 9,
        "the 9-byte OPTIONS frame must be counted"
    );
    assert_eq!(
        conn_state.bytes_sent,
        frame.len() as u64,
        "the SUPPORTED frame must be counted"
    );

    // "disconnect this peer": half-close from outside, the socket reads EOF.
    let outcome = state
        .send_to_peer(
            server_id,
            conn,
            serde_json::json!({"type": "close_connection"}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_to_peer close");
    assert!(
        matches!(outcome, ClientSendOutcome::Disconnected),
        "expected Disconnected, got {outcome:?}"
    );

    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("EOF within 5s")
        .expect("read after close");
    assert_eq!(n, 0, "expected EOF after close_connection");

    // The handle goes away with the connection.
    for _ in 0..100 {
        if !state.has_peer_handle(server_id, conn).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("peer handle still registered after the connection closed");
}
