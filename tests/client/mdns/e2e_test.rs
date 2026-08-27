//! E2E tests for mDNS client
//!
//! These tests verify mDNS client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.
//! Test strategy: Use netget binary to start mDNS client, < 5 LLM calls total.
//!
//! Note: These tests rely on built-in mDNS responders (Avahi on Linux, mDNSResponder on macOS).
//! Tests may not discover services if no mDNS services are available on the network.

#[cfg(all(test, feature = "mdns"))]
mod mdns_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test mDNS client initialization
    /// LLM calls: 1 (client startup)
    #[tokio::test]
    async fn test_mdns_client_initialization() -> E2EResult<()> {
        // Start mDNS client with instruction to browse for HTTP services with mocks
        let client_config = NetGetConfig::new(
            "Initialize mDNS client and browse for HTTP services (_http._tcp.local).",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup (user command)
                .on_instruction_containing("Initialize mDNS client")
                .and_instruction_containing("browse for HTTP services")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "local",
                        "protocol": "mDNS",
                        "instruction": "Browse for HTTP services"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to initialize
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client output shows initialization
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("mDNS"))
                || output.iter().any(|l| l.contains("initialized"))
                || output.iter().any(|l| l.contains("ready")),
            "Client should show mDNS initialization. Output: {:?}",
            output
        );

        println!("✅ mDNS client initialized successfully");

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

    /// Test mDNS client service discovery
    /// LLM calls: 2 (client startup, browse service)
    /// Note: This test may not find services if network has no mDNS-advertised services
    #[tokio::test]
    async fn test_mdns_client_service_discovery() -> E2EResult<()> {
        // Start mDNS client with mocks
        let client_config = NetGetConfig::new(
            "Initialize mDNS client and browse for any services (_services._dns-sd._udp.local). \
             Wait 10 seconds to collect service discovery responses, then report findings.",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup (user command)
                .on_instruction_containing("Initialize mDNS client")
                .and_instruction_containing("browse for any services")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "local",
                        "protocol": "mDNS",
                        "instruction": "Browse for services"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected event (browse for services)
                .on_event("mdns_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "browse_service",
                        "service_type": "_services._dns-sd._udp.local"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client significant time to discover services
        tokio::time::sleep(Duration::from_secs(12)).await;

        // Verify the client was initialized as mDNS protocol
        assert_eq!(client.protocol, "mDNS", "Client should be mDNS protocol");

        let output = client.get_output().await;

        // The test passes if:
        // 1. Client initialized successfully
        // 2. Client attempted to browse for services (even if none found)
        assert!(
            output.iter().any(|l| l.contains("mDNS"))
                || output.iter().any(|l| l.contains("browse"))
                || output.iter().any(|l| l.contains("service")),
            "Client should show mDNS service discovery activity. Output: {:?}",
            output
        );

        println!("✅ mDNS client performed service discovery");
        println!("Note: No services may be found if network has no active mDNS responders");

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

    /// Test mDNS client hostname resolution
    /// LLM calls: 2 (client startup, resolve hostname)
    #[tokio::test]
    async fn test_mdns_client_hostname_resolution() -> E2EResult<()> {
        // Start mDNS client with instruction to resolve a local hostname with mocks
        // Using localhost.local which should be resolvable on most systems
        let client_config = NetGetConfig::new(
            "Initialize mDNS client and resolve 'localhost.local' to an IP address.",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup (user command)
                .on_instruction_containing("Initialize mDNS client")
                .and_instruction_containing("resolve 'localhost.local'")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": "local",
                        "protocol": "mDNS",
                        "instruction": "Resolve localhost.local"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected event (resolve hostname)
                .on_event("mdns_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "resolve_hostname",
                        "hostname": "localhost.local"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to resolve
        tokio::time::sleep(Duration::from_secs(3)).await;

        let output = client.get_output().await;

        // Verify client attempted hostname resolution
        assert!(
            output.iter().any(|l| l.contains("resolve"))
                || output.iter().any(|l| l.contains("localhost"))
                || output.iter().any(|l| l.contains("mDNS")),
            "Client should show hostname resolution activity. Output: {:?}",
            output
        );

        println!("✅ mDNS client attempted hostname resolution");

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
