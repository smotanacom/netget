//! SMB's declared `max_inbound_bytes` — `MAX_WRITE_SIZE`, the negotiated `MaxWriteSize` —
//! driven from the wire.
//!
//! This server speaks raw SMB2 with no transport length prefix, so every read is a fixed-size
//! header or body except one: a WRITE's `Length`, a peer-chosen u32 that sizes the buffer its
//! data is read into. So the bound is tested with WRITE, after a NEGOTIATE and a
//! SESSION_SETUP the model approves:
//!
//! 1. **`NEGOTIATE` advertises the bound** as `MaxWriteSize`, so a client never sends more.
//! 2. **`bound` is accepted**: a WRITE of exactly `MAX_WRITE_SIZE` bytes is read and handed to
//!    the model as a write.
//! 3. **`bound + 1` is refused before the model**: a WRITE header declaring
//!    `MAX_WRITE_SIZE + 1` is answered `STATUS_INVALID_PARAMETER` (MS-SMB2 3.3.5.13) and the
//!    connection closes, with zero model calls.
//! 4. **The server still serves**: a fresh connection's SESSION_SETUP reaches the model.
//!
//! **Verified by removal.** With the `length > MAX_WRITE_SIZE` check removed, assertion 3
//! fails: the server allocates for the declared payload and waits for it instead of answering.
//! With `close_after_reply` never set, the refusal arrives but the connection stays open,
//! waiting to read the unread payload as the next header, and the close assertion fails.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features smb --test server -- smb::inbound_limit --test-threads=100

#![cfg(feature = "smb")]

use std::time::Duration;

use netget::server::smb::MAX_WRITE_SIZE;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::helpers::inbound_limit::InboundLimitServer;
use crate::helpers::mock_builder::MockLlmBuilder;

const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;

fn header(command: u16, message_id: u64, session_id: u64) -> Vec<u8> {
    let mut h = Vec::with_capacity(64);
    h.extend_from_slice(b"\xFESMB");
    h.extend_from_slice(&[64, 0]);
    h.extend_from_slice(&[0; 2]);
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&command.to_le_bytes());
    h.extend_from_slice(&[1, 0]);
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&message_id.to_le_bytes());
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&[0; 4]);
    h.extend_from_slice(&session_id.to_le_bytes());
    h.extend_from_slice(&[0; 16]);
    h
}

fn negotiate() -> Vec<u8> {
    let mut p = header(0x0000, 0, 0);
    p.extend_from_slice(&[36, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    p.extend_from_slice(&[0; 16]);
    p.extend_from_slice(&[0; 8]);
    p.extend_from_slice(&[0x10, 0x02]);
    p
}

fn session_setup() -> Vec<u8> {
    let mut p = header(0x0001, 1, 0);
    p.extend_from_slice(&[25, 0, 0, 0]);
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(&[0; 4]);
    p.extend_from_slice(&[88, 0, 0, 0]);
    p.extend_from_slice(&[0; 8]);
    p
}

/// A WRITE header and 49-byte body declaring `len` bytes of data (MS-SMB2 2.2.21).
fn write_header(len: u32) -> Vec<u8> {
    let mut p = header(0x0009, 2, 1);
    p.extend_from_slice(&[49, 0]);
    p.extend_from_slice(&112u16.to_le_bytes());
    p.extend_from_slice(&len.to_le_bytes());
    p.extend_from_slice(&0u64.to_le_bytes());
    p.extend_from_slice(&[0xAB; 16]);
    p.extend_from_slice(&[0; 4 + 4 + 2 + 2 + 4]);
    p.push(0);
    assert_eq!(p.len(), 64 + 49);
    p
}

/// Read one response (the server writes one per request); `None` on EOF.
async fn read_response(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 65536];
    match tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => None,
        Ok(Ok(n)) => {
            buf.truncate(n);
            Some(buf)
        }
    }
}

fn status(resp: &[u8]) -> u32 {
    u32::from_le_bytes([resp[8], resp[9], resp[10], resp[11]])
}

