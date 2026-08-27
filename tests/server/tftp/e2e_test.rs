//! TFTP server E2E tests
//!
//! Tests the TFTP server implementation with mock LLM responses.
//! Uses real TFTP protocol clients to verify server behavior.

use super::super::super::helpers::{self, start_netget_server, E2EResult, NetGetConfig};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Build a TFTP Read Request (RRQ) packet
fn build_rrq_packet(filename: &str, mode: &str) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&1u16.to_be_bytes()); // Opcode RRQ
    packet.extend_from_slice(filename.as_bytes());
    packet.push(0);
    packet.extend_from_slice(mode.as_bytes());
    packet.push(0);
    packet
}

/// Build a TFTP Write Request (WRQ) packet
fn build_wrq_packet(filename: &str, mode: &str) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&2u16.to_be_bytes()); // Opcode WRQ
    packet.extend_from_slice(filename.as_bytes());
    packet.push(0);
    packet.extend_from_slice(mode.as_bytes());
    packet.push(0);
    packet
}

/// Build a TFTP ACK packet
fn build_ack_packet(block_number: u16) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&4u16.to_be_bytes()); // Opcode ACK
    packet.extend_from_slice(&block_number.to_be_bytes());
    packet
}

/// Build a TFTP DATA packet
fn build_data_packet(block_number: u16, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&3u16.to_be_bytes()); // Opcode DATA
    packet.extend_from_slice(&block_number.to_be_bytes());
    packet.extend_from_slice(data);
    packet
}

/// Parse TFTP packet opcode
fn parse_opcode(packet: &[u8]) -> Option<u16> {
    if packet.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([packet[0], packet[1]]))
}

/// Parse TFTP DATA packet
fn parse_data_packet(packet: &[u8]) -> Option<(u16, Vec<u8>)> {
    if packet.len() < 4 {
        return None;
    }
    let opcode = u16::from_be_bytes([packet[0], packet[1]]);
    if opcode != 3 {
        return None;
    }
    let block_number = u16::from_be_bytes([packet[2], packet[3]]);
    let data = packet[4..].to_vec();
    Some((block_number, data))
}

/// Parse TFTP ACK packet
fn parse_ack_packet(packet: &[u8]) -> Option<u16> {
    if packet.len() < 4 {
        return None;
    }
    let opcode = u16::from_be_bytes([packet[0], packet[1]]);
    if opcode != 4 {
        return None;
    }
    Some(u16::from_be_bytes([packet[2], packet[3]]))
}

/// Parse TFTP ERROR packet
fn parse_error_packet(packet: &[u8]) -> Option<(u16, String)> {
    if packet.len() < 5 {
        return None;
    }
    let opcode = u16::from_be_bytes([packet[0], packet[1]]);
    if opcode != 5 {
        return None;
    }
    let error_code = u16::from_be_bytes([packet[2], packet[3]]);
    let msg = String::from_utf8_lossy(&packet[4..packet.len() - 1]).to_string();
    Some((error_code, msg))
}

