//! E2E tests for OpenID Connect client
//!
//! These tests verify OpenID Connect client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.
//!
//! Note: OIDC tests are limited because we cannot easily run a full OIDC provider.
//! We test initialization, discovery, and basic LLM interpretation.

#[cfg(all(test, feature = "openidconnect"))]
mod openid_connect_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test OpenID Connect client initialization and discovery
    /// LLM calls: 1 (client connection with discovery)
    ///
    /// This test verifies that the OIDC client can:
    /// 1. Initialize with a provider URL
    /// 2. Attempt discovery (will fail with public providers without credentials)
    /// 3. Handle errors gracefully
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real accounts.google.com /
              // example.com and requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and this
              // assertion never passes.
    async fn test_oidc_client_initialization() -> E2EResult<()> {
        // Try to connect to a well-known OIDC provider (Google)
        // This will fail authentication but should succeed in discovery
        let client_config = NetGetConfig::new(
            "Connect to https://accounts.google.com as OpenID Connect client. Discover the provider configuration."
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to initialize and discover
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client output shows OIDC protocol or discovery attempt
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("OpenID"))
                || output.iter().any(|l| l.contains("OIDC"))
                || output.iter().any(|l| l.contains("discovered")),
            "Client should show OpenID Connect protocol or discovery message. Output: {:?}",
            output
        );

        println!("✅ OpenID Connect client initialized and attempted discovery");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test OpenID Connect client can be configured with parameters
    /// LLM calls: 1 (client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real accounts.google.com /
              // example.com and requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and this
              // assertion never passes.
    async fn test_oidc_client_with_parameters() -> E2EResult<()> {
        // Connect with explicit client credentials
        let client_config = NetGetConfig::new(
            "Connect to https://accounts.google.com as OpenID Connect client with client_id=test-app-id"
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to initialize
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Verify the client is OpenIDConnect protocol
        assert!(
            client.protocol == "OpenIDConnect" || client.protocol.contains("OpenID"),
            "Client should be OpenIDConnect protocol, got: {}",
            client.protocol
        );

        println!("✅ OpenID Connect client configured with parameters");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test OpenID Connect client LLM flow interpretation
    /// LLM calls: 1 (client connection)
    ///
    /// This test verifies the LLM can interpret different OIDC flow instructions
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real accounts.google.com /
              // example.com and requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and this
              // assertion never passes.
    async fn test_oidc_client_flow_interpretation() -> E2EResult<()> {
        // Client instructed to use device code flow
        let client_config = NetGetConfig::new(
            "Connect to https://example.com as OpenID Connect client. Use device code flow for authentication."
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to process instruction
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Verify client recognized OIDC instruction
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("OpenID"))
                || output.iter().any(|l| l.contains("device"))
                || output.iter().any(|l| l.contains("flow")),
            "Client should recognize OIDC flow instruction. Output: {:?}",
            output
        );

        println!("✅ OpenID Connect client interpreted flow instruction");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test OpenID Connect client handles invalid provider URLs
    /// LLM calls: 1 (client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real accounts.google.com /
              // example.com and requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and this
              // assertion never passes.
    async fn test_oidc_client_invalid_provider() -> E2EResult<()> {
        // Try to connect to an invalid provider
        let client_config = NetGetConfig::new(
            "Connect to http://invalid-oidc-provider.local as OpenID Connect client",
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to fail gracefully
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client shows error or handles gracefully
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("ERROR"))
                || output.iter().any(|l| l.contains("Failed"))
                || output.iter().any(|l| l.contains("error")),
            "Client should show error for invalid provider. Output: {:?}",
            output
        );

        println!("✅ OpenID Connect client handled invalid provider gracefully");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(10).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test OpenID Connect client can disconnect cleanly
    /// LLM calls: 1 (client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real accounts.google.com /
              // example.com and requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and this
              // assertion never passes.
    async fn test_oidc_client_disconnect() -> E2EResult<()> {
        let client_config = NetGetConfig::new(
            "Connect to https://accounts.google.com as OpenID Connect client. Then disconnect.",
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and disconnect
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client process is still running or stopped cleanly
        // (The exact behavior depends on whether LLM executes disconnect action)
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("OpenID"))
                || output.iter().any(|l| l.contains("connect"))
                || output.iter().any(|l| l.contains("disconnect")),
            "Client should show OIDC connection activity. Output: {:?}",
            output
        );

        println!("✅ OpenID Connect client handled disconnect instruction");

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
