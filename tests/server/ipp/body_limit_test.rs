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

#![cfg(all(test, feature = "ipp"))]

use super::super::super::helpers::{self, E2EResult, NetGetConfig};
use std::time::Duration;

/// Larger than `MAX_IPP_BODY_BYTES` (8 MiB) in `src/server/ipp/mod.rs`.
const OVERSIZED: usize = 9 * 1024 * 1024;

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
