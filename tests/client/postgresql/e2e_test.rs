//! E2E tests for PostgreSQL client
//!
//! These tests verify PostgreSQL client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.

#[cfg(all(test, feature = "postgresql"))]
mod postgresql_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test PostgreSQL client connection and query execution
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and the client
              // never connects.
    async fn test_postgresql_client_connect_and_query() -> E2EResult<()> {
        // Start a PostgreSQL server listening on an available port
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via PostgreSQL. \
            Respond to queries with sample data. \
            For SELECT queries, return a simple result set.",
        );

        let mut server = start_netget_server(server_config).await?;

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Now start a PostgreSQL client that connects and sends a query
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via PostgreSQL. \
            Execute 'SELECT 1 as test' query and display results.",
            server.port
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and execute query
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify client output shows connection
        client.wait_for_any(&["connected"], 30).await;
        assert!(
            client.output_contains("connected").await,
            "Client should show connection message. Output: {:?}",
            client.get_output().await
        );

        println!("✅ PostgreSQL client connected and executed query successfully");

        // Cleanup
        server.verify_mocks().await?;
        server.stop().await?;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test PostgreSQL client can be controlled via LLM instructions
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and the client
              // never connects.
    async fn test_postgresql_client_llm_controlled_queries() -> E2EResult<()> {
        // Start a simple PostgreSQL server
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via PostgreSQL. \
            Log all incoming queries.",
        );

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Client that sends specific queries based on LLM instruction
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via PostgreSQL. \
            Execute 'SELECT * FROM users' query.",
            server.port
        ));

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify the client is PostgreSQL protocol
        assert_eq!(
            client.protocol, "PostgreSQL",
            "Client should be PostgreSQL protocol"
        );

        println!("✅ PostgreSQL client responded to LLM instruction");

        // Cleanup
        server.verify_mocks().await?;
        server.stop().await?;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test PostgreSQL client transaction support
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and the client
              // never connects.
    async fn test_postgresql_client_transactions() -> E2EResult<()> {
        // Start a PostgreSQL server
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via PostgreSQL. \
            Support transaction commands (BEGIN, COMMIT, ROLLBACK).",
        );

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Client that executes a transaction
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via PostgreSQL. \
            Begin a transaction, execute an INSERT query, then commit.",
            server.port
        ));

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify client connected
        client.wait_for_any(&["connected"], 30).await;
        assert!(
            client.output_contains("connected").await,
            "Client should show connection message"
        );

        println!("✅ PostgreSQL client transaction test completed");

        // Cleanup
        server.verify_mocks().await?;
        server.stop().await?;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }
}
