//! E2E tests for etcd client with mocks
//!
//! Tests etcd client operations against NetGet etcd server

#![cfg(all(test, feature = "etcd"))]

use crate::helpers::*;
use serde_json::json;
use std::time::Duration;

/// Test basic etcd client operations (PUT, GET, DELETE) with mocks
///
/// LLM calls: 7 total
/// - 1 server startup
/// - 1 client startup
/// - 1 client connected event
/// - 1 PUT operation (etcd_response_received)
/// - 1 GET operation (etcd_response_received)
/// - 1 DELETE operation (etcd_response_received)
/// - 1 GET after delete (etcd_response_received)
#[tokio::test]
async fn test_etcd_client_basic_operations() -> E2EResult<()> {
    // Start etcd server with mocks
    let server_config =
        NetGetConfig::new("Listen on port {AVAILABLE_PORT} via etcd. Handle all KV operations.")
            .with_mock(|mock| {
                mock
                    // Mock 1: Server startup
                    .on_instruction_containing("Listen on port")
                    .and_instruction_containing("etcd")
                    .respond_with_actions(json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "ETCD",
                            "instruction": "etcd KV store"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 2: PUT /test/key1 = value1
                    .on_event("etcd_put_request")
                    .and_event_data_contains("key", "/test/key1")
                    .respond_with_actions(json!([
                        {
                            "type": "etcd_put_response",
                            "revision": 1
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 3: GET /test/key1
                    .on_event("etcd_range_request")
                    .and_event_data_contains("key", "/test/key1")
                    .respond_with_actions(json!([
                        {
                            "type": "etcd_range_response",
                            "kvs": [
                                {
                                    "key": "/test/key1",
                                    "value": "value1",
                                    "create_revision": 1,
                                    "mod_revision": 1,
                                    "version": 1,
                                    "lease": 0
                                }
                            ],
                            "more": false,
                            "count": 1
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 4: DELETE /test/key1
                    .on_event("etcd_delete_request")
                    .and_event_data_contains("key", "/test/key1")
                    .respond_with_actions(json!([
                        {
                            "type": "etcd_delete_range_response",
                            "deleted": 1
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mock 5: GET /test/key1 after delete (returns empty)
                    .on_event("etcd_range_request")
                    .and_event_data_contains("key", "/test/key1")
                    .respond_with_actions(json!([
                        {
                            "type": "etcd_range_response",
                            "kvs": [],
                            "more": false,
                            "count": 0
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

    let mut server = start_netget_server(server_config).await?;

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start etcd client with mocks
    let client_config = NetGetConfig::new(format!(
        "Connect to 127.0.0.1:{} via etcd. Execute: PUT /test/key1 = value1, then GET /test/key1, then DELETE /test/key1, then GET /test/key1 again to verify deletion.",
        server.port
    ))
    .with_mock(|mock| {
        mock
            // Mock 1: Client startup
            .on_instruction_containing("Connect to")
            .and_instruction_containing("etcd")
            .respond_with_actions(json!([
                {
                    "type": "open_client",
                    "remote_addr": format!("127.0.0.1:{}", server.port),
                    "protocol": "etcd",
                    "instruction": "Execute PUT, GET, DELETE sequence"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Client connected - perform PUT
            .on_event("etcd_connected")
            .respond_with_actions(json!([
                {
                    "type": "etcd_put",
                    "key": "/test/key1",
                    "value": "value1"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: Response from PUT - perform GET
            .on_event("etcd_response_received")
            .and_event_data_contains("operation", "put")
            .respond_with_actions(json!([
                {
                    "type": "etcd_get",
                    "key": "/test/key1"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 4: Response from GET - perform DELETE
            .on_event("etcd_response_received")
            .and_event_data_contains("operation", "get")
            .respond_with_actions(json!([
                {
                    "type": "etcd_delete",
                    "key": "/test/key1"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 5: Response from DELETE - perform final GET
            .on_event("etcd_response_received")
            .and_event_data_contains("operation", "delete")
            .respond_with_actions(json!([
                {
                    "type": "etcd_get",
                    "key": "/test/key1"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 6: Response from final GET - disconnect
            .on_event("etcd_response_received")
            .and_event_data_contains("operation", "get")
            .respond_with_actions(json!([
                {
                    "type": "disconnect"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut client = start_netget_client(client_config).await?;

    // Give client time to execute operations
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Verify client output shows connection and operations
    client.wait_for_any(&["etcd"], 30).await;
    assert!(
        client.output_contains("etcd").await,
        "Client should show etcd protocol. Output: {:?}",
        client.get_output().await
    );

    println!("✅ etcd client completed PUT, GET, DELETE sequence successfully");

    // Verify mock expectations were met
    server.verify_mocks().await?;
    client.verify_mocks().await?;

    // Cleanup
    server.stop().await?;
    client.stop().await?;

    Ok(())
}

/// Test etcd client with multiple keys and operations
///
/// LLM calls: 8 total
/// - 1 server startup
/// - 1 client startup
/// - 1 client connected event
/// - 3 PUT operations
/// - 2 GET operations
#[tokio::test]
async fn test_etcd_client_multiple_keys() -> E2EResult<()> {
    // Start etcd server with mocks
    let server_config =
        NetGetConfig::new("Listen on port {AVAILABLE_PORT} via etcd. Store config keys.")
            .with_mock(|mock| {
                mock
                    // Mock: Server startup
                    .on_instruction_containing("Listen on port")
                    .and_instruction_containing("etcd")
                    .respond_with_actions(json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "ETCD",
                            "instruction": "Config store"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    // Mocks: 3 PUT operations
                    .on_event("etcd_put_request")
                    .respond_with_actions(json!([
                        {
                            "type": "etcd_put_response",
                            "revision": 1
                        }
                    ]))
                    .expect_calls(3) // Will be called 3 times
                    .and()
                    // Mocks: GET operations
                    .on_event("etcd_range_request")
                    .respond_with_actions(json!([
                        {
                            "type": "etcd_range_response",
                            "kvs": [
                                {
                                    "key": "/app/config/database",
                                    "value": "postgresql://localhost:5432/mydb",
                                    "create_revision": 1,
                                    "mod_revision": 1,
                                    "version": 1,
                                    "lease": 0
                                }
                            ],
                            "more": false,
                            "count": 1
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            });

    let mut server = start_netget_server(server_config).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start client with mocks
    let client_config = NetGetConfig::new(format!(
        "Connect to 127.0.0.1:{} via etcd. PUT /app/config/database=postgresql://localhost:5432/mydb, PUT /app/config/timeout=30, PUT /app/config/max_connections=100, then GET /app/config/database",
        server.port
    ))
    .with_mock(|mock| {
        mock
            // Mock: Client startup
            .on_instruction_containing("Connect to")
            .and_instruction_containing("etcd")
            .respond_with_actions(json!([
                {
                    "type": "open_client",
                    "remote_addr": format!("127.0.0.1:{}", server.port),
                    "protocol": "etcd",
                    "instruction": "Store and retrieve config"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock: Connected - PUT first key
            .on_event("etcd_connected")
            .respond_with_actions(json!([
                {
                    "type": "etcd_put",
                    "key": "/app/config/database",
                    "value": "postgresql://localhost:5432/mydb"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock: After first PUT - PUT second key
            .on_event("etcd_response_received")
            .and_event_data_contains("operation", "put")
            .respond_with_actions(json!([
                {
                    "type": "etcd_put",
                    "key": "/app/config/timeout",
                    "value": "30"
                }
            ]))
            .expect_at_least(1)  // Will be called at least once
            .and()
            // Mock: After GET - disconnect
            .on_event("etcd_response_received")
            .and_event_data_contains("operation", "get")
            .respond_with_actions(json!([
                {
                    "type": "disconnect"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut client = start_netget_client(client_config).await?;

    tokio::time::sleep(Duration::from_secs(3)).await;

    println!("✅ etcd client completed multiple key operations");

    // Verify mocks
    server.verify_mocks().await?;
    client.verify_mocks().await?;

    // Cleanup
    server.stop().await?;
    client.stop().await?;

    Ok(())
}

/// Test etcd client with nonexistent key
///
/// LLM calls: 3 total
/// - 1 server startup
/// - 1 client startup
/// - 1 GET operation
#[tokio::test]
async fn test_etcd_client_nonexistent_key() -> E2EResult<()> {
    // Start server
    let server_config =
        NetGetConfig::new("Listen on port {AVAILABLE_PORT} via etcd.").with_mock(|mock| {
            mock.on_instruction_containing("Listen on port")
                .and_instruction_containing("etcd")
                .respond_with_actions(json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "ETCD",
                        "instruction": "Empty store"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock: GET nonexistent key returns empty
                .on_event("etcd_range_request")
                .and_event_data_contains("key", "/does/not/exist")
                .respond_with_actions(json!([
                    {
                        "type": "etcd_range_response",
                        "kvs": [],
                        "more": false,
                        "count": 0
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let mut server = start_netget_server(server_config).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start client
    let client_config = NetGetConfig::new(format!(
        "Connect to 127.0.0.1:{} via etcd. GET /does/not/exist and verify it returns empty.",
        server.port
    ))
    .with_mock(|mock| {
        mock.on_instruction_containing("Connect to")
            .and_instruction_containing("etcd")
            .respond_with_actions(json!([
                {
                    "type": "open_client",
                    "remote_addr": format!("127.0.0.1:{}", server.port),
                    "protocol": "etcd",
                    "instruction": "Query nonexistent key"
                }
            ]))
            .expect_calls(1)
            .and()
            .on_event("etcd_connected")
            .respond_with_actions(json!([
                {
                    "type": "etcd_get",
                    "key": "/does/not/exist"
                }
            ]))
            .expect_calls(1)
            .and()
            .on_event("etcd_response_received")
            .respond_with_actions(json!([
                {
                    "type": "disconnect"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let mut client = start_netget_client(client_config).await?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    println!("✅ etcd client verified nonexistent key returns empty");

    // Verify mocks
    server.verify_mocks().await?;
    client.verify_mocks().await?;

    // Cleanup
    server.stop().await?;
    client.stop().await?;

    Ok(())
}
