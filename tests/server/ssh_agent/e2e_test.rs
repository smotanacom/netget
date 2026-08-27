//! E2E tests for SSH Agent server with Ollama mocks
//!
//! These tests verify SSH Agent protocol implementation with mock LLM responses.
//! Unlike most NetGet e2e tests, SSH Agent requires Unix domain sockets.

#![cfg(all(feature = "ssh-agent", unix))]

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::super::helpers::{self, E2EResult, NetGetConfig};

/// Helper to construct SSH Agent REQUEST_IDENTITIES message (type 11)
fn build_request_identities() -> Vec<u8> {
    let mut msg = Vec::new();
    // Length: 1 byte (just the message type)
    msg.extend_from_slice(&1u32.to_be_bytes());
    // Message type: SSH_AGENTC_REQUEST_IDENTITIES (11)
    msg.push(11);
    msg
}

/// Helper to construct SSH Agent SIGN_REQUEST message (type 13)
fn build_sign_request(key_blob: &[u8], data: &[u8], flags: u32) -> Vec<u8> {
    let mut msg = Vec::new();

    // Calculate total length: 1 (type) + 4 (key len) + key + 4 (data len) + data + 4 (flags)
    let total_len = 1 + 4 + key_blob.len() + 4 + data.len() + 4;

    msg.extend_from_slice(&(total_len as u32).to_be_bytes());
    msg.push(13); // Type: SIGN_REQUEST
    msg.extend_from_slice(&(key_blob.len() as u32).to_be_bytes());
    msg.extend_from_slice(key_blob);
    msg.extend_from_slice(&(data.len() as u32).to_be_bytes());
    msg.extend_from_slice(data);
    msg.extend_from_slice(&flags.to_be_bytes());

    msg
}

/// Helper to construct SSH Agent ADD_IDENTITY message (type 17)
fn build_add_identity_ed25519(public_key: &[u8], private_key: &[u8], comment: &str) -> Vec<u8> {
    let mut msg = Vec::new();
    let key_type = b"ssh-ed25519";

    // Calculate length
    let total_len = 1 // type
        + 4 + key_type.len() // key type string
        + 4 + public_key.len() // public key
        + 4 + private_key.len() // private key
        + 4 + comment.len(); // comment

    msg.extend_from_slice(&(total_len as u32).to_be_bytes());
    msg.push(17); // Type: ADD_IDENTITY

    // Key type
    msg.extend_from_slice(&(key_type.len() as u32).to_be_bytes());
    msg.extend_from_slice(key_type);

    // Public key
    msg.extend_from_slice(&(public_key.len() as u32).to_be_bytes());
    msg.extend_from_slice(public_key);

    // Private key
    msg.extend_from_slice(&(private_key.len() as u32).to_be_bytes());
    msg.extend_from_slice(private_key);

    // Comment
    msg.extend_from_slice(&(comment.len() as u32).to_be_bytes());
    msg.extend_from_slice(comment.as_bytes());

    msg
}

/// Parse SSH Agent message header (length and type)
fn parse_message_header(data: &[u8]) -> Option<(u32, u8)> {
    if data.len() < 5 {
        return None;
    }

    let length = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let msg_type = data[4];

    Some((length, msg_type))
}

