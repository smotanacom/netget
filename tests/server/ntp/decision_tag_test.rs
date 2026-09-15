//! Every NTP request leaves a `decision=` token in the log, and the token tells the truth
//! about what the client received.
//!
//! NTP is the awkward case in the `decision=` vocabulary and this test exists to pin the
//! awkwardness rather than paper over it. On backend failure the server does **not** deny:
//! it answers with the mechanical stratum-2 time response, which the client accepts as a
//! usable time sample and sets its clock from. An affirmative answer is not a fail-closed, so
//! the failure is tagged `decision=static_default_llm_error` and deliberately **not**
//! `decision=fail_closed_*`. Tagging it `fail_closed` would make `grep decision=fail_closed`
//! - the one diagnostic this repo teaches - report a denial that never happened.
//!
//! Both halves are asserted together, because either alone is meaningless: the log carries
//! the tag, and the wire carries the affirmative stratum-2 answer that makes the tag the
//! honest one.
//!
//! See `src/server/ntp/CLAUDE.md`, "Failure behaviour".

#![cfg(feature = "ntp")]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use std::time::Duration;
use tokio::net::UdpSocket;

/// The client's transmit timestamp, which must come back as the reply's origin timestamp or
/// a real client discards the reply as unrelated to its request.
const CLIENT_TRANSMIT: u64 = 0xD4C3_B2A1_1234_5678;

#[tokio::test]
async fn test_ntp_llm_failure_is_tagged_static_default_not_fail_closed() -> E2EResult<()> {
    // A non-empty instruction is what opts the server into LLM control; without it NTP
    // answers statically and never calls the model at all.
    let prompt = "listen on port {AVAILABLE_PORT} via ntp. Report the time as stratum 1";

    let config = NetGetConfig::new_no_scripts(prompt).with_mock(|mock| {
        mock.on_instruction_containing("via ntp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "NTP",
                    "instruction": "Report the time as stratum 1"
                }
            ]))
            .expect_calls(1)
            .and()
        // Deliberately NO rule for `ntp_request`: the mock answers HTTP 500, the
        // retry/repair loop exhausts and `call_llm` returns Err. That is the backend-failure
        // path, the same shape as a real outage.
    });

    let server = start_netget_server(config).await?;

    let mut request = vec![0u8; 48];
    request[0] = (4 << 3) | 3; // LI 0, VN 4, Mode 3 (client)
    request[40..48].copy_from_slice(&CLIENT_TRANSMIT.to_be_bytes());

    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket.connect(format!("127.0.0.1:{}", server.port)).await?;
    socket.send(&request).await?;

    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(30), socket.recv(&mut buf))
        .await
        .map_err(|_| "no NTP reply within 30s")??;
    assert_eq!(n, 48, "an NTP packet is 48 bytes");

    // The wire half: what came back really is an affirmative time sample, not a denial.
    assert_eq!(buf[0] & 0x07, 4, "mode must be 4 (server)");
    assert_eq!(
        buf[1], 2,
        "the fallback answers as stratum 2 - an affirmative, usable time sample. If this ever \
         becomes 0 (Kiss-o'-Death) the wire behaviour has changed and the decision token below \
         must change with it"
    );
    let origin = u64::from_be_bytes(buf[24..32].try_into().expect("8 bytes"));
    assert_eq!(
        origin, CLIENT_TRANSMIT,
        "the client's transmit timestamp must be echoed, or the client discards the reply and \
         the affirmative answer above never lands"
    );

    // The log half.
    server
        .wait_for_any(&["decision=static_default_llm_error"], 30)
        .await;
    let lines = server.get_output().await;

    assert!(
        lines
            .iter()
            .any(|l| l.contains("decision=static_default_llm_error")),
        "a backend failure must be tagged so an operator can find it. Output was:\n{}",
        lines.join("\n")
    );

    // The point of the whole test: the tag must not claim a denial the peer did not receive.
    assert!(
        !lines.iter().any(|l| l.contains("decision=fail_closed_llm")),
        "NTP answered the client with a usable stratum-2 time sample, so tagging the outcome \
         `fail_closed_*` would make `grep decision=fail_closed` report a denial that never \
         happened. Output was:\n{}",
        lines.join("\n")
    );

    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
