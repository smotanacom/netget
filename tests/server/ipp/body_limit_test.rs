//! An oversized request body must be refused before it is buffered, and before a model call.
//!
//! `hyper::body::Incoming` has no default limit. `req.into_body().collect()` therefore buffered
//! whatever the peer chose to send, from an unauthenticated `POST` to a port that IPP clients
//! try by default. This server keeps no job store and never looks past the 8-byte header, so
//! there is nothing a large body could be needed for.
//!
//! The second half is the part worth a test of its own: the old code turned a body read failure
//! into `Bytes::new()`, which `parse_ipp_header` reports as the operation `Empty`. The model
//! would then have been asked to answer a request nobody sent — the truncated-body-looks-like-a
//! -complete-one shape `http_common::BodyTooLarge` exists to avoid. So this asserts **zero**
//! `ipp_request_received` calls, not merely that the status is 413.
//!
//! The refusal is expressed twice, deliberately. HTTP 413 is what a generic client sees; the
//! body carries `client-error-request-entity-too-large` (0x0408) so an IPP client that only
//! reads the IPP layer learns the same thing rather than reporting a truncated response.
//!
//! **Expressing it twice is worth nothing if the peer never reads it**, which is what the third
//! test here is about. A server that answers 413 and closes while the peer is still writing
//! sends `RST` — closing a socket with unread data in the receive queue discards the response
//! bytes already written along with it — so the peer's `write` fails with `ECONNRESET` and it
//! sees a connection error where a refusal was sent. That was this suite's own intermittent
//! failure: the first test raced, passing whenever the 9 MiB write happened to fit in the
//! socket buffers and failing when it did not. The server now drains, boundedly, before
//! answering (`LINGER_DRAIN_BYTES`).

#![cfg(all(test, feature = "ipp"))]

use super::super::super::helpers::{self, E2EResult, NetGetConfig};
use std::time::Duration;

/// Larger than `MAX_IPP_BODY_BYTES` (8 MiB) in `src/server/ipp/mod.rs`.
const OVERSIZED: usize = 9 * 1024 * 1024;

/// Past the cap by more than any socket buffer will absorb, and still inside the 8 MiB the
/// server will drain. Sized so the peer is **guaranteed** to be mid-write when the server
/// decides: without the drain this fails every run rather than intermittently.
const OVERSIZED_MID_WRITE: usize = 15 * 1024 * 1024;

/// `client-error-request-entity-too-large`, RFC 8011 appendix B.
const STATUS_ENTITY_TOO_LARGE: u16 = 0x0408;

#[tokio::test]
async fn an_oversized_body_is_refused_without_reaching_the_model() -> E2EResult<()> {
    let config =
        NetGetConfig::new("Open IPP on port {AVAILABLE_PORT} as a printer.").with_mock(|mock| {
            mock.on_instruction_containing("Open IPP")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IPP",
                    "instruction": "IPP printer"
                }]))
                .expect_calls(1)
                .and()
                // The load-bearing expectation. If the cap ever stops firing, the oversized
                // request reaches the model and this count goes to 1.
                .on_event("ipp_request_received")
                .respond_with_actions(serde_json::json!([{
                    "type": "ipp_response",
                    "ipp_status": "successful-ok"
                }]))
                .expect_calls(0)
                .and()
        });

    let server = helpers::start_netget_server(config).await?;
    wait_until_listening(server.port).await?;

    // A well-formed 8-byte header so nothing else can be blamed for the refusal, then bulk.
    let mut body = vec![0x01, 0x01, 0x00, 0x02, 0xAA, 0xBB, 0xCC, 0xDD];
    body.resize(OVERSIZED, 0x00);

    let response = tokio::time::timeout(
        Duration::from_secs(60),
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/printers/netget", server.port))
            .header("Content-Type", "application/ipp")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| "IPP server never answered the oversized request")??;

    assert_eq!(
        response.status(),
        413,
        "an oversized body must be refused with 413, not buffered and answered"
    );

    // The IPP layer must carry the same refusal: a client that reads only the IPP status must
    // not see a truncated or absent message.
    let reply = response.bytes().await?;
    assert!(
        reply.len() >= 9,
        "the refusal must still be a well-formed IPP message, got {} bytes",
        reply.len()
    );
    assert_eq!(
        u16::from_be_bytes([reply[2], reply[3]]),
        STATUS_ENTITY_TOO_LARGE,
        "expected client-error-request-entity-too-large, got 0x{:04x}",
        u16::from_be_bytes([reply[2], reply[3]])
    );
    assert_eq!(
        reply[reply.len() - 1],
        0x03,
        "the message must end with the end-of-attributes tag"
    );

    server.wait_for_mocks(5).await;
    server.verify_mocks().await?;
    Ok(())
}

