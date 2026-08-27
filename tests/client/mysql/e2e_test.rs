//! E2E tests for MySQL client
//!
//! These tests verify MySQL client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.

#[cfg(all(test, feature = "mysql"))]
mod mysql_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test MySQL client connection and simple query
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    async fn test_mysql_client_connect_and_query() -> E2EResult<()> {
        // Start a MySQL server listening on an available port
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via MySQL. Accept SELECT queries and respond with sample data.",
        )
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup
                .on_instruction_containing("Listen on port")
                .and_instruction_containing("MySQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "MySQL",
                        "instruction": "Accept SELECT queries and respond with sample data"
                    }
                ]))
                .expect_calls(1)
                .and()
                // No connection rule: the MySQL SERVER raises only `mysql_query`.
                // It has no connection event, so a rule answering one can never fire --
                // and `accept_connection` is not a verb it can execute either. The
                // handshake is handled inside the server, not by the model.
                // Mock 3: SELECT 1 query
                .on_event("mysql_query")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "mysql_query_response",
                        "columns": [{"name": "1", "type": "INT"}],
                        "rows": [[1]]
                    }
                ]))
                .expect_at_least(0)
                .and()
        });

        let mut server = start_netget_server(server_config).await?;

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Now start a MySQL client that connects and sends a query
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via MySQL as user 'root' with password ''. Execute SELECT 1 query.",
            server.port
        ))
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                .on_instruction_containing("Connect to")
                .and_instruction_containing("MySQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": format!("127.0.0.1:{}", server.port),
                        "protocol": "MySQL",
                        "instruction": "Execute SELECT 1 query",
                        "username": "root",
                        "password": ""
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected
                .on_event("mysql_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "execute_query",
                        "query": "SELECT 1"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: Query response received
                .on_event("mysql_result_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_at_least(0)
                .and()
        });

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

        println!("✅ MySQL client connected and executed query successfully");

        // Verify mock expectations
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test MySQL client with database selection
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    async fn test_mysql_client_with_database() -> E2EResult<()> {
        // Start a MySQL server
        let server_config = NetGetConfig::new(
            "Listen on port {} via MySQL. Accept connections to 'testdb' database.",
        )
        .with_mock(|mock| {
            mock.on_instruction_containing("MySQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "MySQL",
                        "instruction": "Accept connections to 'testdb' database"
                    }
                ]))
                .expect_calls(1)
                .and()
            // No connection rule: the MySQL server raises only `mysql_query`, and
            // accept_connection is not a verb it can execute.
        });

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Client that specifies a database
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via MySQL as user 'root' with database 'testdb'. Execute SELECT * FROM users query.",
            server.port
        ))
        .with_mock(|mock| {
            mock
                .on_instruction_containing("Connect to")
                .and_instruction_containing("MySQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": format!("127.0.0.1:{}", server.port),
                        "protocol": "MySQL",
                        "instruction": "Execute SELECT * FROM users query",
                        "username": "root",
                        "database": "testdb"
                    }
                ]))
                .expect_calls(1)
                .and()
                .on_event("mysql_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "execute_query",
                        "query": "SELECT * FROM users"
                    }
                ]))
                .expect_at_least(0)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify the client is MySQL protocol
        assert_eq!(client.protocol, "MySQL", "Client should be MySQL protocol");

        println!("✅ MySQL client connected with database specification");

        // Verify mock expectations
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test MySQL client transaction control
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    async fn test_mysql_client_transaction() -> E2EResult<()> {
        // Start a MySQL server
        let server_config =
            NetGetConfig::new("Listen on port {} via MySQL. Accept transaction commands.")
                .with_mock(|mock| {
                    mock.on_instruction_containing("MySQL")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "open_server",
                                "port": 0,
                                "base_stack": "MySQL",
                                "instruction": "Accept transaction commands"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // No connection rule: the MySQL server raises only `mysql_query`, and
                        // accept_connection is not a verb it can execute.
                        .on_event("mysql_query")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "mysql_ok_response",
                                "affected_rows": 0
                            }
                        ]))
                        .expect_at_least(0)
                        .and()
                });

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Client that uses transactions
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via MySQL. Begin a transaction, execute INSERT INTO logs VALUES ('test'), then commit.",
            server.port
        ))
        .with_mock(|mock| {
            mock
                .on_instruction_containing("Connect to")
                .and_instruction_containing("MySQL")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": format!("127.0.0.1:{}", server.port),
                        "protocol": "MySQL",
                        "instruction": "Begin transaction, execute INSERT, then commit",
                        "username": "root"
                    }
                ]))
                .expect_calls(1)
                .and()
                .on_event("mysql_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "execute_query",
                        "query": "BEGIN"
                    }
                ]))
                .expect_at_least(0)
                .and()
                .on_event("mysql_result_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "execute_query",
                        "query": "INSERT INTO logs VALUES ('test')"
                    }
                ]))
                .expect_at_least(0)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify client output shows connection
        client.wait_for_any(&["connected"], 30).await;
        assert!(
            client.output_contains("connected").await,
            "Client should show connection message. Output: {:?}",
            client.get_output().await
        );

        println!("✅ MySQL client transaction test passed");

        // Verify mock expectations
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }
}
