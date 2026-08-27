//! E2E tests for HTTP/3 client
//!
//! These tests verify HTTP/3 client functionality by spawning the actual NetGet binary
//! and testing client behavior as a black-box over QUIC transport.

#[cfg(all(test, feature = "http3"))]
mod http3_client_tests {
    use crate::helpers::*;
    use std::time::Duration;

    /// Test HTTP/3 client making a GET request over QUIC
    /// LLM calls: 5 (server startup, server request, client startup, client connected, client response)
    #[tokio::test]
    #[ignore = "NetGet has no HTTP/3 server: the http3 feature builds the client only, so `base_stack: HTTP3` cannot start and there is nothing on the machine speaking HTTP/3 over QUIC for the client to reach. Re-enable when an http3 server protocol exists."]
    async fn test_http3_client_get_request() -> E2EResult<()> {
        // Start an HTTP/3 server listening on an available port with mocks
        let server_config = NetGetConfig::new("Listen on port {AVAILABLE_PORT} via HTTP/3. Respond to GET requests with 'Hello from HTTP/3 server'.")
            .with_mock(|mock| {
                mock
                    // Mock 1: Server startup
                    .on_instruction_containing("Listen on port")
                    .and_instruction_containing("HTTP/3")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "HTTP3",
                            "instruction": "Respond to GET requests with 'Hello from HTTP/3 server'"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Server receives GET request
                    .on_event("http_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "send_http_response",
                            "status": 200,
                            "headers": {"Content-Type": "text/plain"},
                            "body": "Hello from HTTP/3 server"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

        let mut server = start_netget_server(server_config).await?;

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Now start an HTTP/3 client that makes a GET request with mocks
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via HTTP/3. Send a GET request to / and read the response.",
            server.port
        ))
        .with_mock(|mock| {
            mock
                // Mock 1: Client startup
                .on_instruction_containing("Connect to")
                .and_instruction_containing("HTTP/3")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_client",
                        "remote_addr": format!("127.0.0.1:{}", server.port),
                        "protocol": "HTTP3",
                        "instruction": "Send GET request to / and read response"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Client connected
                .on_event("http3_connected")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_http3_request",
                        "method": "GET",
                        "path": "/",
                        "headers": {},
                        "body": ""
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: Client receives response
                .on_event("http3_response_received")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "wait_for_more"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

        let mut client = start_netget_client(client_config).await?;

        // Give client time to make QUIC connection and HTTP/3 request
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify client output shows HTTP/3 or QUIC protocol
        client
            .wait_for_any(&["HTTP/3", "HTTP3", "QUIC", "connected"], 30)
            .await;
        assert!(
            client.output_contains("HTTP/3").await
                || client.output_contains("HTTP3").await
                || client.output_contains("QUIC").await
                || client.output_contains("connected").await,
            "Client should show HTTP/3, QUIC protocol or connection message. Output: {:?}",
            client.get_output().await
        );

        println!("✅ HTTP/3 client made GET request over QUIC successfully");

        // Verify mock expectations were met
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(30).await;
        client.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test HTTP/3 client with stream priorities
    /// LLM calls: 5 (server startup, server request, client startup, client connected, client response)
    #[tokio::test]
    #[ignore = "NetGet has no HTTP/3 server: the http3 feature builds the client only, so `base_stack: HTTP3` cannot start and there is nothing on the machine speaking HTTP/3 over QUIC for the client to reach. Re-enable when an http3 server protocol exists."]
    async fn test_http3_client_with_priority() -> E2EResult<()> {
        // Start an HTTP/3 server with mocks
        let server_config = NetGetConfig::new("Listen on port {AVAILABLE_PORT} via HTTP/3. Log all incoming requests with their stream IDs.")
            .with_mock(|mock| {
                mock
                    // Mock 1: Server startup
                    .on_instruction_containing("Listen on port")
                    .and_instruction_containing("HTTP/3")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "HTTP3",
                            "instruction": "Log all incoming requests with stream IDs"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Server receives high-priority request
                    .on_event("http_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "send_http_response",
                            "status": 200,
                            "headers": {},
                            "body": "Request logged"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Client that makes a high-priority request with mocks
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via HTTP/3. Send a GET request to /urgent with high priority (priority 7).",
            server.port
        ))
            .with_mock(|mock| {
                mock
                    // Mock 1: Client startup
                    .on_instruction_containing("Connect to")
                    .and_instruction_containing("HTTP/3")
                    .and_instruction_containing("priority")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_client",
                            "remote_addr": format!("127.0.0.1:{}", server.port),
                            "protocol": "HTTP3",
                            "instruction": "Send high-priority GET request to /urgent"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Client connected - send high-priority request
                    .on_event("http3_connected")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "send_http3_request",
                            "method": "GET",
                            "path": "/urgent",
                            "headers": {},
                            "body": "",
                            "priority": 7
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 3: Client receives response
                    .on_event("http3_response_received")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "wait_for_more"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify the client is HTTP3 protocol
        assert_eq!(client.protocol, "HTTP3", "Client should be HTTP3 protocol");

        println!("✅ HTTP/3 client sent prioritized request");

        // Verify mock expectations were met
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(30).await;
        client.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }

    /// Test HTTP/3 client can handle LLM-controlled requests
    /// LLM calls: 5 (server startup, server request, client startup, client connected, client response)
    #[tokio::test]
    #[ignore = "NetGet has no HTTP/3 server: the http3 feature builds the client only, so `base_stack: HTTP3` cannot start and there is nothing on the machine speaking HTTP/3 over QUIC for the client to reach. Re-enable when an http3 server protocol exists."]
    async fn test_http3_client_llm_controlled() -> E2EResult<()> {
        // Start an HTTP/3 server with mocks
        let server_config = NetGetConfig::new("Listen on port {AVAILABLE_PORT} via HTTP/3. Respond to POST requests with the request body echoed back.")
            .with_mock(|mock| {
                mock
                    // Mock 1: Server startup
                    .on_instruction_containing("Listen on port")
                    .and_instruction_containing("HTTP/3")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "HTTP3",
                            "instruction": "Echo back POST request bodies"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Server receives POST request
                    .on_event("http_request")
                    .and_event_data_contains("method", "POST")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "send_http_response",
                            "status": 200,
                            "headers": {"Content-Type": "application/json"},
                            "body": "{\"message\": \"test message received\"}"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

        let mut server = start_netget_server(server_config).await?;

        tokio::time::sleep(Duration::from_millis(1000)).await;

        // Client that makes a POST request with custom headers and mocks
        let client_config = NetGetConfig::new(format!(
            "Connect to 127.0.0.1:{} via HTTP/3. Send a POST request to /api/data with JSON body containing a test message.",
            server.port
        ))
            .with_mock(|mock| {
                mock
                    // Mock 1: Client startup
                    .on_instruction_containing("Connect to")
                    .and_instruction_containing("HTTP/3")
                    .and_instruction_containing("POST")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_client",
                            "remote_addr": format!("127.0.0.1:{}", server.port),
                            "protocol": "HTTP3",
                            "instruction": "Send POST request with JSON body"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: Client connected - send POST request
                    .on_event("http3_connected")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "send_http3_request",
                            "method": "POST",
                            "path": "/api/data",
                            "headers": {"Content-Type": "application/json"},
                            "body": "{\"message\": \"test message\"}"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 3: Client receives response
                    .on_event("http3_response_received")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "wait_for_more"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

        let mut client = start_netget_client(client_config).await?;

        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify client made HTTP/3 connection
        client.wait_for_any(&["HTTP3", "QUIC"], 30).await;
        assert!(
            client.output_contains("HTTP3").await || client.output_contains("QUIC").await,
            "Client should use HTTP/3 or QUIC. Output: {:?}",
            client.get_output().await
        );

        println!("✅ HTTP/3 client responded to LLM instruction");

        // Verify mock expectations were met
        // Wait for the exchange the mocks describe, rather than trusting a fixed
        // sleep to have covered it. Under load the last response routinely lands
        // after the sleep expires, and the test reports it as never having happened.
        server.wait_for_mocks(30).await;
        client.wait_for_mocks(30).await;
        server.verify_mocks().await?;
        client.verify_mocks().await?;

        // Cleanup
        server.stop().await?;
        client.stop().await?;

        Ok(())
    }
}
