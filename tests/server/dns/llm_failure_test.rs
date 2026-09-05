//! What a DNS client gets when the LLM backend fails.
//!
//! The failure is forced by configuring a mock for the *startup* instruction only. The
//! `dns_query` event then matches no rule, the mock Ollama server answers HTTP 500, and
//! `call_llm` returns `Err` - the same shape as a real backend outage, an overload, or a
//! malformed model response.
//!
//! Before this path existed the server wrote nothing at all, and the client sat in `recvfrom`
//! until its own timeout (5s per server in glibc's resolver) with no way to tell an outage from
//! a black hole. The assertion below is at the protocol level: RCODE 2 (SERVFAIL), the
//! transaction ID echoed, and the question section repeated - all three are required or a real
//! stub resolver discards the packet and we are back to silence.

#![cfg(feature = "dns")]

use crate::server::helpers::{start_netget_server, E2EResult, NetGetConfig};
use hickory_proto::op::{Message as DnsMessage, MessageType, ResponseCode};
use hickory_proto::rr::{Name, RecordType};
use hickory_proto::serialize::binary::BinEncodable;
use std::str::FromStr;
use std::time::Duration;
use tokio::net::UdpSocket;

const QUERY_ID: u16 = 0xBEEF;

#[tokio::test]
async fn test_dns_answers_servfail_when_llm_fails() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via dns. Respond to A queries for example.com";

    let server_config = NetGetConfig::new_no_scripts(prompt).with_mock(|mock| {
        mock.on_instruction_containing("via dns")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "DNS",
                    "instruction": "Respond to A queries for example.com"
                }
            ]))
            .expect_calls(1)
            .and()
        // Deliberately NO rule for the `dns_query` event: the mock answers 500,
        // which is what drives the server down its LLM-failure path.
    });

    let server = start_netget_server(server_config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Build a real DNS query rather than driving a resolver: a resolver would hide the
    // wire-level detail this test is about.
    let mut query = DnsMessage::new();
    query.set_id(QUERY_ID);
    query.set_message_type(MessageType::Query);
    query.set_recursion_desired(true);
    query.add_query(hickory_proto::op::Query::query(
        Name::from_str("example.com.")?,
        RecordType::A,
    ));
    let query_bytes = query.to_bytes()?;

    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    socket.connect(format!("127.0.0.1:{}", server.port)).await?;
    socket.send(&query_bytes).await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(20), socket.recv(&mut buf))
        .await
        .map_err(|_| {
            "No DNS response within 20s - the server went silent on LLM failure, which is the \
             exact defect this test exists to catch"
        })??;

    // Byte-level check first, independent of any decoder: RCODE is the low nibble of byte 3,
    // and QR (byte 2 bit 7) must be set for this to be a response at all.
    assert!(n >= 12, "response is shorter than a DNS header: {n} bytes");
    assert_eq!(buf[0], (QUERY_ID >> 8) as u8, "transaction ID high byte");
    assert_eq!(buf[1], (QUERY_ID & 0xFF) as u8, "transaction ID low byte");
    assert_eq!(buf[2] & 0x80, 0x80, "QR bit must mark this as a response");
    assert_eq!(buf[3] & 0x0F, 2, "RCODE must be 2 (SERVFAIL)");

    let response = DnsMessage::from_vec(&buf[..n])?;
    assert_eq!(response.id(), QUERY_ID, "transaction ID must be echoed");
    assert_eq!(response.message_type(), MessageType::Response);
    assert_eq!(
        response.response_code(),
        ResponseCode::ServFail,
        "LLM failure must be reported as SERVFAIL"
    );
    assert_eq!(
        response.answers().len(),
        0,
        "a SERVFAIL must not carry answers"
    );

    // The question section must come back, or glibc/systemd-resolved/dig discard the packet.
    // This regressed once before (fixed in 6a384617) and is easy to lose again.
    let questions = response.queries();
    assert_eq!(questions.len(), 1, "question section must be echoed");
    assert_eq!(questions[0].name(), &Name::from_str("example.com.")?);
    assert_eq!(questions[0].query_type(), RecordType::A);

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
