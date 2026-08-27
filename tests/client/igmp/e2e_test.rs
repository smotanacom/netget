//! E2E tests for IGMP client
//!
//! These tests verify IGMP client functionality by spawning the actual NetGet binary
//! and testing multicast group join/leave behavior as a black-box.
//! Test strategy: Use netget binary to start IGMP client, < 10 LLM calls total.

#[cfg(all(test, feature = "igmp"))]
mod igmp_client_tests {
    use crate::helpers::*;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// Test IGMP client can join a multicast group and receive data
    /// LLM calls: 2 (client startup, multicast join)
    #[tokio::test]
    async fn test_igmp_client_join_and_receive() -> E2EResult<()> {
        // Start IGMP client and instruct it to join a multicast group
        let multicast_group = "239.255.1.1";
        let multicast_port = 15000;

        let client_config = NetGetConfig::new(format!(
            "Start IGMP client on port {}. Join multicast group {} and log all received data.",
            multicast_port, multicast_group
        ))
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup (user command)
                .on_instruction_containing("Start IGMP client")
                .and_instruction_containing("239.255.1.1")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "igmp",
                        "protocol": "igmp",
                        "instruction": "Join multicast group 239.255.1.1 and listen"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected (igmp_connected event)
                .on_event("igmp_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "join_multicast_group",
                        "multicast_addr": "239.255.1.1",
                        "interface_addr": "0.0.0.0"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: Data received (igmp_data_received event)
                .on_event("igmp_data_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to start and join the group
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client shows it's ready
        client.wait_for_any(&["IGMP"], 30).await;
        assert!(
            client.output_contains("IGMP").await,
            "Client should show IGMP initialization. Output: {:?}",
            client.get_output().await
        );

        println!("✅ IGMP client initialized");

        // Send a multicast packet to the group
        let sender = UdpSocket::bind("0.0.0.0:0").await?;
        let test_data = b"HELLO_MULTICAST";
        let dest = format!("{}:{}", multicast_group, multicast_port);
        sender.send_to(test_data, &dest).await?;

        println!("✅ Sent multicast packet to {}", dest);

        // Give client time to receive and process
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Verify mock expectations were met
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test IGMP client can join and leave multicast groups
    /// LLM calls: 3 (client startup, join, leave)
    #[tokio::test]
    async fn test_igmp_client_join_and_leave() -> E2EResult<()> {
        let multicast_group = "239.255.1.2";

        let client_config = NetGetConfig::new(format!(
            "Start IGMP client. Join multicast group {}, then immediately leave it.",
            multicast_group
        ))
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup (user command)
                .on_instruction_containing("Start IGMP client")
                .and_instruction_containing("239.255.1.2")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "igmp",
                        "protocol": "igmp",
                        "instruction": "Join and then leave multicast group"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected (igmp_connected event)
                .on_event("igmp_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "join_multicast_group",
                        "multicast_addr": "239.255.1.2",
                        "interface_addr": "0.0.0.0"
                    },
                    {
                        "type": "leave_multicast_group",
                        "multicast_addr": "239.255.1.2",
                        "interface_addr": "0.0.0.0"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to process
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client processed the instructions
        assert_eq!(client.protocol, "igmp", "Client should be IGMP protocol");

        println!("✅ IGMP client joined and left multicast group");

        // Verify mock expectations were met
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test IGMP client can send multicast data
    /// LLM calls: 2 (client startup, send)
    #[tokio::test]
    async fn test_igmp_client_send_multicast() -> E2EResult<()> {
        let multicast_group = "239.255.1.3";
        let multicast_port = 15001;

        let client_config = NetGetConfig::new(format!(
            "Start IGMP client. Send the string 'TEST' to multicast group {} port {}.",
            multicast_group, multicast_port
        ))
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup (user command)
                .on_instruction_containing("Start IGMP client")
                .and_instruction_containing("239.255.1.3")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "igmp",
                        "protocol": "igmp",
                        "instruction": "Send TEST to multicast group"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected (igmp_connected event)
                .on_event("igmp_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_multicast",
                        "multicast_addr": "239.255.1.3",
                        "port": 15001,
                        "data": "54455354" // "TEST" in hex
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to send
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client is IGMP protocol
        assert_eq!(client.protocol, "igmp", "Client should be IGMP protocol");

        println!("✅ IGMP client sent multicast data");

        // Verify mock expectations were met
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;

        // Cleanup
        client.stop().await?;

        Ok(())
    }
}
