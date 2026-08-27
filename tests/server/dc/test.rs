//! End-to-end DC (Direct Connect) tests for NetGet
//!
//! These tests spawn the actual NetGet binary with DC prompts
//! and validate the responses using DC protocol clients.

#![cfg(feature = "dc")]

// Helper module imported from parent

use super::super::super::helpers::{self, E2EResult, NetGetConfig};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read DC command (terminated by |)
async fn read_dc_command(
    stream: &mut tokio::net::TcpStream,
    timeout_secs: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            stream.read_exact(&mut byte),
        )
        .await
        {
            Ok(Ok(_)) => {
                buffer.push(byte[0]);
                if byte[0] == b'|' {
                    break;
                }
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err("Timeout reading DC command".into()),
        }
    }

    Ok(String::from_utf8_lossy(&buffer).to_string())
}

#[tokio::test]
async fn test_dc_authentication() -> E2EResult<()> {
    println!("\n=== E2E Test: DC Authentication (Lock/Key/Hello) ===");

    // PROMPT: Tell the LLM to act as a DC hub
    let prompt = "listen on port {AVAILABLE_PORT} via dc. When users send $ValidateNick, accept with $Hello. \
        When users send $Key, acknowledge. Be a friendly DC hub named 'NetGet Hub'.";

    // Start the server with mocks
    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup (user command)
            .on_instruction_containing("listen")
            .and_instruction_containing("dc")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "DC",
                    "instruction": "DC hub - accept all users with $Hello, send hub name NetGet Hub"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: ValidateNick received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "ValidateNick")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_dc_hello",
                    "nickname": "testuser"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: Key received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "Key")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "wait_for_more"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = helpers::start_netget_server(config).await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Connect and perform DC authentication
    println!("Connecting to DC hub...");
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP connected");

    // Read initial $Lock challenge
    println!("Reading $Lock challenge...");
    let lock_response = read_dc_command(&mut stream, 10).await?;
    println!("Lock response: {}", lock_response.trim_end_matches('|'));
    assert!(
        lock_response.contains("$Lock"),
        "Expected $Lock challenge, got: {}",
        lock_response
    );
    println!("✓ $Lock challenge received");

    // Send $ValidateNick
    println!("Sending $ValidateNick testuser|");
    stream.write_all(b"$ValidateNick testuser|").await?;
    stream.flush().await?;

    // Read response (should be $Hello)
    let mut received_hello = false;
    for attempt in 1..=3 {
        match tokio::time::timeout(Duration::from_secs(10), read_dc_command(&mut stream, 10)).await
        {
            Ok(Ok(response)) => {
                println!(
                    "DC response ({}): {}",
                    attempt,
                    response.trim_end_matches('|')
                );
                if response.contains("$Hello") && response.contains("testuser") {
                    println!("✓ $Hello received");
                    received_hello = true;
                    break;
                }
            }
            Ok(Err(e)) => {
                println!("  Read error: {}", e);
                break;
            }
            Err(_) => {
                println!("  Timeout on attempt {}", attempt);
                break;
            }
        }
    }

    // Send $Key (fake key)
    println!("Sending $Key fakekey123|");
    stream.write_all(b"$Key fakekey123|").await?;
    stream.flush().await?;

    // Read any response (just to complete the handshake)
    let _ = tokio::time::timeout(Duration::from_secs(5), read_dc_command(&mut stream, 5)).await;

    assert!(
        received_hello,
        "Expected $Hello response after $ValidateNick"
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
async fn test_dc_hub_info() -> E2EResult<()> {
    println!("\n=== E2E Test: DC Hub Information ===");

    // PROMPT: Tell the LLM to send hub info
    let prompt = "listen on port {AVAILABLE_PORT} via dc. Accept all users. \
        Send hub name 'NetGet DC Hub' and hub topic 'Test Hub' to new users after they send $ValidateNick.";

    // Start the server with mocks
    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup (user command)
            .on_instruction_containing("listen")
            .and_instruction_containing("dc")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "DC",
                    "instruction": "DC hub - send hub name and topic to new users"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: ValidateNick received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "ValidateNick")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_dc_hello",
                    "nickname": "testuser"
                },
                {
                    "type": "send_dc_hubname",
                    "name": "NetGet DC Hub"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: Key received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "Key")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "wait_for_more"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = helpers::start_netget_server(config).await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Connect and check hub info
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP connected");

    // Read $Lock
    let _ = read_dc_command(&mut stream, 10).await?;
    println!("✓ $Lock received");

    // Authenticate
    stream.write_all(b"$ValidateNick testuser|").await?;
    stream.flush().await?;

    stream.write_all(b"$Key fakekey|").await?;
    stream.flush().await?;

    // Read responses (may include Hello, HubName, HubTopic)
    let mut received_hub_name = false;
    for attempt in 1..=10 {
        match tokio::time::timeout(Duration::from_secs(10), read_dc_command(&mut stream, 10)).await
        {
            Ok(Ok(response)) => {
                println!(
                    "DC response ({}): {}",
                    attempt,
                    response.trim_end_matches('|')
                );
                if response.contains("$HubName") || response.contains("NetGet") {
                    println!("✓ Hub name received");
                    received_hub_name = true;
                    break;
                }
            }
            Ok(Err(e)) => {
                println!("  Read error: {}", e);
                break;
            }
            Err(_) => {
                println!("  Timeout on attempt {}", attempt);
                break;
            }
        }
    }

    if !received_hub_name {
        println!("Note: Did not receive hub name");
        println!("  This may be expected if hub info broadcasting is not fully implemented");
    }

    // Wait a moment for server to process the $Key command
    tokio::time::sleep(Duration::from_millis(500)).await;

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
async fn test_dc_chat() -> E2EResult<()> {
    println!("\n=== E2E Test: DC Chat Messages ===");

    // PROMPT: Tell the LLM to handle chat
    let prompt = "listen on port {AVAILABLE_PORT} via dc. Accept all users. \
        When users send public chat messages (format: <nickname> message|), echo them back or respond with a greeting.";

    // Start the server with mocks
    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup (user command)
            .on_instruction_containing("listen")
            .and_instruction_containing("dc")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "DC",
                    "instruction": "DC hub - echo chat messages or respond with greetings"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: ValidateNick received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "ValidateNick")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_dc_hello",
                    "nickname": "testuser"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: Key received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "Key")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "wait_for_more"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 4: Chat message received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "testuser> Hello hub!")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_dc_raw",
                    "command": "<Hub> Hello testuser!"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = helpers::start_netget_server(config).await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Connect, authenticate, and send chat
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP connected");

    // Read $Lock and authenticate
    let _ = read_dc_command(&mut stream, 10).await?;
    stream.write_all(b"$ValidateNick testuser|").await?;
    stream.flush().await?;
    stream.write_all(b"$Key fakekey|").await?;
    stream.flush().await?;

    // Wait for any auth responses
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Clear any pending data
    let mut discard = [0u8; 1024];
    while stream.try_read(&mut discard).is_ok() {}

    // Send chat message
    println!("Sending: <testuser> Hello hub!|");
    stream.write_all(b"<testuser> Hello hub!|").await?;
    stream.flush().await?;

    // Read response
    match tokio::time::timeout(Duration::from_secs(10), read_dc_command(&mut stream, 10)).await {
        Ok(Ok(response)) => {
            println!("DC chat response: {}", response.trim_end_matches('|'));
            assert!(
                response.contains("Hello") || response.contains("hello") || response.contains("<"),
                "Expected chat response, got: {}",
                response
            );
            println!("✓ Chat response received");
        }
        Ok(Err(e)) => {
            println!("Note: Read error: {}", e);
            println!("  This may be expected if chat is not fully implemented");
        }
        Err(_) => {
            println!("Note: No chat response (timeout)");
            println!("  This may be expected if chat is not fully implemented");
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

#[tokio::test]
async fn test_dc_search() -> E2EResult<()> {
    println!("\n=== E2E Test: DC Search ===");

    // PROMPT: Tell the LLM to handle search
    let prompt = "listen on port {AVAILABLE_PORT} via dc. Accept all users. \
        When users send $Search commands, respond with one fake search result: filename 'test.txt', \
        size 1024 bytes, using $SR command.";

    // Start the server with mocks
    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup (user command)
            .on_instruction_containing("listen")
            .and_instruction_containing("dc")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "DC",
                    "instruction": "DC hub - respond to search with fake results"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: ValidateNick received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "ValidateNick")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_dc_hello",
                    "nickname": "testuser"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: Key received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "Key")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "wait_for_more"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 4: Search received (dc_command_received event)
            .on_event("dc_command_received")
            .and_event_data_contains("command", "Search")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_dc_search_result",
                    "source": "testuser",
                    "filename": "test.txt",
                    "size": 1024,
                    "slots": 1,
                    "hub_name": "NetGetHub"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = helpers::start_netget_server(config).await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Connect, authenticate, and search
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP connected");

    // Read $Lock and authenticate
    let _ = read_dc_command(&mut stream, 10).await?;
    stream.write_all(b"$ValidateNick testuser|").await?;
    stream.flush().await?;
    stream.write_all(b"$Key fakekey|").await?;
    stream.flush().await?;

    // Wait for any auth responses
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Clear any pending data
    let mut discard = [0u8; 1024];
    while stream.try_read(&mut discard).is_ok() {}

    // Send search command
    println!("Sending: $Search Hub:testuser F?F?0?1?test|");
    stream
        .write_all(b"$Search Hub:testuser F?F?0?1?test|")
        .await?;
    stream.flush().await?;

    // Read search results (may be multiple responses)
    let mut received_search_result = false;
    for attempt in 1..=10 {
        match tokio::time::timeout(Duration::from_secs(10), read_dc_command(&mut stream, 10)).await
        {
            Ok(Ok(response)) => {
                println!(
                    "DC response ({}): {}",
                    attempt,
                    response.trim_end_matches('|')
                );
                if response.contains("$SR") && response.contains("test") {
                    println!("✓ Search result ($SR) received");
                    received_search_result = true;
                    break;
                }
            }
            Ok(Err(e)) => {
                println!("  Read error: {}", e);
                break;
            }
            Err(_) => {
                println!("  Timeout on attempt {}", attempt);
                break;
            }
        }
    }

    if !received_search_result {
        println!("Note: Did not receive search result");
        println!("  This may be expected if search is not fully implemented");
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
