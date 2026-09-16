//! The dashboard's "message this peer" / "disconnect this peer" path on a Kafka broker
//! connection: `AppState::send_to_peer` injects an action into one live connection.
//!
//! What this proves, and what it deliberately does not:
//!
//! - The peer handle exists **before the peer has said anything**. Kafka is
//!   client-speaks-first, so a `*` manual rule parks the connection's very first request; the
//!   operator being asked to answer it must be able to reach the connection while it waits.
//! - `[ disconnect this peer ]` really hangs up: the injected `close_connection` half-closes
//!   the write side and the client socket reads EOF.
//! - An injected **wire verb** is reported as `Executed`, not `Sent`. Every Kafka action
//!   returns `ActionResult::Custom` rather than `ActionResult::Output`, because a Kafka
//!   response is `(size)(correlation_id)(body)` and the correlation id belongs to the request
//!   being answered — an injection answers none. `peer_support` has no bytes to write, and
//!   says so. This asserts that honest outcome rather than pretending a frame went out.
//! - The broker's own path still works alongside the injection, through the same shared write
//!   half, and its bytes are counted in both directions.
//!
//! Zero LLM calls: ApiVersions is answered by Rust with no event at all, and the `*` static
//! handler covers anything else. Loopback only.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features kafka --test server -- kafka::peer_inject --test-threads=100

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::server::kafka::kafka_protocol::messages::{
    ApiKey, ApiVersionsResponse, ResponseHeader,
};
use netget::server::kafka::kafka_protocol::protocol::Decodable;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

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
    panic!("Kafka broker #{} never bound a port", id.as_u32());
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
        "Kafka broker #{} never registered a peer handle",
        id.as_u32()
    );
}

/// An ApiVersions v0 request: request header v1 (api key, api version, correlation id, a null
/// client id) and an empty body. Hand-rolled rather than encoded, so the exact byte count this
/// test asserts on is visible here.
fn api_versions_v0_request(correlation_id: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(ApiKey::ApiVersions as i16).to_be_bytes());
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&correlation_id.to_be_bytes());
    body.extend_from_slice(&(-1i16).to_be_bytes()); // client_id: null
    let mut frame = (body.len() as i32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}

/// Read one length-prefixed Kafka response frame; returns the body without the size prefix.
async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut size = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut size))
        .await
        .expect("a Kafka size prefix within 10s")
        .expect("read Kafka size prefix");
    let n = i32::from_be_bytes(size);
    assert!(n > 0 && n < 10_000_000, "implausible response size {n}");
    let mut buf = vec![0u8; n as usize];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
        .await
        .expect("a Kafka response body within 10s")
        .expect("read Kafka response body");
    buf
}

#[tokio::test]
async fn injected_kafka_action_is_executed_and_close_sends_eof() {
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();

    let server_id = ServerForm {
        protocol: "kafka".to_string(),
        port: Some(0),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "*",
            "handler": { "type": "static", "actions": [] }
        })]),
        ..Default::default()
    }
    .create(&state, tx.clone())
    .await
    .expect("create kafka broker");
    let port = wait_for_port(&state, server_id).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .expect("connect");

    // A broker says nothing until the client speaks, so the handle must exist before any
    // traffic at all. Nothing has been written on this socket yet.
    let conn = wait_for_peer_handle(&state, server_id).await;

    // A wire verb, injected from outside the connection task. Kafka's actions are
    // correlation-id-bound `Custom` results, so this is executed and reported — it cannot be
    // framed, because no request is being answered.
    let outcome = state
        .send_to_peer(
            server_id,
            conn,
            serde_json::json!({"type": "metadata_response", "topics": []}),
            Duration::from_secs(5),
        )
        .await
        .expect("send_to_peer");
    match &outcome {
        ClientSendOutcome::Executed { detail } => assert!(
            detail.contains("metadata_response"),
            "the outcome should name the action that ran, got {detail:?}"
        ),
        other => panic!("expected Executed (no bytes can be framed), got {other:?}"),
    }

    // The broker's own path still works over the same shared write half. ApiVersions is
    // answered by Rust, so this stays at zero model calls.
    let request = api_versions_v0_request(4242);
    stream
        .write_all(&request)
        .await
        .expect("write ApiVersions request");
    let response = read_frame(&mut stream).await;

    let mut cursor = std::io::Cursor::new(&response[..]);
    let header =
        ResponseHeader::decode(&mut cursor, ApiKey::ApiVersions.response_header_version(0))
            .expect("response header must decode");
    assert_eq!(
        header.correlation_id, 4242,
        "the broker must echo the correlation id"
    );
    let decoded = ApiVersionsResponse::decode(&mut cursor, 0).expect("body must decode");
    assert_eq!(decoded.error_code, 0, "ApiVersions v0 is supported");

    // Counted in both directions.
    let server = state.get_server(server_id).await.expect("server");
    let conn_state = server
        .connections
        .values()
        .find(|c| c.id.as_u32() == conn)
        .expect("connection tracked");
    assert_eq!(
        conn_state.bytes_received,
        request.len() as u64,
        "the request, size prefix included, must be counted"
    );
    assert_eq!(
        conn_state.bytes_sent,
        (response.len() + 4) as u64,
        "the response, size prefix included, must be counted"
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
