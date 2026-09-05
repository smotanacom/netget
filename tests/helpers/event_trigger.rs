//! Event triggering strategies for E2E example tests
//!
//! This module provides abstractions for how protocol events are triggered.
//! Different protocols require different methods to trigger their events:
//! - TCP: Opening a connection
//! - UDP: Sending a packet with correlation ID matching
//! - HTTP: Making an HTTP request
//! - Hardware: Requires physical devices (may fail in CI)

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

/// Result type for event trigger operations
pub type TriggerResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Defines how to trigger a specific event for testing
///
/// Events are categorized by their triggering mechanism:
/// - Connection events: Triggered by opening TCP/UDP connections
/// - Packet events: Triggered by sending specific protocol packets
/// - Protocol events: Triggered by protocol-specific client operations
/// - Hardware events: Require physical devices or elevated permissions
#[derive(Clone)]
pub enum EventTrigger {
    /// Event triggered automatically on server startup
    /// No external action needed - just starting the server triggers the event
    ServerStartup,

    /// Event triggered by opening a TCP connection
    /// Use for: tcp_connection_opened, http_request, ssh_connection_opened
    TcpConnect,

    /// Event triggered by sending a TCP packet with specific data
    /// Use for: protocol-specific events after connection is established
    TcpSend {
        /// Data to send (can be raw bytes or protocol-specific)
        data: Vec<u8>,
    },

    /// Event triggered by sending a UDP packet
    /// Critical: Use dynamic correlation ID matching for UDP protocols
    /// Use for: dns_query, ntp_request, stun_binding_request, dhcp_discover
    UdpPacket {
        /// Function to build the UDP packet
        /// The returned bytes should include a correlation ID that the mock will extract
        packet_data: Vec<u8>,
        /// JSON path to the correlation field in event data (e.g., "query_id" for DNS)
        correlation_field: &'static str,
    },

    /// Event triggered by making an HTTP request
    /// Use for: http_request, webdav_request, s3_request, etc.
    HttpRequest {
        /// HTTP method (GET, POST, PUT, DELETE, etc.)
        method: String,
        /// Request path (e.g., "/api/users")
        path: String,
        /// Optional request body
        body: Option<Vec<u8>>,
        /// Optional headers
        headers: Vec<(String, String)>,
    },

    /// Event triggered after a delay (timer-based events)
    /// Use for: scheduled_task_triggered, idle_timeout, etc.
    Timer {
        /// Delay in milliseconds before the event should trigger
        delay_ms: u64,
    },

    /// Event that requires hardware or elevated permissions
    /// Tests using this trigger type may fail in CI environments
    /// Use for: ble_device_connected, usb_device_attached, arp_request
    Hardware {
        /// Description of the hardware requirement
        description: &'static str,
    },
}

impl EventTrigger {
    /// Check if this trigger type is available in the current environment
    pub fn is_available(&self) -> bool {
        match self {
            EventTrigger::Hardware { .. } => {
                // Hardware events are generally not available in CI/sandboxed environments
                // Check for common CI environment variables
                std::env::var("CI").is_err()
                    && std::env::var("GITHUB_ACTIONS").is_err()
                    && std::env::var("GITLAB_CI").is_err()
            }
            _ => true,
        }
    }

    /// Get a human-readable description of this trigger
    pub fn description(&self) -> String {
        match self {
            EventTrigger::ServerStartup => "Server startup event (automatic)".to_string(),
            EventTrigger::TcpConnect => "TCP connection".to_string(),
            EventTrigger::TcpSend { data } => format!("TCP send ({} bytes)", data.len()),
            EventTrigger::UdpPacket {
                packet_data,
                correlation_field,
            } => {
                format!(
                    "UDP packet ({} bytes, correlation: {})",
                    packet_data.len(),
                    correlation_field
                )
            }
            EventTrigger::HttpRequest { method, path, .. } => {
                format!("HTTP {} {}", method, path)
            }
            EventTrigger::Timer { delay_ms } => format!("Timer ({}ms delay)", delay_ms),
            EventTrigger::Hardware { description } => {
                format!("Hardware: {}", description)
            }
        }
    }