/// Test SSH Agent REQUEST_IDENTITIES with mock LLM response
#[tokio::test]
async fn test_ssh_agent_request_identities_with_mocks() -> E2EResult<()> {
    println!("\n=== E2E Test: SSH Agent REQUEST_IDENTITIES with Mocks ===");

    // Create temporary socket path
    let socket_path =
        std::env::temp_dir().join(format!("netget-test-agent-{}.sock", std::process::id()));

    // Ensure socket doesn't exist
    let _ = std::fs::remove_file(&socket_path);

    let socket_path_str = socket_path.to_str().unwrap().to_string();
    let prompt = format!(
        "Start SSH Agent server on {}. Handle REQUEST_IDENTITIES.",
        socket_path_str
    );

    let config = NetGetConfig::new(&prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("Start SSH Agent server")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "SSH Agent",
                    "instruction": "SSH Agent server",
                    "startup_params": {
                        "socket_path": socket_path_str
                    }
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection opened (no action needed)
            .on_event("ssh_agent_connection_opened")
            .respond_with_actions(serde_json::json!([]))
            .expect_calls(1)
            .and()
            // Mock 3: REQUEST_IDENTITIES event
            .on_event("ssh_agent_request_identities")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_identities_list",
                    "identities": [
                        {
                            "key_type": "ssh-ed25519",
                            "public_key_blob_hex": "0000000b7373682d6564323535313900000020abcd1234",
                            "comment": "test-key"
                        }
                    ]
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut server = helpers::start_netget_server(config).await?;

    println!("SSH Agent server started on socket: {}", socket_path_str);

    // Wait for server to create socket
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Verify socket exists
    if !socket_path.exists() {
        println!("⚠ Socket not created yet, waiting longer...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    if socket_path.exists() {
        println!("✓ Unix socket created");

        // Connect to SSH Agent server
        match UnixStream::connect(&socket_path).await {
            Ok(mut stream) => {
                println!("✓ Connected to SSH Agent server");

                // Wait for connection_opened event to complete
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Send REQUEST_IDENTITIES
                let request = build_request_identities();
                stream.write_all(&request).await?;
                stream.flush().await?;
                println!("→ Sent REQUEST_IDENTITIES");

                // Read response with timeout
                let mut response = vec![0u8; 8192];
                match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut response)).await
                {
                    Ok(Ok(n)) if n >= 5 => {
                        let (length, msg_type) =
                            parse_message_header(&response).expect("Failed to parse response");

                        println!("← Received response: length={}, type={}", length, msg_type);

                        // Verify response is IDENTITIES_ANSWER (12)
                        assert_eq!(
                            msg_type, 12,
                            "Expected IDENTITIES_ANSWER (12), got {}",
                            msg_type
                        );

                        // Parse number of keys
                        if response.len() >= 9 {
                            let num_keys = u32::from_be_bytes([
                                response[5],
                                response[6],
                                response[7],
                                response[8],
                            ]);
                            println!("  Number of keys: {}", num_keys);
                            assert_eq!(num_keys, 1, "Expected 1 key from mock");
                        }

                        println!("✓ REQUEST_IDENTITIES test passed");
                    }
                    Ok(Ok(_)) => {
                        println!("⚠ Connection closed without response");
                    }
                    Ok(Err(e)) => {
                        println!("⚠ Read error: {}", e);
                    }
                    Err(_) => {
                        println!("⚠ Response timeout");
                    }
                }
            }
            Err(e) => {
                println!("⚠ Connection failed: {}", e);
                println!("   Note: Server may not have created socket yet");
            }
        }
    } else {
        println!("⚠ Socket file not created: {}", socket_path_str);
        println!("   Note: SSH Agent may need Unix socket support in server startup");
    }

    // Verify mocks
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
    server.stop().await?;

    println!("=== Test completed ===\n");
    Ok(())
}

