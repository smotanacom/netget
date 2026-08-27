//! E2E tests for WHOIS client
//!
//! These tests verify WHOIS client functionality by connecting to real WHOIS servers
//! and querying well-known domains.
//! Test strategy: Query public WHOIS servers, < 5 LLM calls total.

#[cfg(all(test, feature = "whois"))]
mod whois_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test WHOIS client query to IANA root server
    /// LLM calls: 2 (client connection + response processing)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real whois.iana.org and requires --use-ollama.
              // Under default strict-mock CI mode the LLM call 500s immediately and this
              // assertion never passes. Follows the precedent set by tests/client/npm.
    async fn test_whois_query_example_com() -> E2EResult<()> {
        // Connect to IANA WHOIS server and query example.com
        let client_config = NetGetConfig::new(
            "Connect to whois.iana.org:43 via WHOIS. Query 'example.com' and show the registrar information."
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and receive response
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify client output shows connection
        client.wait_for_any(&["connected"], 30).await;
        assert!(
            client.output_contains("connected").await,
            "Client should show connection message. Output: {:?}",
            client.get_output().await
        );

        // Verify response received (WHOIS server sends "refer:" or domain info)
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("refer"))
                || output.iter().any(|l| l.contains("domain"))
                || output.iter().any(|l| l.contains("whois")),
            "Client should receive WHOIS response. Output: {:?}",
            output
        );

        println!("✅ WHOIS client queried example.com successfully");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test WHOIS client query to .com registry
    /// LLM calls: 2 (client connection + response processing)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real whois.verisign-grs.com and requires
              // --use-ollama. See test_whois_query_example_com for details.
    async fn test_whois_query_verisign() -> E2EResult<()> {
        // Connect to Verisign WHOIS server (authoritative for .com/.net)
        let client_config = NetGetConfig::new(
            "Connect to whois.verisign-grs.com:43 via WHOIS. Query 'example.com' and extract the registrar name."
        );

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and receive response
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify protocol is WHOIS
        assert_eq!(client.protocol, "WHOIS", "Client should be WHOIS protocol");

        // Verify response contains domain information
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("Domain Name"))
                || output.iter().any(|l| l.contains("Registrar"))
                || output.iter().any(|l| l.contains("EXAMPLE.COM")),
            "Client should receive domain information. Output: {:?}",
            output
        );

        println!("✅ WHOIS client queried Verisign successfully");

        // Cleanup
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test WHOIS client handles disconnection
    /// LLM calls: 2 (client connection + response)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: hits real whois.iana.org and requires --use-ollama.
              // See test_whois_query_example_com for details.
    async fn test_whois_auto_disconnect() -> E2EResult<()> {
        // WHOIS servers close the connection after sending response
        let client_config =
            NetGetConfig::new("Connect to whois.iana.org:43 via WHOIS and query 'com'.");

        let mut client = start_netget_client(client_config).await?;

        // Give client time to complete query cycle
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify client shows disconnection (WHOIS is one-shot)
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("disconnected"))
                || output.iter().any(|l| l.contains("closed"))
                || output.iter().any(|l| l.contains("complete")),
            "Client should show disconnection after response. Output: {:?}",
            output
        );

        println!("✅ WHOIS client handled auto-disconnection");

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