/// NEGOTIATE then SESSION_SETUP; returns the NEGOTIATE response.
async fn handshake(stream: &mut TcpStream) -> Vec<u8> {
    stream.write_all(&negotiate()).await.unwrap();
    let neg = read_response(stream).await.expect("NEGOTIATE response");
    stream.write_all(&session_setup()).await.unwrap();
    let setup = read_response(stream).await.expect("SESSION_SETUP response");
    assert_eq!(
        status(&setup),
        0,
        "SESSION_SETUP must succeed for the WRITE to be reached"
    );
    neg
}

async fn start() -> InboundLimitServer {
    InboundLimitServer::try_start_with_mock(
        "smb",
        None,
        MockLlmBuilder::new()
            .on_event("smb_operation")
            .and_event_data_contains("operation", "session_setup")
            .respond_with_actions(serde_json::json!([
                {"type": "smb_auth_success", "username": "guest"}
            ]))
            .expect_at_least(0)
            .and()
            .on_any()
            .respond_with_actions(serde_json::json!([]))
            .expect_at_least(0)
            .build(),
    )
    .await
    .unwrap_or_else(|e| panic!("start smb: {e}"))
}

#[tokio::test]
async fn a_write_over_max_write_size_is_refused_before_the_model_and_the_server_keeps_serving() {
    let server = start().await;
    let bound = MAX_WRITE_SIZE as usize;

    // 1 + 2. The bound is advertised, and a WRITE of exactly that size reaches the model.
    let mut s = server.connect().await;
    let neg = handshake(&mut s).await;
    // NEGOTIATE response body: MaxTransactSize, MaxReadSize, MaxWriteSize at body offsets
    // 28, 32 and 36 (MS-SMB2 2.2.4).
    let advertised = u32::from_le_bytes([neg[64 + 36], neg[64 + 37], neg[64 + 38], neg[64 + 39]]);
    assert_eq!(
        advertised, MAX_WRITE_SIZE,
        "NEGOTIATE must advertise MaxWriteSize equal to the bound the WRITE arm enforces"
    );
    let before = server.settled_calls().await;
    let mut msg = write_header(MAX_WRITE_SIZE);
    msg.resize(msg.len() + bound, b'w');
    s.write_all(&msg).await.unwrap();
    let after = server.wait_for_calls_above(before, 30).await;
    assert!(
        after > before,
        "a WRITE of exactly MaxWriteSize ({bound}) bytes never reached the model"
    );
    let reply = read_response(&mut s)
        .await
        .expect("a reply to the at-bound WRITE");
    assert_ne!(
        status(&reply),
        STATUS_INVALID_PARAMETER,
        "a WRITE of exactly MaxWriteSize was refused as too large"
    );
    drop(s);

    // 3. One past the bound: STATUS_INVALID_PARAMETER, then close, and no model call.
    let mut s = server.connect().await;
    handshake(&mut s).await;
    let before = server.settled_calls().await;
    s.write_all(&write_header(MAX_WRITE_SIZE + 1))
        .await
        .unwrap();
    let reply = read_response(&mut s)
        .await
        .expect("an over-size WRITE must be answered, not left waiting for its payload");
    assert_eq!(
        status(&reply),
        STATUS_INVALID_PARAMETER,
        "a WRITE longer than MaxWriteSize must fail with STATUS_INVALID_PARAMETER (MS-SMB2 3.3.5.13)"
    );
    let mut sink = [0u8; 64];
    let closed = matches!(
        tokio::time::timeout(Duration::from_secs(20), s.read(&mut sink)).await,
        Ok(Ok(0)) | Ok(Err(_))
    );
    assert!(
        closed,
        "after refusing an over-size WRITE the server must close: its payload is unread, and \
         reading on would parse it as the next SMB2 header"
    );
    let after = server.settled_calls().await;
    assert_eq!(
        after - before,
        0,
        "a WRITE declaring {} bytes against MaxWriteSize {bound} cost {} model call(s)",
        bound + 1,
        after - before
    );

    // 4. A fresh connection is still served.
    let mut s = server.connect().await;
    let before = server.settled_calls().await;
    handshake(&mut s).await;
    let after = server.settled_calls().await;
    assert!(
        after > before,
        "after refusing an over-size WRITE, a fresh SESSION_SETUP never reached the model"
    );

    server.stop().await;
}