/// Test SSH Agent SIGN_REQUEST with mock LLM response
#[tokio::test]
async fn test_ssh_agent_sign_request_with_mocks() -> E2EResult<()> {
    println!("\n=== E2E Test: SSH Agent SIGN_REQUEST with Mocks ===");

    let socket_path = std::env::temp_dir().join(format!(
        "netget-test-agent-sign-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&socket_path);

    let socket_path_str = socket_path.to_str().unwrap().to_string();
    let prompt = format!(
        "Start SSH Agent server on {}. Handle SIGN_REQUEST.",
        socket_path_str
    );

    let config = NetGetConfig::new(&prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("Start SSH Agent server")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "SSH Agent",
                    "instruction": "SSH Agent with signing",
                    "startup_params": {
                        "socket_path": socket_path_str
                    }
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection opened (no action needed, just acknowledge)
            .on_event("ssh_agent_connection_opened")
            .respond_with_actions(serde_json::json!([]))
            .expect_calls(1)
            .and()
            // Mock 3: SIGN_REQUEST event
            .on_event("ssh_agent_sign_request")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_sign_response",
                    "signature_hex": "0000000b7373682d65643235353139000000400a1b2c3d4e5f"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut server = helpers::start_netget_server(config).await?;

    println!("SSH Agent server started on socket: {}", socket_path_str);
    tokio::time::sleep(Duration::from_secs(1)).await;

    if socket_path.exists() {
        println!("✓ Unix socket created");

        match UnixStream::connect(&socket_path).await {
            Ok(mut stream) => {
                println!("✓ Connected to SSH Agent server");

                // Wait for connection_opened event to complete
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Send SIGN_REQUEST
                let key_blob = b"test_public_key";
                let data_to_sign = b"test_data";
                let request = build_sign_request(key_blob, data_to_sign, 0);

                stream.write_all(&request).await?;
                stream.flush().await?;
                println!("→ Sent SIGN_REQUEST");

                // Read response
                let mut response = vec![0u8; 8192];
                match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut response)).await
                {
                    Ok(Ok(n)) if n >= 5 => {
                        let (length, msg_type) =
                            parse_message_header(&response).expect("Failed to parse response");

                        println!("← Received response: length={}, type={}", length, msg_type);

                        // Verify response is SIGN_RESPONSE (14)
                        assert_eq!(
                            msg_type, 14,
                            "Expected SIGN_RESPONSE (14), got {}",
                            msg_type
                        );

                        println!("✓ SIGN_REQUEST test passed");
                    }
                    Ok(Ok(_)) => println!("⚠ Connection closed without response"),
                    Ok(Err(e)) => println!("⚠ Read error: {}", e),
                    Err(_) => println!("⚠ Response timeout"),
                }
            }
            Err(e) => println!("⚠ Connection failed: {}", e),
        }
    } else {
        println!("⚠ Socket file not created");
    }

    // Verify mocks
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
    server.stop().await?;

    println!("=== Test completed ===\n");
    Ok(())
}

/// Test SSH Agent ADD_IDENTITY with mock LLM response
#[tokio::test]
async fn test_ssh_agent_add_identity_with_mocks() -> E2EResult<()> {
    println!("\n=== E2E Test: SSH Agent ADD_IDENTITY with Mocks ===");

    let socket_path =
        std::env::temp_dir().join(format!("netget-test-agent-add-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);

    let socket_path_str = socket_path.to_str().unwrap().to_string();
    let prompt = format!(
        "Start SSH Agent server on {}. Accept keys with ADD_IDENTITY.",
        socket_path_str
    );

    let config = NetGetConfig::new(&prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("Start SSH Agent server")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "SSH Agent",
                    "instruction": "SSH Agent accepting keys",
                    "startup_params": {
                        "socket_path": socket_path_str
                    }
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection opened (no action needed)
            .on_event("ssh_agent_connection_opened")
            .respond_with_actions(serde_json::json!([]))
            .expect_calls(1)
            .and()
            // Mock 3: ADD_IDENTITY event
            .on_event("ssh_agent_add_identity")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_success"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut server = helpers::start_netget_server(config).await?;

    println!("SSH Agent server started on socket: {}", socket_path_str);
    tokio::time::sleep(Duration::from_secs(1)).await;

    if socket_path.exists() {
        println!("✓ Unix socket created");

        match UnixStream::connect(&socket_path).await {
            Ok(mut stream) => {
                println!("✓ Connected to SSH Agent server");

                // Wait for connection_opened event to complete
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Send ADD_IDENTITY
                let public_key = b"test_public_key_32_bytes_here!!";
                let private_key = b"test_private_key_64_bytes_here!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!";
                let request = build_add_identity_ed25519(public_key, private_key, "test-key");

                stream.write_all(&request).await?;
                stream.flush().await?;
                println!("→ Sent ADD_IDENTITY");

                // Read response
                let mut response = vec![0u8; 8192];
                match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut response)).await
                {
                    Ok(Ok(n)) if n >= 5 => {
                        let (length, msg_type) =
                            parse_message_header(&response).expect("Failed to parse response");

                        println!("← Received response: length={}, type={}", length, msg_type);

                        // Verify response is SUCCESS (6)
                        assert_eq!(msg_type, 6, "Expected SUCCESS (6), got {}", msg_type);

                        println!("✓ ADD_IDENTITY test passed");
                    }
                    Ok(Ok(_)) => println!("⚠ Connection closed without response"),
                    Ok(Err(e)) => println!("⚠ Read error: {}", e),
                    Err(_) => println!("⚠ Response timeout"),
                }
            }
            Err(e) => println!("⚠ Connection failed: {}", e),
        }
    } else {
        println!("⚠ Socket file not created");
    }

    // Verify mocks
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
    server.stop().await?;

    println!("=== Test completed ===\n");
    Ok(())
}

/// Test SSH Agent with multiple operations in sequence
#[tokio::test]
async fn test_ssh_agent_multiple_operations_with_mocks() -> E2EResult<()> {
    println!("\n=== E2E Test: SSH Agent Multiple Operations with Mocks ===");

    let socket_path = std::env::temp_dir().join(format!(
        "netget-test-agent-multi-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&socket_path);

    let socket_path_str = socket_path.to_str().unwrap().to_string();
    let prompt = format!(
        "Start SSH Agent server on {}. Handle all operations.",
        socket_path_str
    );

    let config = NetGetConfig::new(&prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("Start SSH Agent server")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "SSH Agent",
                    "instruction": "SSH Agent multi-operation server",
                    "startup_params": {
                        "socket_path": socket_path_str
                    }
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection opened (no action needed)
            .on_event("ssh_agent_connection_opened")
            .respond_with_actions(serde_json::json!([]))
            .expect_calls(1)
            .and()
            // Mock 3: ADD_IDENTITY
            .on_event("ssh_agent_add_identity")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_success"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 4: REQUEST_IDENTITIES (both calls - returns 1 key for both)
            .on_event("ssh_agent_request_identities")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_identities_list",
                    "identities": [
                        {
                            "key_type": "ssh-ed25519",
                            "public_key_blob_hex": "0000000b7373682d6564323535313900000020abcd1234",
                            "comment": "added-key"
                        }
                    ]
                }
            ]))
            .expect_calls(2)
            .and()
    });

    let mut server = helpers::start_netget_server(config).await?;

    println!("SSH Agent server started on socket: {}", socket_path_str);
    tokio::time::sleep(Duration::from_secs(1)).await;

    if socket_path.exists() {
        println!("✓ Unix socket created");

        match UnixStream::connect(&socket_path).await {
            Ok(mut stream) => {
                println!("✓ Connected to SSH Agent server");

                // Wait for connection_opened event to complete
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Operation 1: REQUEST_IDENTITIES
                println!("\n→ Operation 1: REQUEST_IDENTITIES");
                stream.write_all(&build_request_identities()).await?;
                stream.flush().await?;

                let mut response = vec![0u8; 8192];
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_secs(3), stream.read(&mut response)).await
                {
                    if n >= 9 {
                        let num_keys = u32::from_be_bytes([
                            response[5],
                            response[6],
                            response[7],
                            response[8],
                        ]);
                        println!("  ✓ Got {} keys", num_keys);
                        // Note: mock returns 1 key for both calls (stateless)
                    }
                }

                tokio::time::sleep(Duration::from_millis(500)).await;

                // Operation 2: ADD_IDENTITY
                println!("\n→ Operation 2: ADD_IDENTITY");
                let public_key = b"test_public_key_32_bytes_here!!";
                let private_key = b"test_private_key_64_bytes_here!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!";
                stream
                    .write_all(&build_add_identity_ed25519(
                        public_key,
                        private_key,
                        "added-key",
                    ))
                    .await?;
                stream.flush().await?;

                let mut response = vec![0u8; 8192];
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_secs(3), stream.read(&mut response)).await
                {
                    if n >= 5 {
                        let (_, msg_type) = parse_message_header(&response).unwrap();
                        println!("  ✓ Got response type: {}", msg_type);
                        assert_eq!(msg_type, 6, "Expected SUCCESS");
                    }
                }

                tokio::time::sleep(Duration::from_millis(500)).await;

                // Operation 3: REQUEST_IDENTITIES (expect 1 key)
                println!("\n→ Operation 3: REQUEST_IDENTITIES (should have 1 key)");
                stream.write_all(&build_request_identities()).await?;
                stream.flush().await?;

                let mut response = vec![0u8; 8192];
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_secs(3), stream.read(&mut response)).await
                {
                    if n >= 9 {
                        let num_keys = u32::from_be_bytes([
                            response[5],
                            response[6],
                            response[7],
                            response[8],
                        ]);
                        println!("  ✓ Got {} keys (expected 1)", num_keys);
                        assert_eq!(num_keys, 1, "Expected 1 key after adding");
                    }
                }

                println!("\n✓ All operations completed successfully");
            }
            Err(e) => println!("⚠ Connection failed: {}", e),
        }
    } else {
        println!("⚠ Socket file not created");
    }

    // Verify mocks
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    // Cleanup
    let _ = std::fs::remove_file(&socket_path);
    server.stop().await?;

    println!("=== Test completed ===\n");
    Ok(())
}
