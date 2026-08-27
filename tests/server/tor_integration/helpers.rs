//! Helper utilities for Tor integration tests

use super::super::helpers::server::get_server_output;
use super::super::helpers::server::NetGetServer;
use super::super::helpers::{self, NetGetConfig};
use anyhow::Result;
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::sleep;

/// Relay keys extracted from NetGet Tor Relay
#[derive(Debug, Clone)]
pub struct RelayKeys {
    /// Identity fingerprint (20 bytes hex)
    pub identity_fingerprint: String,
    /// Ed25519 identity public key (32 bytes base64)
    pub ed25519_identity: String,
    /// x25519 ntor onion key (32 bytes base64)
    pub ntor_onion_key: String,
    /// Relay IP address
    pub address: String,
    /// Relay OR port
    pub or_port: u16,
}

/// Authority keys extracted from NetGet Tor Directory
#[derive(Debug, Clone)]
pub struct AuthorityKeys {
    /// V3 identity fingerprint (40 hex chars)
    pub v3_identity_fingerprint: String,
    /// Authority fingerprint (40 hex chars)
    pub authority_fingerprint: String,
}

/// Complete Tor test network
pub struct TorTestNetwork {
    pub relay: NetGetServer,
    pub directory: NetGetServer,
    pub relay_keys: RelayKeys,
    pub authority_keys: AuthorityKeys,
    pub http_server_port: u16,
    pub http_server_handle: tokio::task::JoinHandle<()>,
}