/// A body under the cap must still be served, so the refusal is not a refusal of everything.
#[tokio::test]
async fn a_body_under_the_cap_is_still_answered() -> E2EResult<()> {
    let config =
        NetGetConfig::new("Open IPP on port {AVAILABLE_PORT} as a printer.").with_mock(|mock| {
            mock.on_instruction_containing("Open IPP")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IPP",
                    "instruction": "IPP printer"
                }]))
                .expect_calls(1)
                .and()
                .on_event("ipp_request_received")
                .respond_with_actions(serde_json::json!([{
                    "type": "ipp_job_attributes",
                    "attributes": {"job-id": 7, "job-state": "completed"}
                }]))
                .expect_calls(1)
                .and()
        });

    let server = helpers::start_netget_server(config).await?;
    wait_until_listening(server.port).await?;

    // 1 MiB: a plausible Print-Job document, comfortably inside the cap.
    let mut body = vec![0x01, 0x01, 0x00, 0x02, 0x00, 0x00, 0x02, 0xA7];
    body.push(0x03); // end-of-attributes, then document data
    body.resize(1024 * 1024, b'A');

    let response = tokio::time::timeout(
        Duration::from_secs(60),
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/printers/netget", server.port))
            .header("Content-Type", "application/ipp")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| "IPP server never answered a request inside the cap")??;

    assert_eq!(response.status(), 200, "a 1 MiB Print-Job must be served");
    let reply = response.bytes().await?;
    assert_eq!(
        u32::from_be_bytes([reply[4], reply[5], reply[6], reply[7]]),
        0x0000_02A7,
        "the request-id must still be echoed"
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    Ok(())
}

/// The refusal must reach a peer that is still writing when the server decides.
///
/// This is the one the fix is for. The server stops reading at 8 MiB, so with a 15 MiB body the
/// peer still has ~7 MiB to send — far more than any socket buffer holds — and is therefore
/// blocked in `write` when the 413 is produced. Answer-and-close there sends `RST`, the peer's
/// `write` fails with `ECONNRESET`, and both the HTTP status and the IPP status are lost. The
/// server drains the remainder (counting and discarding, never buffering) so the peer can
/// finish, then answers.
///
/// Both halves matter: the status assertions would pass on a server that had no limit at all if
/// `expect_calls(0)` were dropped, and `expect_calls(0)` would pass on a server that reset the
/// connection if the status assertions were dropped.
///
/// Written against a raw socket rather than `reqwest`, and that is the difference between a
/// test that reproduces the defect every run and one that reproduces it about half the time.
/// A hyper client polls the read side while it is still writing, so it can parse the 413 out of
/// its receive buffer *before* the `RST` arrives and lose the race in the test's favour —
/// measured at 5 failures in 8 runs with the drain disabled, which is exactly the intermittency
/// this suite was reported with. Writing the whole body first and only then reading is what an
/// IPP client with a document to send does anyway, and it makes the reset unmissable.
#[tokio::test]
async fn the_refusal_reaches_a_peer_that_is_still_writing() -> E2EResult<()> {
    let config =
        NetGetConfig::new("Open IPP on port {AVAILABLE_PORT} as a printer.").with_mock(|mock| {
            mock.on_instruction_containing("Open IPP")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IPP",
                    "instruction": "IPP printer"
                }]))
                .expect_calls(1)
                .and()
                .on_event("ipp_request_received")
                .respond_with_actions(serde_json::json!([{
                    "type": "ipp_response",
                    "ipp_status": "successful-ok"
                }]))
                .expect_calls(0)
                .and()
        });

    let server = helpers::start_netget_server(config).await?;
    wait_until_listening(server.port).await?;

    let mut body = vec![0x01, 0x01, 0x00, 0x02, 0xAA, 0xBB, 0xCC, 0xDD];
    body.resize(OVERSIZED_MID_WRITE, 0x00);

    let (status, reply) =
        tokio::time::timeout(Duration::from_secs(60), post_then_read(server.port, &body))
            .await
            .map_err(|_| "IPP server never answered the oversized request")?
            .map_err(|e| {
                format!(
            "the peer was reset instead of being answered ({e}); the server decided at the cap \
             and closed while {} MiB was still in flight",
            (OVERSIZED_MID_WRITE - 8 * 1024 * 1024) / (1024 * 1024)
        )
            })?;

    assert_eq!(status, 413, "the peer must receive the refusal");
    assert!(
        reply.len() >= 9,
        "the refusal must still be a well-formed IPP message, got {} bytes",
        reply.len()
    );
    assert_eq!(
        u16::from_be_bytes([reply[2], reply[3]]),
        STATUS_ENTITY_TOO_LARGE,
        "the IPP layer must carry the refusal too"
    );

    server.wait_for_mocks(5).await;
    server.verify_mocks().await?;
    Ok(())
}

