//! What an MQTT client gets when the LLM backend fails - and the fail-open this closed.
//!
//! MQTT collapsed "the handler ran and declined to decide" and "the handler could not run" into
//! one outcome, `Silent`, and then applied the permissive default for it. For CONNECT that
//! default is `build_connack(0, ...)`, which is **Connection Accepted**: an LLM outage
//! authenticated every client that asked, credentials included, and a model's explicit refusal
//! was indistinguishable from the backend being down. SUBSCRIBE was the same shape one level
//! down - the default granted every requested QoS on every filter, so an outage made the
//! subscription decision.
//!
//! The permissive defaults are correct for a handler that ran and said nothing. They are only
//! reachable now when that is what happened.
//!
//! MQTT 3.1.1's refusals, and why each one:
//!
//! * CONNECT -> CONNACK return code 3, "server unavailable" (§3.2.2.3), then close, which the
//!   spec requires after any non-zero CONNACK.
//! * SUBSCRIBE -> SUBACK return code 0x80, "failure" (§3.9.3), one per filter. This is the
//!   only per-request failure code 3.1.1 has.
//! * PUBLISH / UNSUBSCRIBE -> close the connection. PUBACK and UNSUBACK are a bare packet
//!   identifier with no failure code, and sending one would tell the client its message was
//!   taken or its subscriptions were removed. Closing instead leaves QoS 1/2 redelivery to do
//!   its job.
//!
//! The packets below are built and decoded by hand against the spec rather than with a client
//! library, because the assertion is about specific bytes in specific positions.

#![cfg(feature = "mqtt")]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A minimal MQTT 3.1.1 CONNECT: clean session, no will, no credentials.
fn build_connect(client_id: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x04, b'M', b'Q', b'T', b'T']); // protocol name
    body.push(0x04); // protocol level 4 = 3.1.1
    body.push(0x02); // connect flags: clean session
    body.extend_from_slice(&[0x00, 0x3C]); // keep alive 60s
    body.extend_from_slice(&(client_id.len() as u16).to_be_bytes());
    body.extend_from_slice(client_id.as_bytes());

    let mut packet = vec![0x10]; // CONNECT
    packet.extend_from_slice(&encode_remaining_length(body.len()));
    packet.extend_from_slice(&body);
    packet
}

/// A SUBSCRIBE for one filter at the given QoS.
fn build_subscribe(packet_id: u16, filter: &str, qos: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    body.extend_from_slice(&(filter.len() as u16).to_be_bytes());
    body.extend_from_slice(filter.as_bytes());
    body.push(qos);

    let mut packet = vec![0x82]; // SUBSCRIBE, flags 0b0010 as the spec requires
    packet.extend_from_slice(&encode_remaining_length(body.len()));
    packet.extend_from_slice(&body);
    packet
}

fn encode_remaining_length(mut len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out
}

async fn read_exact_or_fail(stream: &mut TcpStream, n: usize, what: &str) -> E2EResult<Vec<u8>> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(Duration::from_secs(20), stream.read_exact(&mut buf))
        .await
        .map_err(|_| {
            format!(
                "No {what} within 20s - the broker went silent on LLM failure, which is the \
                 exact defect this test exists to catch"
            )
        })??;
    Ok(buf)
}

/// CONNECT with a failing backend must be refused, not accepted.
#[tokio::test]
async fn test_mqtt_refuses_connect_when_llm_fails() -> E2EResult<()> {
    let config = NetGetConfig::new_no_scripts("Start an MQTT broker on port {AVAILABLE_PORT}")
        .with_mock(|mock| {
            mock.on_instruction_containing("MQTT broker")
                .respond_with_actions(serde_json::json!([
                    {"type": "open_server", "port": 0, "base_stack": "MQTT", "instruction": "MQTT broker"}
                ]))
                .expect_calls(1)
                .and()
            // No rule for `mqtt_connect`.
        });

    let server = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    stream.write_all(&build_connect("failtest")).await?;
    stream.flush().await?;

    let connack = read_exact_or_fail(&mut stream, 4, "CONNACK").await?;
    println!("MQTT CONNACK: {connack:02x?}");

    assert_eq!(
        connack[0] >> 4,
        2,
        "expected a CONNACK packet: {connack:02x?}"
    );
    assert_eq!(
        connack[1], 0x02,
        "CONNACK has a 2-byte body: {connack:02x?}"
    );
    assert_ne!(
        connack[3], 0x00,
        "return code 0 is Connection Accepted. An LLM outage must never accept an MQTT \
         session - this is the fail-open this test exists to close: {connack:02x?}"
    );
    assert_eq!(
        connack[3], 0x03,
        "expected return code 3 (server unavailable): {connack:02x?}"
    );

    // 3.2.2.3: the server must close the connection after a non-zero CONNACK. A reset counts
    // as closed - what must not happen is the socket staying usable or the read blocking.
    // Checked after `verify_mocks` so a failure here is not masked by the harness's own
    // "dropped without verifying" panic.
    let mut trailing = [0u8; 1];
    let closed =
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut trailing)).await {
            Err(_) => Err("the broker left the connection open after refusing CONNECT".to_string()),
            Ok(Ok(0)) | Ok(Err(_)) => Ok(()),
            Ok(Ok(n)) => Err(format!(
                "expected EOF after a refused CONNACK, got {n} more byte(s): {trailing:02x?}"
            )),
        };

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    closed?;
    Ok(())
}

/// CONNECT is answered, SUBSCRIBE is not: every filter must come back 0x80, not granted.
#[tokio::test]
async fn test_mqtt_refuses_subscribe_when_llm_fails() -> E2EResult<()> {
    let config = NetGetConfig::new_no_scripts("Start an MQTT broker on port {AVAILABLE_PORT}")
        .with_mock(|mock| {
            mock.on_instruction_containing("MQTT broker")
                .respond_with_actions(serde_json::json!([
                    {"type": "open_server", "port": 0, "base_stack": "MQTT", "instruction": "MQTT broker"}
                ]))
                .expect_calls(1)
                .and()
                .on_event("mqtt_connect")
                .respond_with_actions(serde_json::json!([
                    {"type": "mqtt_connack", "return_code": 0, "session_present": false}
                ]))
                .expect_calls(1)
                .and()
            // No rule for `mqtt_subscribe`.
        });

    let server = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    stream.write_all(&build_connect("subtest")).await?;
    stream.flush().await?;

    let connack = read_exact_or_fail(&mut stream, 4, "CONNACK").await?;
    assert_eq!(
        connack[3], 0x00,
        "expected the mocked accept: {connack:02x?}"
    );

    stream
        .write_all(&build_subscribe(0x1234, "sensors/#", 1))
        .await?;
    stream.flush().await?;

    // SUBACK: type byte, remaining length, 2-byte packet id, one return code per filter.
    let header = read_exact_or_fail(&mut stream, 2, "SUBACK").await?;
    assert_eq!(header[0] >> 4, 9, "expected a SUBACK packet: {header:02x?}");
    let body = read_exact_or_fail(&mut stream, header[1] as usize, "SUBACK body").await?;
    println!("MQTT SUBACK body: {body:02x?}");

    assert_eq!(
        u16::from_be_bytes([body[0], body[1]]),
        0x1234,
        "SUBACK must echo the packet identifier: {body:02x?}"
    );
    assert_eq!(
        body.len(),
        3,
        "one return code per requested filter: {body:02x?}"
    );
    assert_eq!(
        body[2], 0x80,
        "expected 0x80 (failure) rather than a granted QoS - granting a subscription is an \
         access decision, and nothing decided it: {body:02x?}"
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