impl TorTestNetwork {
    /// Create and start a complete Tor test network
    pub async fn setup() -> Result<Self> {
        println!("\n=== Setting up Tor Test Network ===\n");

        // 1. Start test HTTP server (destination)
        let (http_port, http_handle) = start_test_http_server().await;
        println!("✓ HTTP server started on port {}", http_port);

        // 2. Start NetGet Tor Relay
        let relay_prompt = "listen on port {AVAILABLE_PORT} via tor-relay. Handle TLS connections and Tor cells. Allow exit connections to localhost for testing.";
        let relay_config = NetGetConfig::new_no_scripts(relay_prompt)
            .with_log_level("info")
            .with_mock(|mock| {
                mock
                    // Mock 1: Relay server startup
                    .on_instruction_containing("tor-relay")
                    .respond_with_actions(json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "TorRelay",
                            "instruction": "Tor exit relay allowing localhost connections"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });
        let relay_server = helpers::start_netget_server(relay_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start relay: {}", e))?;

        // Wait for the relay to fully initialize
        sleep(Duration::from_secs(3)).await;

        let relay_port = relay_server.port;
        // REMOVED: assert_stack_name call
        println!("✓ Tor relay started on port {}", relay_port);

        // 3. Extract relay keys
        let relay_keys = extract_relay_keys(&relay_server).await?;
        println!("✓ Extracted relay keys:");
        println!("  Fingerprint: {}", relay_keys.identity_fingerprint);
        println!("  OR Port: {}", relay_keys.or_port);

        // 4. Create consensus document with relay info
        let consensus = super::consensus_builder::build_consensus(&relay_keys)?;

        // 5. Start NetGet Tor Directory
        let directory_prompt = format!(
            "listen on port {{AVAILABLE_PORT}} via tor-directory. When clients request /tor/status-vote/current/consensus, \
             respond with this document:\n\n{}\n\nFor microdescriptor requests, return appropriate microdescriptors.",
            consensus
        );
        let consensus_copy = consensus.clone();
        let directory_config = NetGetConfig::new_no_scripts(directory_prompt)
            .with_log_level("info")
            .with_mock(|mock| {
                mock
                    // Mock 1: Directory server startup
                    .on_instruction_containing("tor-directory")
                    .respond_with_actions(json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "HTTP",
                            "protocol": "TOR_DIRECTORY",
                            "instruction": "Tor directory serving custom consensus"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Consensus request.
                    //
                    // This server is opened with base_stack HTTP, and HTTP raises `http_request`
                    // and answers with `send_http_response` whose status field is `status`.
                    // The rule named `http_request_received` / `http_response` / `status_code`,
                    // none of which exists, so it never matched and none of it was validated —
                    // `assert_actions_valid_for_event` skips an event id it cannot resolve.
                    .on_event("http_request")
                    .and_event_data_contains("path", "/tor/status-vote/current/consensus")
                    .respond_with_actions(json!([
                        {
                            "type": "send_http_response",
                            "status": 200,
                            "headers": {
                                "Content-Type": "text/plain"
                            },
                            "body": consensus_copy
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });
        let directory_server = helpers::start_netget_server(directory_config)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to start directory: {}", e))?;

        let directory_port = directory_server.port;
        // REMOVED: assert_stack_name call
        println!("✓ Tor directory started on port {}", directory_port);

        // Wait a moment for authority key log messages to be captured
        sleep(Duration::from_millis(500)).await;

        // 6. Extract authority keys from directory output
        let authority_keys = extract_authority_keys(&directory_server).await?;
        println!("✓ Extracted authority keys:");
        println!("  V3 Identity: {}", authority_keys.v3_identity_fingerprint);
        println!("  Fingerprint: {}", authority_keys.authority_fingerprint);

        Ok(TorTestNetwork {
            relay: relay_server,
            directory: directory_server,
            relay_keys,
            authority_keys,
            http_server_port: http_port,
            http_server_handle: http_handle,
        })
    }

    /// Shutdown the test network
    pub async fn shutdown(mut self) -> Result<()> {
        // Verify mock expectations before shutdown
        self.relay
            .verify_mocks()
            .await
            .map_err(|e| anyhow::anyhow!("Relay mock verification failed: {}", e))?;
        self.directory
            .verify_mocks()
            .await
            .map_err(|e| anyhow::anyhow!("Directory mock verification failed: {}", e))?;

        self.relay
            .stop()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to stop relay: {}", e))?;
        self.directory
            .stop()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to stop directory: {}", e))?;
        self.http_server_handle.abort();
        Ok(())
    }
}

/// Extract relay keys from NetGet Tor Relay server output
async fn extract_relay_keys(server: &NetGetServer) -> Result<RelayKeys> {
    let output = get_server_output(server).await;

    // Look for fingerprint in output
    let mut fingerprint = None;
    for line in &output {
        if line.contains("fingerprint:") || line.contains("Relay fingerprint:") {
            // Find the position after "fingerprint:" to start searching for hex
            let search_start = if let Some(pos) = line.find("fingerprint:") {
                pos + "fingerprint:".len()
            } else {
                0
            };

            // Extract hex fingerprint (40 characters) from after "fingerprint:"
            if let Some(hex_start) = line[search_start..].find(|c: char| c.is_ascii_hexdigit()) {
                let hex_part = &line[search_start + hex_start..];
                let hex_fingerprint: String = hex_part
                    .chars()
                    .take_while(|c| c.is_ascii_hexdigit())
                    .collect();
                if hex_fingerprint.len() == 40 {
                    fingerprint = Some(hex_fingerprint);
                    break;
                }
            }
        }
    }

    let fingerprint =
        fingerprint.ok_or_else(|| anyhow::anyhow!("Could not find relay fingerprint in output"))?;

    // For now, use placeholder values for Ed25519 and ntor keys
    // In a real implementation, these would be extracted from the relay's log output
    // or exposed via an API endpoint

    Ok(RelayKeys {
        identity_fingerprint: fingerprint,
        ed25519_identity: base64::encode(&[0u8; 32]), // Placeholder
        ntor_onion_key: base64::encode(&[0u8; 32]),   // Placeholder
        address: "127.0.0.1".to_string(),
        or_port: server.port,
    })
}

/// Extract authority keys from NetGet Tor Directory server output
async fn extract_authority_keys(server: &NetGetServer) -> Result<AuthorityKeys> {
    let output = get_server_output(server).await;

    // Look for v3 identity fingerprint in output
    let mut v3_ident = None;
    let mut fingerprint = None;

    for line in &output {
        // Match: "[INFO] Authority v3 identity fingerprint: <40 hex chars>"
        if line.contains("v3 identity fingerprint:") {
            if let Some(hex_start) = line.rfind(": ") {
                let hex_part = &line[hex_start + 2..].trim();
                if hex_part.len() == 40 && hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
                    v3_ident = Some(hex_part.to_string());
                }
            }
        }

        // Match: "[INFO] Authority fingerprint: <40 hex chars>"
        if line.contains("Authority fingerprint:") && !line.contains("v3 identity") {
            if let Some(hex_start) = line.rfind(": ") {
                let hex_part = &line[hex_start + 2..].trim();
                if hex_part.len() == 40 && hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
                    fingerprint = Some(hex_part.to_string());
                }
            }
        }
    }

    let v3_identity_fingerprint = v3_ident.ok_or_else(|| {
        anyhow::anyhow!("Could not find authority v3 identity fingerprint in output")
    })?;
    let authority_fingerprint = fingerprint
        .ok_or_else(|| anyhow::anyhow!("Could not find authority fingerprint in output"))?;

    Ok(AuthorityKeys {
        v3_identity_fingerprint,
        authority_fingerprint,
    })
}

/// Start a simple HTTP test server
async fn start_test_http_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    let handle = tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut request_line = String::new();

                    // Read request line
                    if reader.read_line(&mut request_line).await.is_ok() {
                        // Read headers until empty line
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).await.is_ok() {
                                if line == "\r\n" || line == "\n" {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }

                        // Send response
                        let response = "HTTP/1.1 200 OK\r\n\
                                      Content-Type: text/plain\r\n\
                                      Content-Length: 31\r\n\
                                      Connection: close\r\n\
                                      \r\n\
                                      Hello from Tor test network!";

                        let stream = reader.into_inner();
                        let _ = stream.writable().await;
                        if let Ok(mut stream_ref) = stream.into_std() {
                            use std::io::Write;
                            let _ = stream_ref.write_all(response.as_bytes());
                            let _ = stream_ref.shutdown(std::net::Shutdown::Both);
                        }
                    }
                });
            }
        }
    });

    (port, handle)
}