    /// Execute this trigger against a server at the given address
    pub fn execute(
        &self,
        addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = TriggerResult<()>> + Send>> {
        let trigger = self.clone();
        Box::pin(async move {
            match trigger {
                EventTrigger::ServerStartup => {
                    // Nothing to do - event already triggered on startup
                    Ok(())
                }

                EventTrigger::TcpConnect => {
                    use tokio::net::TcpStream;
                    let _stream = TcpStream::connect(addr).await?;
                    // Connection established - event should trigger
                    // Keep connection alive briefly for event to process
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    Ok(())
                }

                EventTrigger::TcpSend { data } => {
                    use tokio::io::AsyncWriteExt;
                    use tokio::net::TcpStream;

                    let mut stream = TcpStream::connect(addr).await?;
                    stream.write_all(&data).await?;
                    stream.flush().await?;
                    // Keep connection alive briefly for event to process
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    Ok(())
                }

                EventTrigger::UdpPacket { packet_data, .. } => {
                    use tokio::net::UdpSocket;

                    // Bind to ephemeral port
                    let socket = UdpSocket::bind("127.0.0.1:0").await?;
                    socket.send_to(&packet_data, addr).await?;
                    // Wait briefly for response (we may not care about it for the test)
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Ok(())
                }

                EventTrigger::HttpRequest {
                    method,
                    path,
                    body,
                    headers,
                } => {
                    let client = reqwest::Client::new();
                    let url = format!("http://{}{}", addr, path);

                    let mut request = match method.to_uppercase().as_str() {
                        "GET" => client.get(&url),
                        "POST" => client.post(&url),
                        "PUT" => client.put(&url),
                        "DELETE" => client.delete(&url),
                        "PATCH" => client.patch(&url),
                        "HEAD" => client.head(&url),
                        _ => return Err(format!("Unsupported HTTP method: {}", method).into()),
                    };

                    for (key, value) in headers {
                        request = request.header(&key, &value);
                    }

                    if let Some(body_data) = body {
                        request = request.body(body_data);
                    }

                    // We don't care about the response, just that the request was made
                    let _ = request.send().await;
                    Ok(())
                }

                EventTrigger::Timer { delay_ms } => {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    Ok(())
                }

                EventTrigger::Hardware { description } => Err(format!(
                    "Hardware trigger not available: {}. \
                         This test requires physical hardware or elevated permissions.",
                    description
                )
                .into()),
            }
        })
    }
}

// Helper functions for building common protocol packets.
//
// A `///` here documented `build_dns_query` rather than the section, and the blank line between
// them made that non-obvious — `clippy::empty_line_after_doc_comments`, which is in the denied
// `suspicious` group and so fails the blocking gate at wider feature sets.

/// Build a minimal DNS query packet
/// Returns (packet_bytes, query_id)
pub fn build_dns_query(domain: &str, query_type: u16) -> (Vec<u8>, u16) {
    // Generate random query ID
    let query_id: u16 = rand::random();

    let mut packet = Vec::new();

    // Header (12 bytes)
    packet.extend_from_slice(&query_id.to_be_bytes()); // ID
    packet.extend_from_slice(&[0x01, 0x00]); // Flags: Standard query, recursion desired
    packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT: 1 question
    packet.extend_from_slice(&[0x00, 0x00]); // ANCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]); // NSCOUNT: 0
    packet.extend_from_slice(&[0x00, 0x00]); // ARCOUNT: 0

    // Question section
    for label in domain.split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0); // Root label

    packet.extend_from_slice(&query_type.to_be_bytes()); // QTYPE (e.g., A=1, TXT=16)
    packet.extend_from_slice(&[0x00, 0x01]); // QCLASS: IN

    (packet, query_id)
}

/// Build a minimal NTP request packet
/// Returns (packet_bytes, reference_timestamp)
pub fn build_ntp_request() -> (Vec<u8>, u64) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Simplified NTP packet (48 bytes)
    let mut packet = vec![0u8; 48];
    packet[0] = 0b00_100_011; // LI=0, VN=4, Mode=3 (client)

    (packet, timestamp)
}

/// Build a minimal STUN binding request
/// Returns (packet_bytes, transaction_id as hex string)
pub fn build_stun_request() -> (Vec<u8>, String) {
    let mut packet = Vec::new();

    // STUN header (20 bytes)
    packet.extend_from_slice(&[0x00, 0x01]); // Message Type: Binding Request
    packet.extend_from_slice(&[0x00, 0x00]); // Message Length: 0 (no attributes)
    packet.extend_from_slice(&[0x21, 0x12, 0xa4, 0x42]); // Magic Cookie

    // 12-byte transaction ID
    let mut transaction_id = [0u8; 12];
    for byte in &mut transaction_id {
        *byte = rand::random();
    }
    packet.extend_from_slice(&transaction_id);

    let tx_id_hex = hex::encode(&transaction_id);

    (packet, tx_id_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trigger_descriptions() {
        let triggers = vec![
            EventTrigger::ServerStartup,
            EventTrigger::TcpConnect,
            EventTrigger::TcpSend {
                data: vec![1, 2, 3],
            },
            EventTrigger::UdpPacket {
                packet_data: vec![1, 2, 3, 4],
                correlation_field: "query_id",
            },
            EventTrigger::HttpRequest {
                method: "GET".to_string(),
                path: "/api/test".to_string(),
                body: None,
                headers: vec![],
            },
            EventTrigger::Timer { delay_ms: 1000 },
            EventTrigger::Hardware {
                description: "Bluetooth adapter",
            },
        ];

        for trigger in triggers {
            let desc = trigger.description();
            assert!(!desc.is_empty(), "Trigger should have description");
            println!("{}", desc);
        }
    }

    #[test]
    fn test_dns_query_builder() {
        let (packet, query_id) = build_dns_query("example.com", 1);

        // Check header
        assert_eq!(packet.len(), 29); // 12 header + 17 question
        let packet_id = u16::from_be_bytes([packet[0], packet[1]]);
        assert_eq!(packet_id, query_id);
    }

    #[test]
    fn test_stun_request_builder() {
        let (packet, tx_id) = build_stun_request();

        assert_eq!(packet.len(), 20); // STUN header
        assert_eq!(tx_id.len(), 24); // 12 bytes as hex
    }
}
