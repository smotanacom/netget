//! E2E tests for STUN client
//!
//! These tests verify STUN client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box with public STUN servers.

#[cfg(all(test, feature = "stun"))]
mod stun_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test STUN client discovering external address
    /// LLM calls: 1 (client connection and binding request)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real stun.l.google.com and requires
              // --use-ollama. Under default strict-mock CI mode the LLM call 500s
              // immediately and this assertion never passes. Follows the precedent
              // set by tests/client/npm.
    async fn test_stun_client_discover_external_address() -> E2EResult<()> {
        // Connect to Google's public STUN server
        let client_config = NetGetConfig::new(
            "Connect to stun.l.google.com:19302 via STUN. Send a binding request to discover my external IP address."
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and make binding request
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client output shows STUN protocol or external address discovery
        let output = client.get_output().await;

        client
            .wait_for_any(&["STUN", "external", "binding"], 30)
            .await;
        assert!(
            client.output_contains("STUN").await
                || client.output_contains("external").await
                || client.output_contains("binding").await,
            "Client should show STUN protocol or binding request message. Output: {:?}",
            output
        );

        println!("✅ STUN client discovered external address successfully");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test STUN client with alternative STUN server
    /// LLM calls: 1 (client connection and binding request)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real stun1.l.google.com and requires
              // --use-ollama. See test_stun_client_discover_external_address for details.
    async fn test_stun_client_alternative_server() -> E2EResult<()> {
        // Connect to alternative Google STUN server
        let client_config = NetGetConfig::new(
            "Connect to stun1.l.google.com:19302 via STUN and query my external address.",
        );

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify the client is STUN protocol
        assert_eq!(client.protocol, "STUN", "Client should be STUN protocol");

        println!("✅ STUN client connected to alternative server");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test STUN client handles connection and binding response
    /// LLM calls: 1 (client connection and binding request)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real stun.l.google.com and requires
              // --use-ollama. See test_stun_client_discover_external_address for details.
    async fn test_stun_client_binding_response() -> E2EResult<()> {
        // Test that STUN client can process binding responses
        let client_config = NetGetConfig::new(
            "Connect to stun.l.google.com:19302 via STUN. Send binding request and report the external IP:port discovered."
        );

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_secs(2)).await;

        // Check for evidence of binding response processing
        let output = client.get_output().await;

        // Should mention either "external", "address", "binding", or show an IP address pattern
        assert!(
            output.iter().any(|l| l.contains("external"))
                || output.iter().any(|l| l.contains("address"))
                || output.iter().any(|l| l.contains("binding"))
                || output.iter().any(|l| l.contains("discovered"))
                || output.iter().any(|l| l.contains("NAT")),
            "Client should show evidence of binding response. Output: {:?}",
            output
        );

        println!("✅ STUN client processed binding response");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }
}
