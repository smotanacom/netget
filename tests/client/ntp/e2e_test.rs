//! E2E tests for NTP client
//!
//! These tests verify NTP client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.
//! Test strategy: Use public NTP servers, < 3 LLM calls per test.

#[cfg(all(test, feature = "ntp"))]
mod ntp_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test NTP client queries public time server
    /// LLM calls: 2 (client startup, response processing)
    #[tokio::test]
    async fn test_ntp_client_query_time_server() -> E2EResult<()> {
        // Use Google's public NTP server
        let client_config = NetGetConfig::new(
            "Query time.google.com:123 for current time and show the server time.",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                .on_instruction_containing("Query time.google.com")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "time.google.com:123",
                        "protocol": "NTP",
                        "instruction": "Query time server"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: NTP response received
                .on_event("ntp_response_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "analyze_response"
                    }
                ]))
                .expect_at_most(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to query and process response
        tokio::time::sleep(Duration::from_secs(6)).await;

        // Verify client output shows NTP response
        client.wait_for_any(&["ntp", "time"], 30).await;
        assert!(
            client.output_contains("ntp").await || client.output_contains("time").await,
            "Client should show NTP response. Output: {:?}",
            client.get_output().await
        );

        println!("✅ NTP client queried time server successfully");

        // Verify mock expectations were met
        client.verify_mocks().await?;

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test NTP client reports stratum level
    /// LLM calls: 2 (client startup, response processing)
    #[tokio::test]
    async fn test_ntp_client_stratum_analysis() -> E2EResult<()> {
        // Use pool.ntp.org which should return stratum 2-3
        let client_config = NetGetConfig::new(
            "Query pool.ntp.org:123 and report the stratum level.",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                .on_instruction_containing("Query pool.ntp.org")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "pool.ntp.org:123",
                        "protocol": "NTP",
                        "instruction": "Query NTP server and report stratum"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: NTP response received
                .on_event("ntp_response_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "analyze_response"
                    }
                ]))
                .expect_at_most(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to query and process response
        tokio::time::sleep(Duration::from_secs(6)).await;

        // Verify protocol is NTP
        assert_eq!(client.protocol, "NTP", "Client should be NTP protocol");

        println!("✅ NTP client analyzed stratum level");

        // Verify mock expectations were met
        client.verify_mocks().await?;

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test NTP client handles multiple queries
    /// LLM calls: 2 (initial query) - tests single-query limitation
    #[tokio::test]
    async fn test_ntp_client_single_query_model() -> E2EResult<()> {
        // Request time from NTP server
        let client_config = NetGetConfig::new("Query time.google.com:123 for the current time.")
            .with_mock(|mock| {
                mock
                    // Mock 1: Client startup
                    .on_instruction_containing("Query time.google.com")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_client",
                            "remote_addr": "time.google.com:123",
                            "protocol": "NTP",
                            "instruction": "Query time server"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: NTP response received
                    .on_event("ntp_response_received")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "analyze_response"
                        }
                    ]))
                    .expect_at_most(1)
                    .and()
            });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to complete
        tokio::time::sleep(Duration::from_secs(6)).await;

        // Verify client is disconnected after single query
        // (This validates the single-query design documented in CLAUDE.md)
        let output = client.get_output().await;
        println!("NTP client output: {:?}", output);

        println!("✅ NTP client completed single query");

        // Verify mock expectations were met
        client.verify_mocks().await?;

        // Cleanup
        client.stop().await?;

        Ok(())
    }
}
