//! What an NTP client gets when the LLM backend fails while the operator opted into LLM
//! control: the correct static time response, not silence and not a fabricated clock.
//!
//! A normal NTP time response is fully determined by the request plus the server's own clock,
//! so the mechanical answer is the safe fallback here. Falling back to the TRUE current time
//! fails *closed*, not open: an operator who opted into the LLM to skew the clock simply gets
//! the truth instead of a lie in their favour, and a client is never handed a fabricated
//! reading. Silence, by contrast, looks like a merely slow server and the client keeps polling.
//!
//! The packet is decoded here byte by byte against the RFC's field layout rather than through
//! the server's own builder, so the test is evidence and not a tautology.

#![cfg(feature = "ntp")]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;

/// The client's transmit timestamp, which must come back as the reply's origin timestamp.
const CLIENT_TRANSMIT: u64 = 0xE5F1_2345_89AB_CDEF;

/// NTP epoch (1900) is this many seconds before the Unix epoch (1970).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

#[tokio::test]
async fn test_ntp_answers_static_time_when_llm_fails() -> E2EResult<()> {
    // The instruction opts this server into LLM control; the mock then fails every request
    // (no matching rule -> HTTP 500), forcing the static fallback.
    let prompt = "listen on port {AVAILABLE_PORT} via ntp. Answer with the current time";

    let server_config = NetGetConfig::new_no_scripts(prompt).with_mock(|mock| {
        mock.on_instruction_containing("via ntp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "NTP",
                    "instruction": "Answer with the current time"
                }
            ]))
            .expect_calls(1)
            .and()
        // No rule for `ntp_request`: the mock answers 500, the LLM call fails, and the
        // server must fall back to the correct static time response.
    });

    let server = start_netget_server(server_config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A minimal NTPv4 client request: LI=0, VN=4, Mode=3.
    let mut request = vec![0u8; 48];
    request[0] = (4 << 3) | 3;
    request[40..48].copy_from_slice(&CLIENT_TRANSMIT.to_be_bytes());

    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket.connect(format!("127.0.0.1:{}", server.port)).await?;
    socket.send(&request).await?;

    let mut buf = vec![0u8; 1024];
    let n = tokio::time::timeout(Duration::from_secs(20), socket.recv(&mut buf))
        .await
        .map_err(|_| {
            "No NTP response within 20s - the server went silent on LLM failure, which is the \
             exact defect this test exists to catch"
        })??;

    assert_eq!(n, 48, "an NTP packet is 48 bytes");

    let leap_indicator = (buf[0] >> 6) & 0x03;
    let version = (buf[0] >> 3) & 0x07;
    let mode = buf[0] & 0x07;
    assert_eq!(mode, 4, "mode must be 4 (server)");
    assert_eq!(version, 4, "the reply must use the client's NTP version");
    assert_eq!(
        leap_indicator, 0,
        "LI must be 0 (no warning): the static fallback is a usable time answer"
    );
    assert_eq!(
        buf[1], 2,
        "the static default answers as stratum 2, not a Kiss-o'-Death (stratum 0)"
    );

    // Origin timestamp (bytes 24-31) must be the client's transmit timestamp verbatim, or the
    // client discards the reply as unrelated to its request - which is silence again.
    let origin = u64::from_be_bytes(buf[24..32].try_into().expect("8 bytes"));
    assert_eq!(
        origin, CLIENT_TRANSMIT,
        "the client's transmit timestamp must be echoed as the origin timestamp"
    );

    // Transmit timestamp (bytes 40-47) must be a real, current clock reading.
    let transmit = u64::from_be_bytes(buf[40..48].try_into().expect("8 bytes"));
    assert_ne!(
        transmit, 0,
        "the transmit timestamp must be a real clock reading"
    );
    let transmit_secs = (transmit >> 32) as u64;
    let now_ntp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + NTP_UNIX_OFFSET;
    assert!(
        transmit_secs.abs_diff(now_ntp) < 300,
        "the static fallback must report the true current time, not a fabricated one \
         (transmit {transmit_secs} vs now {now_ntp})"
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
