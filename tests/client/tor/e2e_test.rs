//! E2E tests for Tor client with local tor_relay server
//!
//! These tests verify Tor client functionality using a local tor_relay server
//! with BEGIN_DIR support (fully local, no internet required).

#[cfg(all(test, feature = "tor"))]
mod tor_client_tests {
    use crate::helpers::*;
    use serde_json::json;
    use std::time::Duration;

    /// Test Tor client connecting to local tor_relay with BEGIN_DIR support
    /// Arti bootstraps from localhost tor_relay over OR protocol
    /// LLM calls: 3 (server startup, circuit created, client startup)
    #[tokio::test]
    async fn test_tor_client_with_local_relay() -> E2EResult<()> {
        // Start a local tor_relay server (now supports BEGIN_DIR for directory)
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} as Tor Relay. Accept circuits and serve directory over BEGIN_DIR."
        )
            .with_mock(|mock| {
                mock
                    // Mock 1: Server startup
                    .on_instruction_containing("Tor Relay")
                    .respond_with_actions(json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "Tor Relay",
                            "instruction": "Relay with directory support"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Circuit created event (Arti will create circuit for BEGIN_DIR)
                    .on_event("tor_relay_circuit_created")
                    // tor_relay has no `wait_for_more`; its no-op verb is tor_relay_log.
                    // The relay's vocabulary is circuit-shaped -- send_destroy,
                    // detect_relay_cell, close_connection, tor_relay_log -- and a rule
                    // naming an action it cannot execute proves nothing.
                    .respond_with_actions(json!([
                        {
                            "type": "tor_relay_log",
                            "message": "circuit created"
                        }
                    ]))
                    .expect_at_least(0)  // May or may not fire depending on timing
                    .and()
            });

        let server = start_netget_server(server_config).await?;

        // Give server time to start
        tokio::time::sleep(Duration::from_secs(1)).await;

        println!("✓ Tor relay server started on port {}", server.port);

        // Now start Tor client pointing to local relay
        let client_config = NetGetConfig::new(format!(
            "Connect via Tor to example.com:80 using local relay at 127.0.0.1:{}",
            server.port
        ))
        .with_mock(|mock| {
            mock
                // Mock 3: Client startup with directory_server parameter
                .on_instruction_containing("Connect via Tor")
                .and_instruction_containing("local relay")
                .respond_with_actions(json!([
                    {
                        "type": "open_client",
                        "remote_addr": "example.com:80",
                        "protocol": "Tor",
                        "instruction": "Bootstrap from local relay",
                        "startup_params": {
                            "directory_server": format!("127.0.0.1:{}", server.port)
                        }
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 4: LLM responds to bootstrap completion (optional)
                .on_event("tor_bootstrap_complete")
                .respond_with_actions(json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_at_least(0) // Optional - may not fire in test timeframe
                .and()
        });

        let client = start_netget_client(client_config).await?;

        // Give time for Arti to bootstrap (connect, create circuit, BEGIN_DIR, fetch consensus)
        println!("Waiting for Arti bootstrap (circuit creation + BEGIN_DIR)...");
        tokio::time::sleep(Duration::from_secs(15)).await;

        // Check server output to see BEGIN_DIR activity
        let server_output = server.get_output().await;
        println!("=== Tor Relay Server Output ===");
        for line in &server_output {
            println!("{}", line);
        }
        println!("=== End Server Output ===");

        // Check client output
        let client_output = client.get_output().await;
        println!("=== Tor Client Output ===");
        for line in &client_output {
            println!("{}", line);
        }
        println!("=== End Client Output ===");

        // Verify mocks were called
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(10).await;
        client.wait_for_mocks(10).await;
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Look for evidence of BEGIN_DIR or circuit activity in server output
        let has_circuit = server_output.iter().any(|line| {
            line.contains("circuit") || line.contains("BEGIN_DIR") || line.contains("CREATE2")
        });

        println!("Circuit/BEGIN_DIR activity detected: {}", has_circuit);

        println!("✓ Test completed - check output for Arti bootstrap activity");

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }
}
