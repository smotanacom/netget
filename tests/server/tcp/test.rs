//! End-to-end TCP tests for NetGet
//!
//! These tests spawn the actual NetGet binary with TCP/FTP prompts
//! and validate the responses using raw TCP connections for speed.
//!
//! Note: Full FTP client testing (with suppaftp) is too slow (>2 minutes)
//! due to multiple LLM round-trips required for each FTP command.
//! Instead, we test individual FTP protocol commands with raw TCP.

#![cfg(feature = "tcp")]

// Helper module imported from parent

use super::super::super::helpers::{self, E2EResult, NetGetConfig};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn test_ftp_greeting() -> E2EResult<()> {
    println!("\n=== E2E Test: FTP Greeting ===");

    // PROMPT: Tell the LLM to respond to CONNECT with FTP greeting
    let prompt = "listen on port {AVAILABLE_PORT} via ftp. When a client sends 'CONNECT', respond with '220 NetGet FTP Server\\r\\n'";

    // Start the server with mocks
    let server = helpers::start_netget_server(NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: User command interpretation (start server)
            .on_instruction_containing("ftp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "FTP server that responds to CONNECT with 220 greeting"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: TCP data received event (send greeting when client sends CONNECT)
            .on_event("tcp_data_received")
            .and_event_data_contains("data", "CONNECT")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_tcp_data",
                    "data": "220 NetGet FTP Server\r\n"
                }
            ]))
            .expect_calls(1)
            .and()
    }))
    .await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Send CONNECT and verify FTP greeting
    println!("Connecting TCP client...");
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP client connected");

    // Send CONNECT
    println!("Sending: CONNECT");
    stream.write_all(b"CONNECT\r\n").await?;
    stream.flush().await?;

    // Read greeting
    let mut buffer = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buffer[..n]);
            println!("Received: {}", response.trim());

            assert!(
                response.starts_with("220"),
                "Expected FTP 220 greeting, got: {}",
                response
            );
            println!("✓ FTP greeting test passed");
        }
        Ok(Ok(_)) => return Err("Connection closed without greeting".into()),
        Ok(Err(e)) => return Err(format!("Read error: {}", e).into()),
        Err(_) => return Err("Greeting timeout".into()),
    }

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test passed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_ftp_user_command() -> E2EResult<()> {
    println!("\n=== E2E Test: FTP USER Command ===");

    // PROMPT: Tell the LLM to respond to USER command
    let prompt = "listen on port {AVAILABLE_PORT} via ftp. When you receive 'USER' command, respond with '331 Password required\\r\\n'";

    // Start the server with mocks
    let server = helpers::start_netget_server(NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: User command interpretation (start server)
            .on_instruction_containing("ftp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "FTP server that responds to USER with 331 password required"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: TCP data received event (send 331 response when USER command received)
            .on_event("tcp_data_received")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_tcp_data",
                    "data": "331 Password required\r\n"
                }
            ]))
            .expect_calls(1)
            .and()
    }))
    .await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Send USER command and verify response
    println!("Connecting TCP client...");
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP client connected");

    // Send USER command
    println!("Sending: USER anonymous");
    stream.write_all(b"USER anonymous\r\n").await?;
    stream.flush().await?;

    // Read response
    let mut buffer = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buffer[..n]);
            println!("Received: {}", response.trim());

            assert!(
                response.starts_with("331"),
                "Expected FTP 331 response, got: {}",
                response
            );
            println!("✓ FTP USER command test passed");
        }
        Ok(Ok(_)) => return Err("Connection closed without response".into()),
        Ok(Err(e)) => return Err(format!("Read error: {}", e).into()),
        Err(_) => return Err("USER response timeout".into()),
    }

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test passed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_ftp_pwd_command() -> E2EResult<()> {
    println!("\n=== E2E Test: FTP PWD Command ===");

    // PROMPT: Tell the LLM to respond to PWD command
    let prompt = "listen on port {AVAILABLE_PORT} via ftp. When you receive 'PWD' command, respond with '257 \"/home/user\"\\r\\n'";

    // Start the server with mocks
    let server = helpers::start_netget_server(NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: User command interpretation (start server)
            .on_instruction_containing("ftp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "FTP server that responds to PWD with 257 current directory"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: TCP data received event (send 257 response when PWD command received)
            .on_event("tcp_data_received")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_tcp_data",
                    "data": "257 \"/home/user\"\r\n"
                }
            ]))
            .expect_calls(1)
            .and()
    }))
    .await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Send PWD command and verify response
    println!("Connecting TCP client...");
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP client connected");

    // Send PWD command
    println!("Sending: PWD");
    stream.write_all(b"PWD\r\n").await?;
    stream.flush().await?;

    // Read response
    let mut buffer = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buffer[..n]);
            println!("Received: {}", response.trim());

            assert!(
                response.starts_with("257"),
                "Expected FTP 257 response, got: {}",
                response
            );
            println!("✓ FTP PWD command test passed");
        }
        Ok(Ok(_)) => return Err("Connection closed without response".into()),
        Ok(Err(e)) => return Err(format!("Read error: {}", e).into()),
        Err(_) => return Err("PWD response timeout".into()),
    }

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test passed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_simple_echo() -> E2EResult<()> {
    println!("\n=== E2E Test: Simple Echo Server ===");

    // PROMPT: Tell the LLM to echo back with ACK prefix
    let prompt = "listen on port {AVAILABLE_PORT} via tcp. When you receive any data, reply with 'ACK: ' followed by the received data";

    // Start the server with mocks
    let server = helpers::start_netget_server(NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: TCP data received event (echo with ACK prefix) - MUST BE FIRST (most specific)
            .on_event("tcp_data_received")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_tcp_data",
                    "data": "ACK: Hello, LLM!"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: User command interpretation (start server) - MUST BE SECOND (less specific)
            .on_instruction_containing("tcp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "TCP echo server that prefixes ACK: to received data"
                }
            ]))
            .expect_calls(1)
            .and()
    }))
    .await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Send data and verify echo response
    println!("Connecting TCP client...");
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP client connected");

    // Send test data
    let test_message = "Hello, LLM!";
    println!("Sending: {}", test_message);
    stream.write_all(test_message.as_bytes()).await?;
    stream.flush().await?;

    // Read response with timeout
    let mut buffer = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buffer[..n]);
            println!("Received: {}", response);

            // Verify response format
            assert!(
                response.contains("ACK"),
                "Response should contain 'ACK', got: {}",
                response
            );
            assert!(
                response.contains(test_message),
                "Response should echo the message, got: {}",
                response
            );

            println!("✓ Echo test passed");
        }
        Ok(Ok(_)) => return Err("Connection closed without response".into()),
        Ok(Err(e)) => return Err(format!("Read error: {}", e).into()),
        Err(_) => return Err("Response timeout".into()),
    }

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test passed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_custom_response() -> E2EResult<()> {
    println!("\n=== E2E Test: Custom Response Server ===");

    // PROMPT: Tell the LLM to respond to PING with PONG
    let prompt = "listen on port {AVAILABLE_PORT} via tcp. When you receive 'PING', respond with 'PONG\\r\\n'";

    // Start the server with mocks
    let server = helpers::start_netget_server(NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: TCP data received event (send PONG when PING received) - MUST BE FIRST (most specific)
            .on_event("tcp_data_received")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_tcp_data",
                    "data": "PONG\r\n"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: User command interpretation (start server) - MUST BE SECOND (less specific)
            .on_instruction_containing("tcp")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "TCP",
                    "instruction": "TCP server that responds to PING with PONG"
                }
            ]))
            .expect_calls(1)
            .and()
    }))
    .await?;
    println!("Server started on port {}", server.port);

    // VALIDATION: Verify PING/PONG
    println!("Connecting TCP client...");
    let mut stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ TCP client connected");

    // Send PING
    println!("Sending: PING");
    stream.write_all(b"PING").await?;
    stream.flush().await?;

    // Read PONG
    let mut buffer = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buffer)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buffer[..n]);
            println!("Received: {}", response.trim());
            assert!(
                response.contains("PONG"),
                "Expected PONG response, got: {}",
                response
            );
            println!("✓ PING/PONG test passed");
        }
        Ok(Ok(_)) => return Err("Connection closed without response".into()),
        Ok(Err(e)) => return Err(format!("Read error: {}", e).into()),
        Err(_) => return Err("Response timeout".into()),
    }

    // Verify mock expectations were met
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test passed ===\n");
    Ok(())
}
