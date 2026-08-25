//! End-to-end DNS-over-HTTPS (DoH) tests for NetGet
//!
//! This test spawns a single NetGet DoH server with a Python script
//! and validates multiple query types against the same server instance.

#![cfg(feature = "doh")]

use super::super::super::helpers::{self, E2EResult};
use hickory_proto::op::{Message as DnsMessage, Query};
use hickory_proto::rr::{Name, RecordType};
use reqwest::Client;
use std::str::FromStr;
use std::time::Duration;

/// Helper to query DoH server using GET method (base64url encoded)
async fn query_doh_get(
    client: &Client,
    port: u16,
    domain: &str,
    record_type: RecordType,
) -> E2EResult<DnsMessage> {
    let url = format!("https://127.0.0.1:{}/dns-query", port);

    // Build DNS query message
    let name = Name::from_str(domain)?;
    let mut query_msg = DnsMessage::new();
    query_msg.add_query(Query::query(name, record_type));
    query_msg.set_recursion_desired(true);

    // Serialize to wire format
    let query_bytes = query_msg.to_vec()?;

    // Encode as base64url
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let encoded = URL_SAFE_NO_PAD.encode(&query_bytes);

    // Send GET request
    let response = client.get(&url).query(&[("dns", encoded)]).send().await?;

    let response_bytes = response.bytes().await?;

    // Parse DNS response
    let dns_response = DnsMessage::from_vec(&response_bytes)?;

    Ok(dns_response)
}

/// Helper to query DoH server using POST method (binary DNS message)
async fn query_doh_post(
    client: &Client,
    port: u16,
    domain: &str,
    record_type: RecordType,
) -> E2EResult<DnsMessage> {
    let url = format!("https://127.0.0.1:{}/dns-query", port);

    // Build DNS query message
    let name = Name::from_str(domain)?;
    let mut query_msg = DnsMessage::new();
    query_msg.add_query(Query::query(name, record_type));
    query_msg.set_recursion_desired(true);

    // Serialize to wire format
    let query_bytes = query_msg.to_vec()?;

    // Send POST request
    let response = client
        .post(&url)
        .header("Content-Type", "application/dns-message")
        .body(query_bytes)
        .send()
        .await?;

    let response_bytes = response.bytes().await?;

    // Parse DNS response
    let dns_response = DnsMessage::from_vec(&response_bytes)?;

    Ok(dns_response)
}

/// Create an HTTP client that accepts self-signed certificates (for testing)
fn create_insecure_client() -> E2EResult<Client> {
    // Initialize rustls crypto provider (required for rustls 0.23+)
    use rustls::crypto::CryptoProvider;
    let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    // `http2_prior_knowledge()` asserts HTTP/2 out of band and skips ALPN entirely, so nothing
    // below exercises protocol negotiation. That is a real gap in coverage — see
    // `server_advertises_h2_alpn` for what does cover it, and the note in
    // src/server/doh/CLAUDE.md for why this client cannot: under
    // `danger_accept_invalid_certs` reqwest builds its own rustls ClientConfig that does not
    // offer `h2`, so dropping prior knowledge here just makes it send HTTP/1.1 to an
    // HTTP/2-only server.
    let client = Client::builder()
        .danger_accept_invalid_certs(true)
        .http2_prior_knowledge()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(client)
}

