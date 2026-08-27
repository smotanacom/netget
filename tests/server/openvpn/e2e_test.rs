//! E2E tests for the OpenVPN control-plane responder.
//!
//! Two kinds of test live here.
//!
//! 1. **A real client.** `test_real_openvpn_client_accepts_our_reset_reply`
//!    drives the system's `openvpn` binary against the server and asserts the
//!    client logs `TLS: Initial packet from ...` — a line it emits only after
//!    parsing and accepting a `P_CONTROL_HARD_RESET_SERVER_V2`. It also asserts
//!    the client never reports a completed tunnel, because this server cannot
//!    build one.
//!
//! 2. **Raw UDP with an independent codec.** The remaining tests build request
//!    frames and decode replies with `super::wire`, which is written from the
//!    protocol layout and never calls NetGet's codec.
//!
//! No test requires root. The server has no TUN device, so there is nothing to
//! elevate for.
//!
//! **`openvpn` must be installed.** If it is missing the real-client test fails
//! rather than skipping: a capability check that returns success when the
//! capability is absent is worse than no test at all.

#![cfg(feature = "openvpn")]

use super::wire::*;
use crate::helpers::*;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::process::Command;
use tokio::time::timeout;

/// Static handler that answers every peer, so no model call happens per peer.
fn accept_handler() -> serde_json::Value {
    serde_json::json!([{
        "type": "open_server",
        "port": 0,
        "base_stack": "openvpn",
        "event_handlers": [{
            "event_pattern": "openvpn_peer_reset",
            "handler": {
                "type": "static",
                "actions": [{"type": "accept_peer", "reason": "e2e test"}]
            }
        }]
    }])
}

/// Static handler that refuses every peer.
fn reject_handler() -> serde_json::Value {
    serde_json::json!([{
        "type": "open_server",
        "port": 0,
        "base_stack": "openvpn",
        "event_handlers": [{
            "event_pattern": "openvpn_peer_reset",
            "handler": {
                "type": "static",
                "actions": [{"type": "reject_peer", "reason": "not on the allow list"}]
            }
        }]
    }])
}

/// Static handler that produces no decision at all, only a log line.
fn no_decision_handler() -> serde_json::Value {
    serde_json::json!([{
        "type": "open_server",
        "port": 0,
        "base_stack": "openvpn",
        "event_handlers": [{
            "event_pattern": "openvpn_peer_reset",
            "handler": {
                "type": "static",
                "actions": [{"type": "show_message", "message": "seen, but undecided"}]
            }
        }]
    }])
}

fn config_with(prompt: &str, startup: serde_json::Value) -> NetGetConfig {
    NetGetConfig::new(prompt).with_mock(move |mock| {
        mock.on_instruction_containing("OpenVPN")
            .respond_with_actions(startup)
            .expect_calls(1)
            .and()
    })
}

/// Bind a client socket and return it together with the server address.
async fn client_socket(port: u16) -> (UdpSocket, String) {
    let sock = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("failed to bind test client socket");
    (sock, format!("127.0.0.1:{}", port))
}

/// Receive one datagram, or `None` if nothing arrives within `secs`.
async fn recv_within(sock: &UdpSocket, secs: u64) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 65535];
    match timeout(Duration::from_secs(secs), sock.recv(&mut buf)).await {
        Ok(Ok(len)) => Some(buf[..len].to_vec()),
        Ok(Err(e)) => panic!("recv failed: {}", e),
        Err(_) => None,
    }
}