#[tokio::test]
async fn test_tftp_read_request_with_mocks() -> E2EResult<()> {
    let test_file_content = b"Hello from TFTP server!";
    let test_file_hex = hex::encode(test_file_content);

    let config = NetGetConfig::new("listen on port {AVAILABLE_PORT} via tftp. Serve file test.txt")
        .with_mock(|mock| {
            mock
                // Mock startup instruction
                .on_instruction_containing("listen")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TFTP",
                        "instruction": "Serve file test.txt"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock tftp_read_request event
                .on_event("tftp_read_request")
                .and_event_data_contains("filename", "test.txt")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_tftp_data",
                        "block_number": 1,
                        "data_hex": test_file_hex,
                        "is_final": true
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let mut server = start_netget_server(config).await?;
    let server_addr = format!("127.0.0.1:{}", server.port);

    // Create UDP client socket
    let client = UdpSocket::bind("127.0.0.1:0").await?;

    // Send RRQ (Read Request)
    let rrq_packet = build_rrq_packet("test.txt", "octet");
    client.send_to(&rrq_packet, &server_addr).await?;

    // Receive DATA packet (block 1)
    let mut buffer = vec![0u8; 516];
    let (n, peer_addr) = timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await??;

    let (block_number, data) =
        parse_data_packet(&buffer[..n]).expect("Failed to parse DATA packet");

    assert_eq!(block_number, 1, "Expected block 1");
    assert_eq!(data, test_file_content, "File content mismatch");
    assert!(data.len() < 512, "Final block should be < 512 bytes");

    // Send ACK for block 1 to the TID port
    let ack_packet = build_ack_packet(1);
    client.send_to(&ack_packet, peer_addr).await?;

    // Verify mocks were called as expected
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    Ok(())
}

#[tokio::test]
async fn test_tftp_write_request_with_mocks() -> E2EResult<()> {
    let test_file_content = b"Upload this to TFTP server";

    let config = NetGetConfig::new("listen on port {AVAILABLE_PORT} via tftp. Accept file uploads")
        .with_mock(|mock| {
            mock
                // Mock startup instruction
                .on_instruction_containing("listen")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TFTP",
                        "instruction": "Accept file uploads"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock tftp_write_request event - respond with ACK 0
                .on_event("tftp_write_request")
                .and_event_data_contains("filename", "upload.txt")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_tftp_ack",
                        "block_number": 0
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock tftp_data_block event - acknowledge received data
                .on_event("tftp_data_block")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_tftp_ack",
                        "block_number": 1
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let mut server = start_netget_server(config).await?;
    let server_addr = format!("127.0.0.1:{}", server.port);

    // Create UDP client socket
    let client = UdpSocket::bind("127.0.0.1:0").await?;

    // Send WRQ (Write Request)
    let wrq_packet = build_wrq_packet("upload.txt", "octet");
    client.send_to(&wrq_packet, &server_addr).await?;

    // Receive ACK 0 (server ready to receive)
    let mut buffer = vec![0u8; 516];
    let (n, peer_addr) = timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await??;

    let ack_block = parse_ack_packet(&buffer[..n]).expect("Failed to parse ACK packet");
    assert_eq!(ack_block, 0, "Expected ACK 0");

    // Send DATA block 1 (final block)
    let data_packet = build_data_packet(1, test_file_content);
    client.send_to(&data_packet, peer_addr).await?;

    // Receive ACK 1
    let (n, _) = timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await??;
    let ack_block = parse_ack_packet(&buffer[..n]).expect("Failed to parse ACK packet");
    assert_eq!(ack_block, 1, "Expected ACK 1");

    // Verify mocks were called as expected
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    Ok(())
}

#[tokio::test]
async fn test_tftp_file_not_found_with_mocks() -> E2EResult<()> {
    let config =
        NetGetConfig::new("listen on port {AVAILABLE_PORT} via tftp. Only serve existing files")
            .with_mock(|mock| {
                mock
                    // Mock startup instruction
                    .on_instruction_containing("listen")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "TFTP",
                            "instruction": "Only serve existing files"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock tftp_read_request event - respond with error
                    .on_event("tftp_read_request")
                    .and_event_data_contains("filename", "nonexistent.txt")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "send_tftp_error",
                            "error_code": 1,
                            "error_message": "File not found"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

    let mut server = start_netget_server(config).await?;
    let server_addr = format!("127.0.0.1:{}", server.port);

    // Create UDP client socket
    let client = UdpSocket::bind("127.0.0.1:0").await?;

    // Send RRQ for nonexistent file
    let rrq_packet = build_rrq_packet("nonexistent.txt", "octet");
    client.send_to(&rrq_packet, &server_addr).await?;

    // Receive ERROR packet
    let mut buffer = vec![0u8; 516];
    let (n, _) = timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await??;

    let (error_code, error_msg) =
        parse_error_packet(&buffer[..n]).expect("Failed to parse ERROR packet");

    assert_eq!(error_code, 1, "Expected error code 1 (File not found)");
    assert_eq!(error_msg, "File not found");

    // Verify mocks were called as expected
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    Ok(())
}

#[tokio::test]
async fn test_tftp_multi_block_transfer_with_mocks() -> E2EResult<()> {
    // Create a file with 1024 bytes (requires 2 blocks)
    let block1_data = vec![0x41u8; 512]; // 'A' repeated 512 times
    let block2_data = vec![0x42u8; 512]; // 'B' repeated 512 times
    let block1_hex = hex::encode(&block1_data);
    let block2_hex = hex::encode(&block2_data);

    let config = NetGetConfig::new("listen on port {AVAILABLE_PORT} via tftp. Serve large files")
        .with_mock(|mock| {
            mock
                // Mock startup instruction
                .on_instruction_containing("listen")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "TFTP",
                        "instruction": "Serve large files"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock tftp_read_request event - send first block
                .on_event("tftp_read_request")
                .and_event_data_contains("filename", "large.bin")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_tftp_data",
                        "block_number": 1,
                        "data_hex": block1_hex,
                        "is_final": false
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock tftp_ack_received event - send second (final) block
                .on_event("tftp_ack_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_tftp_data",
                        "block_number": 2,
                        "data_hex": block2_hex,
                        "is_final": true
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let mut server = start_netget_server(config).await?;
    let server_addr = format!("127.0.0.1:{}", server.port);

    // Create UDP client socket
    let client = UdpSocket::bind("127.0.0.1:0").await?;

    // Send RRQ
    let rrq_packet = build_rrq_packet("large.bin", "octet");
    client.send_to(&rrq_packet, &server_addr).await?;

    let mut buffer = vec![0u8; 516];
    let mut received_data = Vec::new();

    // Receive block 1
    let (n, peer_addr) = timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await??;
    let (block_number, data) =
        parse_data_packet(&buffer[..n]).expect("Failed to parse DATA packet");
    assert_eq!(block_number, 1);
    assert_eq!(data.len(), 512);
    received_data.extend_from_slice(&data);

    // Send ACK 1
    let ack_packet = build_ack_packet(1);
    client.send_to(&ack_packet, peer_addr).await?;

    // Receive block 2 (final)
    let (n, _) = timeout(Duration::from_secs(5), client.recv_from(&mut buffer)).await??;
    let (block_number, data) =
        parse_data_packet(&buffer[..n]).expect("Failed to parse DATA packet");
    assert_eq!(block_number, 2);
    assert_eq!(data.len(), 512);
    received_data.extend_from_slice(&data);

    // Send ACK 2
    let ack_packet = build_ack_packet(2);
    client.send_to(&ack_packet, peer_addr).await?;

    // Verify total received data
    assert_eq!(received_data.len(), 1024);
    assert_eq!(&received_data[..512], &block1_data[..]);
    assert_eq!(&received_data[512..], &block2_data[..]);

    // Verify mocks were called as expected
    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;

    Ok(())
}