#[tokio::test]
async fn test_doh_server() -> E2EResult<()> {
    println!("\n=== E2E Test: DNS-over-HTTPS Server with Mocks ===");

    // Create server with mocks for startup and DNS queries
    let server_config = helpers::NetGetConfig::new(
        "listen on port {AVAILABLE_PORT} via doh. Respond to all A record queries for example.com with IP 93.184.216.34 and TTL 300."
    )
    .with_mock(|mock| {
        mock
            // Mock 1: First GET query for example.com - MUST BE FIRST (most specific)
            .on_event("doh_query")
            .and_event_data_contains("domain", "example.com")
            .and_event_data_contains("method", "GET")
            .respond_with_actions_from_event(|event_data| {
                let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                serde_json::json!([
                    {
                        "type": "send_dns_a_response",
                        "query_id": query_id,
                        "domain": "example.com",
                        "ip": "93.184.216.34",
                        "ttl": 300
                    }
                ])
            })
            .expect_calls(1)
            .and()
            // Mock 2: POST query for example.com - MUST BE SECOND (most specific)
            .on_event("doh_query")
            .and_event_data_contains("domain", "example.com")
            .and_event_data_contains("method", "POST")
            .respond_with_actions_from_event(|event_data| {
                let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                serde_json::json!([
                    {
                        "type": "send_dns_a_response",
                        "query_id": query_id,
                        "domain": "example.com",
                        "ip": "93.184.216.34",
                        "ttl": 300
                    }
                ])
            })
            .expect_calls(1)
            .and()
            // Mock 3: Second GET query for test.com - MUST BE THIRD (most specific)
            .on_event("doh_query")
            .and_event_data_contains("domain", "test.com")
            .and_event_data_contains("method", "GET")
            .respond_with_actions_from_event(|event_data| {
                let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                serde_json::json!([
                    {
                        "type": "send_dns_a_response",
                        "query_id": query_id,
                        "domain": "test.com",
                        "ip": "93.184.216.34",
                        "ttl": 300
                    }
                ])
            })
            .expect_calls(1)
            .and()
            // Mock 4: Server startup - MUST BE LAST (less specific)
            .on_instruction_containing("listen")
            .and_instruction_containing("doh")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "DoH",
                    "instruction": "DNS-over-HTTPS server responding to queries"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut server = helpers::start_netget_server(server_config).await?;

    println!("DoH server started on port {}", server.port);

    // Wait for server to fully initialize
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Create HTTP client
    let client = create_insecure_client()?;

    // Test both GET and POST methods against the same server
    println!("\n[Test 1] Querying via GET method...");
    let response1 = query_doh_get(&client, server.port, "example.com.", RecordType::A).await?;
    assert!(!response1.answers().is_empty(), "Expected answer via GET");
    println!("✓ GET response: {:?}", response1.answers()[0]);

    println!("\n[Test 2] Querying via POST method...");
    let response2 = query_doh_post(&client, server.port, "example.com.", RecordType::A).await?;
    assert!(!response2.answers().is_empty(), "Expected answer via POST");
    println!("✓ POST response: {:?}", response2.answers()[0]);

    println!("\n[Test 3] Another GET query - different domain...");
    let response3 = query_doh_get(&client, server.port, "test.com.", RecordType::A).await?;
    assert!(!response3.answers().is_empty(), "Expected answer from mock");
    println!("✓ GET response: {:?}", response3.answers()[0]);

    println!("\n=== All DoH tests passed! ===");

    // Verify mock expectations were met
    server.verify_mocks().await?;

    // Cleanup
    server.stop().await?;

    Ok(())
}

/// The DoH listener must advertise `h2` in the TLS handshake.
///
/// RFC 8484 DoH runs over HTTP/2 and this server speaks nothing else, so a client that
/// negotiates normally has to be told `h2` during the handshake — otherwise it falls back to
/// HTTP/1.1, which hyper's `http2::Builder` rejects with "http2 error", or it refuses outright.
///
/// The server advertised no ALPN at all until August 2026 and `test_doh_server` did not catch
/// it, because that test connects with `http2_prior_knowledge()` and so never negotiates. This
/// asserts the property that test cannot: the config the listener is built from offers exactly
/// `h2`, and nothing else that the server could not honour.
#[test]
fn server_advertises_h2_alpn() {
    let config = ::netget::server::tls_cert_manager::generate_default_tls_config_with_alpn(&["h2"])
        .expect("build DoH TLS config");
    assert_eq!(
        config.alpn_protocols,
        vec![b"h2".to_vec()],
        "DoH must advertise exactly h2: anything less leaves a negotiating client unable to \
         reach an HTTP/2-only server, anything more advertises a protocol it cannot speak"
    );

    // The shared default must stay ALPN-less: `dot`, `tls` and `quic` are built from it and
    // document themselves as negotiating nothing.
    let shared = ::netget::server::tls_cert_manager::generate_default_tls_config()
        .expect("build shared TLS config");
    assert!(
        shared.alpn_protocols.is_empty(),
        "the shared default gained ALPN, which changes dot/tls/quic behaviour out from under them"
    );
}