// ---------------------------------------------------------------------------
// A real openvpn client
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_real_openvpn_client_accepts_our_reset_reply() -> E2EResult<()> {
    let available = Command::new("openvpn")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        available,
        "the `openvpn` client is required for this test. Install it with \
         `brew install openvpn` (macOS) or `apt-get install openvpn` (Debian/Ubuntu). \
         This test is not skipped when the client is missing, because a skip that \
         reports success would hide a broken handshake."
    );

    let server = start_netget_server(config_with(
        "Start an OpenVPN honeypot on port {AVAILABLE_PORT}",
        accept_handler(),
    ))
    .await?;
    let port = server.port;

    // The client needs *some* client-side auth method configured before it will
    // start, and *some* way to validate a server certificate. --auth-user-pass
    // with a throwaway file and --peer-fingerprint with a bogus fingerprint
    // satisfy both without generating a PKI: neither is ever exercised, because
    // the handshake cannot get as far as a certificate.
    let dir = std::env::temp_dir().join(format!("netget_openvpn_e2e_{}", std::process::id()));
    tokio::fs::create_dir_all(&dir).await.expect("temp dir");
    let creds = dir.join("creds.txt");
    tokio::fs::write(&creds, "netget\nnetget\n")
        .await
        .expect("write creds");

    let mut client = Command::new("openvpn")
        .args([
            "--client",
            "--dev",
            "null",
            "--proto",
            "udp",
            "--remote",
            "127.0.0.1",
            &port.to_string(),
            "--nobind",
            "--verb",
            "4",
            "--auth-user-pass",
        ])
        .arg(&creds)
        .args([
            "--peer-fingerprint",
            "00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:\
             00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start the openvpn client");

    // Let the reset exchange happen, then take everything the client printed.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let _ = client.kill().await;
    let output = client.wait_with_output().await.expect("client output");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = tokio::fs::remove_dir_all(&dir).await;

    assert!(
        !log.contains("Options error"),
        "the openvpn client rejected its own command line, so nothing was tested:\n{}",
        log
    );

    // OpenVPN logs this line only after it has parsed a
    // P_CONTROL_HARD_RESET_SERVER_V2 and adopted the server's session id. A
    // reply with its fields in the wrong order never produces it.
    let marker = format!("TLS: Initial packet from [AF_INET]127.0.0.1:{}", port);
    assert!(
        log.contains(&marker),
        "the real openvpn client did not accept our reset reply (looked for {:?}).\n\
         Client log:\n{}\nServer log:\n{}",
        marker,
        log,
        server.get_output().await.join("\n")
    );

    // And be explicit about the limit: answering the reset is not a tunnel.
    assert!(
        !log.contains("Initialization Sequence Completed"),
        "the client reported a completed tunnel, which this server cannot build - if that \
         is now genuinely possible, the protocol's metadata and docs are wrong.\n{}",
        log
    );

    let server_log = server.get_output().await.join("\n");
    assert!(
        server_log.contains("HARD_RESET_SERVER_V2"),
        "server should log the reply it sent. Server log:\n{}",
        server_log
    );
    assert!(
        server_log.contains("TLS handshake record"),
        "server should log the ClientHello the real client sent next. Server log:\n{}",
        server_log
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw UDP against an independent codec
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_reset_reply_is_spec_correct_and_control_packets_are_acked() -> E2EResult<()> {
    let server = start_netget_server(config_with(
        "Start an OpenVPN honeypot on port {AVAILABLE_PORT}",
        accept_handler(),
    ))
    .await?;
    let (sock, addr) = client_socket(server.port).await;

    let client_session: u64 = 0x1122_3344_5566_7788;
    let client_reset_pid: u32 = 0;

    sock.send_to(&client_reset_v2(client_session, client_reset_pid), &addr)
        .await
        .expect("send reset");

    let reply = recv_within(&sock, 10)
        .await
        .expect("server must answer an accepted reset");

    assert_eq!(
        reply.len(),
        26,
        "a reset reply is 26 bytes; got {}: {}",
        reply.len(),
        to_hex(&reply)
    );

    let decoded = decode_control(&reply);
    assert_eq!(
        decoded.opcode, OP_HARD_RESET_SERVER_V2,
        "reply must be P_CONTROL_HARD_RESET_SERVER_V2, got opcode {}",
        decoded.opcode
    );
    assert_eq!(
        decoded.acks,
        vec![client_reset_pid],
        "the reply must acknowledge the packet id we actually sent"
    );
    assert_eq!(
        decoded.remote_session_id,
        Some(client_session),
        "the reply must echo our session id, not a fixed value"
    );
    assert_eq!(
        decoded.packet_id,
        Some(0),
        "the server's first control packet is numbered 0"
    );
    assert!(
        decoded.payload.is_empty(),
        "trailing bytes after the packet id would be parsed by a client as control payload: {}",
        to_hex(&decoded.payload)
    );

    let server_session = decoded.session_id;
    assert_ne!(
        server_session, 0,
        "the server must use a real session id of its own"
    );

    // A retransmitted reset must be answered again, with the same bytes, and
    // must not be treated as a new peer.
    sock.send_to(&client_reset_v2(client_session, client_reset_pid), &addr)
        .await
        .expect("send reset retransmission");
    let again = recv_within(&sock, 10)
        .await
        .expect("a retransmitted reset must be answered again");
    assert_eq!(
        again, reply,
        "the answer to a retransmitted reset must be identical"
    );

    // Now the control packet a real client sends next: a TLS ClientHello.
    let hello = tls_handshake_record(64);
    sock.send_to(
        &client_control_v1(client_session, server_session, 0, 1, &hello),
        &addr,
    )
    .await
    .expect("send control packet");

    let ack = recv_within(&sock, 10)
        .await
        .expect("the server must acknowledge a control packet");
    assert_eq!(
        ack.len(),
        22,
        "an ACK is 22 bytes; got {}: {}",
        ack.len(),
        to_hex(&ack)
    );

    let decoded_ack = decode_control(&ack);
    assert_eq!(decoded_ack.opcode, OP_ACK_V1);
    assert_eq!(
        decoded_ack.acks,
        vec![1],
        "the ACK must name the control packet id we sent"
    );
    assert_eq!(decoded_ack.remote_session_id, Some(client_session));
    assert_eq!(decoded_ack.session_id, server_session);
    assert_eq!(
        decoded_ack.packet_id, None,
        "P_ACK_V1 has no message packet id; the extra four bytes would be read as payload"
    );
    assert!(decoded_ack.payload.is_empty());

    // A data packet cannot be decrypted - no key exchange has happened - and
    // must not take the server down or produce a bogus reply.
    sock.send_to(&data_v2(1, &[0xEE; 64]), &addr)
        .await
        .expect("send data packet");
    assert!(
        recv_within(&sock, 2).await.is_none(),
        "the server must not answer a data packet it cannot decrypt"
    );

    // Hostile and unsupported input must not stop the server serving.
    let junk: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xFF; 3],
        hex("38090a7265e64d55eeff"), // ACK length 255, nothing behind it
        hex("50aabbccddeeff00112233445566778899"), // tls-crypt-v2 reset
        vec![0x20],                  // control opcode, nothing else
    ];
    for bytes in &junk {
        let _ = sock.send_to(bytes, &addr).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Still alive and still correct: a brand new peer gets a proper answer.
    let (sock2, _) = client_socket(server.port).await;
    let second_session: u64 = 0x0f0e_0d0c_0b0a_0908;
    sock2
        .send_to(&client_reset_v2(second_session, 3), &addr)
        .await
        .expect("send second reset");
    let reply2 = recv_within(&sock2, 10)
        .await
        .expect("the server must still answer after being fed junk");
    let decoded2 = decode_control(&reply2);
    assert_eq!(decoded2.opcode, OP_HARD_RESET_SERVER_V2);
    assert_eq!(decoded2.acks, vec![3]);
    assert_eq!(decoded2.remote_session_id, Some(second_session));

    let server_log = server.get_output().await.join("\n");
    assert!(
        server_log.contains("tls-crypt-v2"),
        "the server should say why it ignored the tls-crypt-v2 frame. Server log:\n{}",
        server_log
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_rejected_peer_receives_nothing() -> E2EResult<()> {
    let server = start_netget_server(config_with(
        "Start an OpenVPN honeypot on port {AVAILABLE_PORT}",
        reject_handler(),
    ))
    .await?;
    let (sock, addr) = client_socket(server.port).await;

    sock.send_to(&client_reset_v2(0xAAAA_BBBB_CCCC_DDDD, 0), &addr)
        .await
        .expect("send reset");

    assert!(
        recv_within(&sock, 4).await.is_none(),
        "reject_peer must be enforced: a refused peer receives no bytes at all"
    );

    // A retransmission must not slip past the refusal either.
    sock.send_to(&client_reset_v2(0xAAAA_BBBB_CCCC_DDDD, 1), &addr)
        .await
        .expect("send reset retransmission");
    assert!(
        recv_within(&sock, 4).await.is_none(),
        "a retransmitted reset from a refused peer must also go unanswered"
    );

    let server_log = server.get_output().await.join("\n");
    assert!(
        server_log.contains("decision=model_reject")
            && server_log.contains("not on the allow list"),
        "the refusal and its reason should be logged, and an explicit model refusal must \
         carry decision=model_reject rather than any fail_closed_ tag. Server log:\n{}",
        server_log
    );
    assert!(
        !server_log.contains("decision=fail_closed"),
        "a model refusal must never be recorded as fail-closed: the two are different \
         events and an operator greps for the difference. Server log:\n{}",
        server_log
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_absent_decision_fails_closed() -> E2EResult<()> {
    // The handler runs and produces output, but no accept_peer and no
    // reject_peer. Silence from the decision layer must not fall through to
    // answering the peer, and must be distinguishable in the log from an
    // explicit refusal.
    let server = start_netget_server(config_with(
        "Start an OpenVPN honeypot on port {AVAILABLE_PORT}",
        no_decision_handler(),
    ))
    .await?;
    let (sock, addr) = client_socket(server.port).await;

    sock.send_to(&client_reset_v2(0x0102_0304_0506_0708, 0), &addr)
        .await
        .expect("send reset");

    assert!(
        recv_within(&sock, 4).await.is_none(),
        "an undecided peer must not be answered - defaulting to a reply would make an LLM \
         outage indistinguishable from approval"
    );

    let server_log = server.get_output().await.join("\n");
    assert!(
        server_log.contains("decision=fail_closed_no_action"),
        "the no-decision path must be logged distinctly from a refusal, under its own \
         greppable decision= tag. Server log:\n{}",
        server_log
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