/// Write a whole IPP POST, *then* read the reply, on a raw socket.
///
/// Deliberately sequential: a client that reads while it writes can win the race against an
/// `RST` and see a refusal that a client which finishes writing first never gets. Returns the
/// HTTP status and the response body, or the write error that means the server hung up.
async fn post_then_read(port: u16, body: &[u8]) -> Result<(u16, Vec<u8>), std::io::Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let head = format!(
        "POST /printers/netget HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Content-Type: application/ipp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("no header terminator in the reply"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other(format!("no status in {head:?}")))?;
    Ok((status, raw[split + 4..].to_vec()))
}

/// The drain is politeness, not an obligation — it has to be bounded, or a peer that keeps
/// writing holds a connection and a task for as long as it likes.
///
/// A body past `MAX_IPP_BODY_BYTES + LINGER_DRAIN_BYTES` (8 + 8 MiB) exceeds what the server
/// will read, so the peer gets the abrupt close it earned. Either outcome is acceptable here —
/// a 413 if the peer happened to finish, a transport error if it did not — but the request must
/// **end**, promptly, and the model must still never be asked. Without the bound this test does
/// not fail, it hangs: an unbounded drain reads for as long as the peer writes.
#[tokio::test]
async fn the_drain_is_bounded_so_a_peer_cannot_hold_the_connection() -> E2EResult<()> {
    let config =
        NetGetConfig::new("Open IPP on port {AVAILABLE_PORT} as a printer.").with_mock(|mock| {
            mock.on_instruction_containing("Open IPP")
                .respond_with_actions(serde_json::json!([{
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IPP",
                    "instruction": "IPP printer"
                }]))
                .expect_calls(1)
                .and()
                .on_event("ipp_request_received")
                .respond_with_actions(serde_json::json!([{
                    "type": "ipp_response",
                    "ipp_status": "successful-ok"
                }]))
                .expect_calls(0)
                .and()
        });

    let server = helpers::start_netget_server(config).await?;
    wait_until_listening(server.port).await?;

    // 8 MiB cap + 8 MiB drain + 8 MiB the server must refuse to read.
    let mut body = vec![0x01, 0x01, 0x00, 0x02, 0xAA, 0xBB, 0xCC, 0xDD];
    body.resize(24 * 1024 * 1024, 0x00);

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/printers/netget", server.port))
            .header("Content-Type", "application/ipp")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| "the server kept reading a body past its drain budget")?;

    if let Ok(response) = outcome {
        assert_eq!(
            response.status(),
            413,
            "if the peer was answered at all, it must have been refused"
        );
    }

    server.wait_for_mocks(5).await;
    server.verify_mocks().await?;
    Ok(())
}

/// Poll until the IPP server's TCP port accepts a connection.
///
/// `start_netget_server` returns when startup has been *parsed*, not when the socket is bound.
/// Every other test here posts a few dozen bytes and wins the race by accident; these post a
/// megabyte and nine, which takes long enough under `--test-threads=100` that losing it shows
/// up as the server "answering wrongly" rather than as a connection refused. Wait for the
/// condition instead of hoping.
async fn wait_until_listening(port: u16) -> E2EResult<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(_) => return Ok(()),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(format!("IPP server never bound port {port}: {e}").into())
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
}
