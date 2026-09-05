//! E2E tests for DataLink client
//!
//! These tests verify DataLink client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.
//! Test strategy: Mock frame injection and capture, < 10 LLM calls total.

#[cfg(all(test, feature = "datalink"))]
mod datalink_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test DataLink client frame injection
    /// LLM calls: 2 (client startup, frame injected event)
    #[tokio::test]
    async fn test_datalink_client_inject_frame_with_mocks() -> E2EResult<()> {
        // Start a DataLink client that injects an ARP frame
        let client_config =
            NetGetConfig::new("Connect to lo0 via DataLink. Inject an ARP request for 10.0.0.2")
                .with_mock(|mock| {
                    mock
                        // Mock 1: Client startup (user command)
                        // Matches the initial user command only. It used to be `.on_any()`,
                        // which -- being the first rule, and rules are first-match-wins --
                        // also swallowed every network event: once this client started
                        // raising a connected event, that event was answered with another
                        // `open_client` and the mock counted eleven calls.
                        .on_instruction_containing("DataLink")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "open_client",
                                "remote_addr": "lo0",
                                "protocol": "DataLink",
                                "startup_params": {
                                    "interface": "lo0",
                                    "promiscuous": false
                                },
                                "instruction": "Inject ARP request for 10.0.0.2"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // Mock 2: the capture is open -- inject the frame the
                        // instruction asks for. Nothing else triggers an injection.
                        .on_event("datalink_connected")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "inject_frame",
                                "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // Mock 3: Frame injected event
                        .on_event("datalink_frame_injected")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "wait_for_more"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                });

        let client = start_netget_client(client_config).await?;

        // Give client time to start and inject frame
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify client output shows connection
        client.wait_for_any(&["DataLink", "datalink"], 30).await;
        assert!(
            client.output_contains("DataLink").await || client.output_contains("datalink").await,
            "Client should show DataLink protocol. Output: {:?}",
            client.get_output().await
        );

        println!("✅ DataLink client injected frame successfully");

        // Note: Mock verification not possible in subprocess tests
        // The mock matching works correctly (see logs), but call tracking
        // happens inside the netget subprocess and can't be reported back

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test DataLink client with promiscuous mode capture
    /// LLM calls: 3 (client startup, frame injected, frame captured)
    #[tokio::test]
    async fn test_datalink_client_promiscuous_capture_with_mocks() -> E2EResult<()> {
        // Start a DataLink client in promiscuous mode
        let client_config = NetGetConfig::new(
            "Connect to lo0 via DataLink with promiscuous mode. Monitor all frames.",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                // Matches the initial user command only; see the note in the first test
                // for why `.on_any()` cannot be used now that a connected event exists.
                .on_instruction_containing("DataLink")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "lo0",
                        "protocol": "DataLink",
                        "startup_params": {
                            "interface": "lo0",
                            "promiscuous": true
                        },
                        "instruction": "Monitor all frames on lo0"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: the capture is open. Nothing to inject here -- this test is
                // about the receive side.
                .on_event("datalink_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: Frame captured event
                .on_event("datalink_frame_captured")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_at_least(1)
                .and()
        });

        let client = start_netget_client(client_config).await?;

        // Give the capture handle time to open before generating anything to capture.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Put some traffic on lo0. The test asserted that a frame was captured while
        // generating no frames at all, so it was waiting on whatever happened to cross
        // the loopback interface. A connection to a listener we own is deterministic.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = sock.write_all(b"datalink capture probe").await;
            }
        });
        for _ in 0..5 {
            if let Ok(mut sock) = tokio::net::TcpStream::connect(addr).await {
                use tokio::io::AsyncWriteExt;
                let _ = sock.write_all(b"datalink capture probe").await;
                let _ = sock.flush().await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        println!("✅ DataLink client in promiscuous mode processed mocked capture");

        // Note: Mock verification not possible in subprocess tests
        // The mock matching works correctly (see logs), but call tracking
        // happens inside the netget subprocess and can't be reported back

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test DataLink client inject and respond pattern
    /// LLM calls: 3 (client startup, frame injected, frame captured with response)
    #[tokio::test]
    async fn test_datalink_client_inject_and_respond_with_mocks() -> E2EResult<()> {
        // Both directions on one client: inject a frame, and capture one.
        //
        // This used to open `eth0` and wait for a real ARP reply from 192.168.1.1. There
        // is no eth0 on macOS and no host at that address in a test environment, so it
        // could not pass anywhere this suite runs -- it was a live-network test wearing a
        // mock's clothes. It runs on lo0 now, and the frame it captures is loopback
        // traffic this test generates rather than an ARP reply. What that still asserts is
        // the thing the client is responsible for: raising `datalink_frame_injected` for
        // what it sends and `datalink_frame_captured` for what arrives.
        let client_config = NetGetConfig::new(
            "Connect to lo0 via DataLink with promiscuous mode. Send a frame and watch for replies."
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                // Matches the initial user command only; see the note in the first test
                // for why `.on_any()` cannot be used now that a connected event exists.
                .on_instruction_containing("DataLink")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "lo0",
                        "protocol": "DataLink",
                        "startup_params": {
                            "interface": "lo0",
                            "promiscuous": true
                        },
                        "instruction": "Send a frame and watch for replies"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: capture is open -- send the frame.
                .on_event("datalink_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "inject_frame",
                        "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: Frame injected
                .on_event("datalink_frame_injected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_at_least(1)
                .and()
                // Mock 4: Frame captured
                .on_event("datalink_frame_captured")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_at_least(1)
                .and()
        });

        let client = start_netget_client(client_config).await?;

        // Let the capture handle open before generating anything to capture.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Traffic for the capture side to see.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = sock.write_all(b"datalink capture probe").await;
            }
        });
        for _ in 0..5 {
            if let Ok(mut sock) = tokio::net::TcpStream::connect(addr).await {
                use tokio::io::AsyncWriteExt;
                let _ = sock.write_all(b"datalink capture probe").await;
                let _ = sock.flush().await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        println!("✅ DataLink client completed inject-and-respond pattern");

        // Note: Mock verification not possible in subprocess tests
        // The mock matching works correctly (see logs), but call tracking
        // happens inside the netget subprocess and can't be reported back

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test DataLink client disconnect
    /// LLM calls: 2 (client startup, disconnect action)
    #[tokio::test]
    async fn test_datalink_client_disconnect_with_mocks() -> E2EResult<()> {
        // Start a DataLink client and disconnect gracefully
        let client_config =
            NetGetConfig::new("Connect to lo0 via DataLink. Inject one frame then disconnect.")
                .with_mock(|mock| {
                    mock
                        // Mock 1: Client startup
                        // Matches the initial user command only. It used to be `.on_any()`,
                        // which -- being the first rule, and rules are first-match-wins --
                        // also swallowed every network event: once this client started
                        // raising a connected event, that event was answered with another
                        // `open_client` and the mock counted eleven calls.
                        .on_instruction_containing("DataLink")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "open_client",
                                "remote_addr": "lo0",
                                "protocol": "DataLink",
                                "startup_params": {
                                    "interface": "lo0",
                                    "promiscuous": false
                                },
                                "instruction": "Inject one frame then disconnect"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // Mock 2: the capture is open -- inject the frame.
                        .on_event("datalink_connected")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "inject_frame",
                                "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // Mock 3: Frame injected, then disconnect
                        .on_event("datalink_frame_injected")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "disconnect"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                });

        let client = start_netget_client(client_config).await?;

        // Give client time to inject and disconnect
        tokio::time::sleep(Duration::from_millis(500)).await;

        println!("✅ DataLink client injected frame and disconnected gracefully");

        // Note: Mock verification not possible in subprocess tests
        // The mock matching works correctly (see logs), but call tracking
        // happens inside the netget subprocess and can't be reported back

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }
}
