//! E2E tests for PostgreSQL client
//!
//! These tests verify PostgreSQL client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.

#[cfg(all(test, feature = "postgresql"))]
mod postgresql_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// The only running LLM-path coverage this client has.
    ///
    /// The other three tests in this file are `#[ignore]`d for want of mocks, so before this
    /// existed the PostgreSQL client's entire model-driven path — connect, query, react to the
    /// rows — was exercised by nothing. Only the zero-LLM command-channel test ran.
    ///
    /// What it pins is the **shape of the follow-up chain**, which is deliberately two turns
    /// deep and no more: `postgresql_connected` asks for a query, the rows come back as
    /// `postgresql_query_result`, and the model's answer to *that* is executed — but
    /// `apply_action` raises no event, so the chain terminates there by construction rather
    /// than by a depth counter. So the server must see **two** queries while the client sees
    /// **one** result event. One query would mean the follow-up was discarded (the defect
    /// found in the MySQL client); three result events would mean the chain had become
    /// unbounded.
    ///
    /// LLM calls: server 1 startup + 2 queries; client 1 startup + 1 connect + 1 result = 6.
    #[tokio::test]
    async fn a_query_result_drives_exactly_one_more_query() -> E2EResult<()> {
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via PostgreSQL. Answer SELECT queries.",
        )
        .with_mock(|mock| {
            mock.on_instruction_containing("Listen on port")
                .and_instruction_containing("PostgreSQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "PostgreSQL",
                        "instruction": "Answer SELECT queries"
                    }
                ]))
                .expect_calls(1)
                .and()
                // ONE rule branching on the event, not two rules on the same event: rules are
                // first-match-wins, so a second rule for the follow-up would report zero calls
                // while the first answered everything.
                .on_event("postgresql_query")
                .respond_with_actions_from_event(|e| {
                    let query = e["query"].as_str().unwrap_or("").to_uppercase();
                    let value = if query.contains("SECOND") { 2 } else { 1 };
                    serde_json::json!([
                        {
                            "type": "postgresql_query_response",
                            "columns": [{"name": "n", "type": "int4"}],
                            "rows": [[value]]
                        }
                    ])
                })
                // The assertion: the follow-up query reached the wire.
                .expect_calls(2)
                .and()
        });

        let mut server = start_netget_server(server_config).await?;
        server.wait_for_any(&["listening", "Running"], 30).await;

        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via PostgreSQL. Query, then query again.",
            server.port
        ))
        .with_mock(|mock| {
            mock.on_instruction_containing("Connect to")
                .and_instruction_containing("PostgreSQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": format!("127.0.0.1:{}", server.port),
                        "protocol": "PostgreSQL",
                        "instruction": "Query, then query again"
                    }
                ]))
                .expect_calls(1)
                .and()
                .on_event("postgresql_connected")
                .respond_with_actions(serde_json::json!([
                    { "type": "execute_query", "query": "SELECT 1 AS first" }
                ]))
                .expect_calls(1)
                .and()
                .on_event("postgresql_query_result")
                .respond_with_actions(serde_json::json!([
                    { "type": "execute_query", "query": "SELECT 2 AS second" }
                ]))
                // Exactly once. The follow-up runs but raises no event, so a second occurrence
                // would mean the chain no longer terminates.
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;
        client.wait_for_any(&["connected"], 30).await;

        server.wait_for_mocks(30).await;
        client.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        server.stop().await?;
        client.stop().await?;
        Ok(())
    }

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
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        server.stop().await?;
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
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
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        server.stop().await?;
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
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
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        server.stop().await?;
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }
}
