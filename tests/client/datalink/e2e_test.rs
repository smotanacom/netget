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
                        // Note: Use .on_any() for initial user command since instruction field is empty before client is created
                        .on_any()
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
                        // Mock 2: Frame injected event
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
                // Note: Use .on_any() for initial user command since instruction field is empty before client is created
                .on_any()
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
                // Mock 2: Frame captured event
                .on_event("datalink_frame_captured")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let client = start_netget_client(client_config).await?;

        // Give client time to start and capture frames
        tokio::time::sleep(Duration::from_millis(500)).await;

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
        // Start a DataLink client that injects ARP request and waits for reply
        let client_config = NetGetConfig::new(
            "Connect to eth0 via DataLink with promiscuous mode. Send ARP request for 192.168.1.1 and wait for reply."
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                // Note: Use .on_any() for initial user command since instruction field is empty before client is created
                .on_any()
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "eth0",
                        "protocol": "DataLink",
                        "startup_params": {
                            "interface": "eth0",
                            "promiscuous": true
                        },
                        "instruction": "Send ARP request and wait for reply"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Frame injected (ARP request sent)
                .on_event("datalink_frame_injected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: Frame captured (ARP reply received)
                .on_event("datalink_frame_captured")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let client = start_netget_client(client_config).await?;

        // Give client time to complete the inject-respond cycle
        tokio::time::sleep(Duration::from_secs(1)).await;

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
                        // Note: Use .on_any() for initial user command since instruction field is empty before client is created
                        .on_any()
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
                        // Mock 2: Frame injected, then disconnect
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
