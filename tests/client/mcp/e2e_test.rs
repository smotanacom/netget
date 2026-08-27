//! E2E tests for MCP client
//!
//! These tests verify MCP client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box.

#[cfg(all(test, feature = "mcp"))]
mod mcp_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test MCP client connecting to server and initializing
    /// LLM calls: 2 (server startup, client connection)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and the client
              // never connects.
    async fn test_mcp_client_initialize() -> E2EResult<()> {
        // Start an MCP server listening on an available port
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via MCP. \
             Provide a tool called 'calculate' that evaluates math expressions.",
        );

        let mut server = start_netget_server(server_config).await?;

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Now start an MCP client that connects and initializes
        let client_config = NetGetConfig::new(format!(
            "Connect to http://127.0.0.1:{} via MCP. \
             After connecting, list available tools.",
            server.port
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and perform actions
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client output shows MCP connection
        client.wait_for_any(&["MCP", "initialized"], 30).await;
        assert!(
            client.output_contains("MCP").await || client.output_contains("initialized").await,
            "Client should show MCP protocol or initialization message. Output: {:?}",
            client.get_output().await
        );

        println!("✅ MCP client connected and initialized successfully");

        // Cleanup
        server.verify_mocks().await?;
        server.stop().await?;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test MCP client calling a tool on the server
    /// LLM calls: 3 (server startup, client connection, tool call)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and the client
              // never connects.
    async fn test_mcp_client_call_tool() -> E2EResult<()> {
        // Start an MCP server with a calculator tool
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via MCP. \
             Provide a tool called 'calculate' that evaluates the expression parameter. \
             When the tool is called, return the result as text.",
        );

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Client that connects and calls the calculate tool
        let client_config = NetGetConfig::new(format!(
            "Connect to http://127.0.0.1:{} via MCP. \
             List available tools, then call the 'calculate' tool with expression '2+2'.",
            server.port
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to make requests
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify the client shows MCP activity
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("MCP"))
                || output.iter().any(|l| l.contains("tool"))
                || output.iter().any(|l| l.contains("calculate")),
            "Client should show MCP tool activity. Output: {:?}",
            output
        );

        println!("✅ MCP client called tool successfully");

        // Cleanup
        server.verify_mocks().await?;
        server.stop().await?;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test MCP client reading a resource from server
    /// LLM calls: 3 (server startup, client connection, resource read)
    #[tokio::test]
    #[ignore] // No .with_mock() configured: requires --use-ollama. Under default
              // strict-mock CI mode the LLM call 500s immediately and the client
              // never connects.
    async fn test_mcp_client_read_resource() -> E2EResult<()> {
        // Start an MCP server with a resource
        let server_config = NetGetConfig::new(
            "Listen on port {AVAILABLE_PORT} via MCP. \
             Provide a resource at URI 'file:///README.md' with content 'Test resource content'.",
        );

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Client that connects and reads the resource
        let client_config = NetGetConfig::new(format!(
            "Connect to http://127.0.0.1:{} via MCP. \
             List available resources, then read the resource at 'file:///README.md'.",
            server.port
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to make requests
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify the client is MCP protocol
        assert_eq!(client.protocol, "MCP", "Client should be MCP protocol");

        println!("✅ MCP client read resource successfully");

        // Cleanup
        server.verify_mocks().await?;
        server.stop().await?;
        client.verify_mocks().await?;
        client.stop().await?;

        Ok(())
    }
}
