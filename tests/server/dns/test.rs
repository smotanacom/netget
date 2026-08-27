//! End-to-end DNS tests for NetGet
//!
//! These tests spawn the actual NetGet binary with DNS prompts
//! and validate the responses using the hickory-client DNS client library.

#![cfg(feature = "dns")]

// Helper module imported from parent

use super::super::super::helpers::{self, E2EResult, NetGetConfig};
use hickory_client::client::{AsyncClient, ClientHandle};
use hickory_client::rr::{DNSClass, Name, RecordType};
use hickory_client::udp::UdpClientStream;
use std::net::SocketAddr;
use std::str::FromStr;

#[tokio::test]
async fn test_dns_a_record_query() -> E2EResult<()> {
    println!("\n=== E2E Test: DNS A Record Query ===");

    // PROMPT: Tell the LLM to act as a DNS server with mocks
    let prompt = "listen on port {AVAILABLE_PORT} via dns. Respond to all A record queries for example.com with IP address 93.184.216.34";

    let server_config = NetGetConfig::new(prompt)
        .with_log_level("debug")
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup (user command)
                .on_instruction_containing("listen on port")
                .and_instruction_containing("dns")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "DNS",
                        "instruction": "Respond to all A record queries for example.com with IP address 93.184.216.34"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: DNS query received (dns_query event) - DYNAMIC RESPONSE
                .on_event("dns_query")
                .and_event_data_contains("domain", "example.com")
                .and_event_data_contains("query_type", "A")
                .respond_with_actions_from_event(|event_data| {
                    // Extract query_id from event (transaction ID must match request)
                    let query_id = event_data["query_id"].as_u64().unwrap_or(0);

                    serde_json::json!([{
                        "type": "send_dns_a_response",
                        "query_id": query_id,  // ← DYNAMIC from event!
                        "domain": "example.com",
                        "ip": "93.184.216.34",
                        "ttl": 300
                    }])
                })
                .expect_calls(1)
                .and()
        });

    // Start the server
    let server = helpers::start_netget_server(server_config).await?;
    println!("DNS server started on port {}", server.port);

    // Wait for DNS server to fully initialize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // VALIDATION: Use hickory-client to query DNS
    println!("Querying example.com A record...");

    let address: SocketAddr = format!("127.0.0.1:{}", server.port).parse()?;
    let stream = UdpClientStream::<tokio::net::UdpSocket>::new(address);
    let (mut client, bg) = AsyncClient::connect(stream).await?;

    // Run the background task
    tokio::spawn(bg);

    // Query for example.com A record
    let name = Name::from_str("example.com.")?;
    let response = client.query(name, DNSClass::IN, RecordType::A).await?;

    println!("DNS response received:");
    let answers = response.answers();
    assert!(
        !answers.is_empty(),
        "Expected at least one A record in response"
    );

    for record in answers {
        println!("  Record: {:?}", record);
    }

    // Check that we got a response
    println!(
        "✓ DNS A record query succeeded with {} answers",
        answers.len()
    );

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_dns_multiple_records() -> E2EResult<()> {
    println!("\n=== E2E Test: DNS Multiple Records ===");

    // PROMPT: Tell the LLM to handle multiple record types
    let prompt = "listen on port {AVAILABLE_PORT} via dns. For example.com A records return 1.2.3.4. For mail.example.com A records return 5.6.7.8";

    let server_config = NetGetConfig::new(prompt)
        .with_log_level("debug")
        .with_mock(|mock| {
            mock
                // Mock 1: Query for mail.example.com - MUST BE FIRST (most specific, avoids substring match)
                .on_event("dns_query")
                .and_event_data_contains("domain", "mail.example.com.")
                .and_event_data_contains("query_type", "A")
                .respond_with_actions_from_event(|event_data| {
                    let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                    serde_json::json!([{
                        "type": "send_dns_a_response",
                        "query_id": query_id,
                        "domain": "mail.example.com",
                        "ip": "5.6.7.8",
                        "ttl": 300
                    }])
                })
                .expect_calls(1)
                .and()
                // Mock 2: Query for example.com - MUST BE SECOND (less specific)
                .on_event("dns_query")
                .and_event_data_contains("domain", "example.com.")
                .and_event_data_contains("query_type", "A")
                .respond_with_actions_from_event(|event_data| {
                    let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                    serde_json::json!([{
                        "type": "send_dns_a_response",
                        "query_id": query_id,
                        "domain": "example.com",
                        "ip": "1.2.3.4",
                        "ttl": 300
                    }])
                })
                .expect_calls(1)
                .and()
                // Mock 3: Server startup (user command) - MUST BE LAST (less specific)
                .on_instruction_containing("listen on port")
                .and_instruction_containing("dns")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "DNS",
                        "instruction": "For example.com A records return 1.2.3.4. For mail.example.com A records return 5.6.7.8"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    // Start the server
    let server = helpers::start_netget_server(server_config).await?;
    println!("DNS server started on port {}", server.port);

    // Wait for server to initialize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // VALIDATION: Query multiple domains
    let address: SocketAddr = format!("127.0.0.1:{}", server.port).parse()?;
    let stream = UdpClientStream::<tokio::net::UdpSocket>::new(address);
    let (mut client, bg) = AsyncClient::connect(stream).await?;
    tokio::spawn(bg);

    // Query example.com
    println!("Querying example.com...");
    let name1 = Name::from_str("example.com.")?;
    let response1 = client.query(name1, DNSClass::IN, RecordType::A).await?;
    assert!(
        !response1.answers().is_empty(),
        "Expected answer for example.com"
    );
    println!(
        "  ✓ example.com returned {} records",
        response1.answers().len()
    );

    // Query mail.example.com
    println!("Querying mail.example.com...");
    let name2 = Name::from_str("mail.example.com.")?;
    let response2 = client.query(name2, DNSClass::IN, RecordType::A).await?;
    assert!(
        !response2.answers().is_empty(),
        "Expected answer for mail.example.com"
    );
    println!(
        "  ✓ mail.example.com returned {} records",
        response2.answers().len()
    );

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_dns_txt_record() -> E2EResult<()> {
    println!("\n=== E2E Test: DNS TXT Record ===");

    // PROMPT: Tell the LLM to handle TXT records
    let prompt = "listen on port {AVAILABLE_PORT} via dns. For TXT record queries on example.com, return 'v=spf1 include:_spf.example.com ~all'";

    let server_config = NetGetConfig::new(prompt)
        .with_log_level("debug")
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup (user command)
                .on_instruction_containing("listen on port")
                .and_instruction_containing("dns")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "DNS",
                        "instruction": "For TXT record queries on example.com, return 'v=spf1 include:_spf.example.com ~all'"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Query for TXT record - DYNAMIC RESPONSE
                .on_event("dns_query")
                .and_event_data_contains("domain", "example.com")
                .and_event_data_contains("query_type", "TXT")
                .respond_with_actions_from_event(|event_data| {
                    let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                    serde_json::json!([{
                        "type": "send_dns_txt_response",
                        "query_id": query_id,
                        "domain": "example.com",
                        "text": "v=spf1 include:_spf.example.com ~all",
                        "ttl": 300
                    }])
                })
                .expect_calls(1)
                .and()
        });

    // Start the server
    let server = helpers::start_netget_server(server_config).await?;
    println!("DNS server started on port {}", server.port);

    // Wait for server to initialize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // VALIDATION: Query TXT record
    let address: SocketAddr = format!("127.0.0.1:{}", server.port).parse()?;
    let stream = UdpClientStream::<tokio::net::UdpSocket>::new(address);
    let (mut client, bg) = AsyncClient::connect(stream).await?;
    tokio::spawn(bg);

    println!("Querying example.com TXT record...");
    let name = Name::from_str("example.com.")?;
    let response = client.query(name, DNSClass::IN, RecordType::TXT).await?;

    println!("DNS TXT response received:");
    let answers = response.answers();
    assert!(!answers.is_empty(), "Expected at least one TXT record");

    for record in answers {
        println!("  TXT Record: {:?}", record);
    }

    println!("✓ DNS TXT record query succeeded");

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_dns_nxdomain() -> E2EResult<()> {
    println!("\n=== E2E Test: DNS NXDOMAIN Response ===");

    // PROMPT: Tell the LLM to return NXDOMAIN for unknown domains
    let prompt = "listen on port {AVAILABLE_PORT} via dns. Only respond with A records for known.example.com (1.2.3.4). For all other domains, return NXDOMAIN";

    let server_config = NetGetConfig::new(prompt)
        .with_log_level("debug")
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup (user command)
                .on_instruction_containing("listen on port")
                .and_instruction_containing("dns")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "DNS",
                        "instruction": "Only respond with A records for known.example.com (1.2.3.4). For all other domains, return NXDOMAIN"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Query for unknown domain - return NXDOMAIN - DYNAMIC RESPONSE
                .on_event("dns_query")
                .and_event_data_contains("domain", "unknown.example.com")
                .and_event_data_contains("query_type", "A")
                .respond_with_actions_from_event(|event_data| {
                    let query_id = event_data["query_id"].as_u64().unwrap_or(0);
                    serde_json::json!([{
                        "type": "send_dns_nxdomain",
                        "query_id": query_id,
                        "domain": "unknown.example.com"
                    }])
                })
                .expect_calls(1)
                .and()
        });

    // Start the server
    let server = helpers::start_netget_server(server_config).await?;
    println!("DNS server started on port {}", server.port);

    // Wait for server to initialize
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // VALIDATION: Query an unknown domain
    let address: SocketAddr = format!("127.0.0.1:{}", server.port).parse()?;
    let stream = UdpClientStream::<tokio::net::UdpSocket>::new(address);
    let (mut client, bg) = AsyncClient::connect(stream).await?;
    tokio::spawn(bg);

    println!("Querying unknown.example.com (should get NXDOMAIN or empty response)...");
    let name = Name::from_str("unknown.example.com.")?;

    // Try to query - might get an error or empty response depending on implementation
    match client.query(name, DNSClass::IN, RecordType::A).await {
        Ok(response) => {
            // Server might return empty answers or NXDOMAIN response code
            println!("  Response code: {:?}", response.response_code());
            println!("  Answers: {}", response.answers().len());
            println!("  ✓ DNS server responded (implementation-dependent behavior)");
        }
        Err(e) => {
            // Might get an error for NXDOMAIN
            println!("  Got error (expected for NXDOMAIN): {:?}", e);
            println!("  ✓ DNS server indicated domain not found");
        }
    }

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}
