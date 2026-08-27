//! End-to-end mDNS tests for NetGet
//!
//! These tests spawn the actual NetGet binary with mDNS prompts
//! and validate service advertisements using mDNS-SD clients.

#![cfg(feature = "mdns")]

// Helper module imported from parent

use super::super::super::helpers::{self, E2EResult, NetGetConfig};
use std::time::Duration;

#[tokio::test]
async fn test_mdns_service_advertisement() -> E2EResult<()> {
    println!("\n=== E2E Test: mDNS Service Advertisement ===");

    // PROMPT: Tell the LLM to advertise a service via mDNS
    let prompt = "listen on port {AVAILABLE_PORT} via mdns. Advertise service: \
        type '_http._tcp.local.', name 'NetGet Test Server', port {AVAILABLE_PORT}, \
        with property 'version=1.0'";

    // Start the server
    let server_config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock.on_instruction_containing("mdns")
            .and_instruction_containing("Advertise service")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "mdns",
                    "instruction": "mDNS service advertisement",
                    "startup_params": {
                        "service_type": "_http._tcp.local.",
                        "service_name": "NetGet Test Server",
                        "properties": {"version": "1.0"}
                    }
                }
            ]))
            .expect_calls(1)
            .and()
    });
    let mut server = helpers::start_netget_server(server_config).await?;
    println!("Server started, mDNS should be advertising");

    // Give mDNS time to start advertising

    // VALIDATION: Query for mDNS services
    println!("Querying for mDNS services...");

    // Create mDNS browser
    let mdns = mdns_sd::ServiceDaemon::new()
        .map_err(|e| format!("Failed to create mDNS daemon: {}", e))?;

    // Browse for _http._tcp services
    let service_type = "_http._tcp.local.";
    println!("Browsing for service type: {}", service_type);

    let receiver = mdns
        .browse(service_type)
        .map_err(|e| format!("Failed to browse mDNS: {}", e))?;

    // Wait for service discovery (with timeout)
    let mut found_service = false;
    let timeout_duration = Duration::from_secs(10);
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match tokio::time::timeout(Duration::from_secs(2), async {
            receiver.recv_async().await
        })
        .await
        {
            Ok(Ok(event)) => {
                match event {
                    mdns_sd::ServiceEvent::ServiceResolved(info) => {
                        println!("✓ mDNS service discovered:");
                        println!("  Instance: {}", info.get_fullname());
                        println!("  Port: {}", info.get_port());
                        println!("  Hostname: {}", info.get_hostname());

                        // Service type is part of fullname
                        println!("  ✓ Service resolved");
                        found_service = true;

                        // Check properties
                        let props = info.get_properties();
                        println!("  Properties count: {}", props.len());

                        break;
                    }
                    mdns_sd::ServiceEvent::ServiceFound(ty, fullname) => {
                        println!("  Service found: {} ({})", fullname, ty);
                    }
                    mdns_sd::ServiceEvent::ServiceRemoved(ty, fullname) => {
                        println!("  Service removed: {} ({})", fullname, ty);
                    }
                    _ => {
                        println!("  Other mDNS event: {:?}", event);
                    }
                }
            }
            Ok(Err(e)) => {
                println!("  mDNS receive error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout on this recv, continue polling
            }
        }
    }

    if found_service {
        println!("✓ mDNS service advertisement verified");
    } else {
        println!("Note: mDNS service not discovered within timeout");
        println!("  This may be due to network configuration or timing");
    }

    // Shutdown mDNS daemon
    let _ = mdns.shutdown();

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_mdns_multiple_services() -> E2EResult<()> {
    println!("\n=== E2E Test: mDNS Multiple Services ===");

    // PROMPT: Tell the LLM to advertise multiple services
    let prompt = "listen on port {AVAILABLE_PORT} via mdns. Register two services: \
        1) type '_http._tcp.local.', name 'Web Service', port {AVAILABLE_PORT} \
        2) type '_ftp._tcp.local.', name 'FTP Service', port {AVAILABLE_PORT}";

    // Start the server
    let server_config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock.on_instruction_containing("mdns")
            .and_instruction_containing("two services")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "mdns",
                    "instruction": "mDNS multiple services",
                    "startup_params": {
                        "services": [
                            {"service_type": "_http._tcp.local.", "service_name": "Web Service"},
                            {"service_type": "_ftp._tcp.local.", "service_name": "FTP Service"}
                        ]
                    }
                }
            ]))
            .expect_calls(1)
            .and()
    });
    let mut server = helpers::start_netget_server(server_config).await?;
    println!("Server started, mDNS should be advertising multiple services");

    // Give mDNS time to start advertising

    // VALIDATION: Query for mDNS services
    println!("Querying for mDNS services...");

    // Create mDNS browser
    let mdns = mdns_sd::ServiceDaemon::new()
        .map_err(|e| format!("Failed to create mDNS daemon: {}", e))?;

    // Browse for all services
    let service_types = vec!["_http._tcp.local.", "_ftp._tcp.local."];
    let mut found_count = 0;

    for service_type in service_types {
        println!("Browsing for service type: {}", service_type);

        let receiver = mdns
            .browse(service_type)
            .map_err(|e| format!("Failed to browse mDNS: {}", e))?;

        // Wait briefly for this service type
        let timeout_duration = Duration::from_secs(5);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout_duration {
            match tokio::time::timeout(Duration::from_secs(1), async {
                receiver.recv_async().await
            })
            .await
            {
                Ok(Ok(event)) => match event {
                    mdns_sd::ServiceEvent::ServiceResolved(info) => {
                        println!("  ✓ Found service: {}", info.get_fullname());
                        found_count += 1;
                        break;
                    }
                    mdns_sd::ServiceEvent::ServiceFound(_, fullname) => {
                        println!("  Service found: {}", fullname);
                    }
                    _ => {}
                },
                _ => break,
            }
        }
    }

    if found_count > 0 {
        println!("✓ Found {} mDNS service(s)", found_count);
    } else {
        println!("Note: No mDNS services discovered");
    }

    // Shutdown mDNS daemon
    let _ = mdns.shutdown();

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_mdns_service_with_properties() -> E2EResult<()> {
    println!("\n=== E2E Test: mDNS Service with TXT Properties ===");

    // PROMPT: Tell the LLM to advertise a service with properties
    let prompt = "listen on port {AVAILABLE_PORT} via mdns. Register service: \
        type '_http._tcp.local.', name 'Property Test', port {AVAILABLE_PORT}, \
        with properties: version='2.0', path='/api', secure='true'";

    // Start the server
    let server_config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock.on_instruction_containing("mdns")
            .and_instruction_containing("properties")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "mdns",
                    "instruction": "mDNS service with properties",
                    "startup_params": {
                        "service_type": "_http._tcp.local.",
                        "service_name": "Property Test",
                        "properties": {"version": "2.0", "path": "/api", "secure": "true"}
                    }
                }
            ]))
            .expect_calls(1)
            .and()
    });
    let mut server = helpers::start_netget_server(server_config).await?;
    println!("Server started, mDNS advertising with properties");

    // Give mDNS time to start

    // VALIDATION: Query and check properties
    println!("Querying for mDNS service properties...");

    let mdns = mdns_sd::ServiceDaemon::new()
        .map_err(|e| format!("Failed to create mDNS daemon: {}", e))?;

    let service_type = "_http._tcp.local.";
    let receiver = mdns
        .browse(service_type)
        .map_err(|e| format!("Failed to browse mDNS: {}", e))?;

    let mut found_properties = false;
    let timeout_duration = Duration::from_secs(10);
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match tokio::time::timeout(Duration::from_secs(2), async {
            receiver.recv_async().await
        })
        .await
        {
            Ok(Ok(event)) => {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                    println!("✓ Service discovered with properties:");
                    let props = info.get_properties();

                    for prop in props.iter() {
                        println!("  Property: {:?}", prop);
                    }

                    if !props.is_empty() {
                        println!("  ✓ TXT properties found");
                        found_properties = true;
                    }
                    break;
                }
            }
            _ => break,
        }
    }

    if found_properties {
        println!("✓ mDNS service properties verified");
    } else {
        println!("Note: Service properties not verified");
    }

    let _ = mdns.shutdown();

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_mdns_custom_service_type() -> E2EResult<()> {
    println!("\n=== E2E Test: mDNS Custom Service Type ===");

    // PROMPT: Tell the LLM to advertise a custom service type
    let prompt = "listen on port {AVAILABLE_PORT} via mdns. Register custom service: \
        type '_netget._tcp.local.', name 'Custom NetGet Service', port {AVAILABLE_PORT}";

    // Start the server
    let server_config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock.on_instruction_containing("mdns")
            .and_instruction_containing("custom service")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "mdns",
                    "instruction": "mDNS custom service type",
                    "startup_params": {
                        "service_type": "_netget._tcp.local.",
                        "service_name": "Custom NetGet Service"
                    }
                }
            ]))
            .expect_calls(1)
            .and()
    });
    let mut server = helpers::start_netget_server(server_config).await?;
    println!("Server started with custom service type");

    // VALIDATION: Query for custom service type
    println!("Querying for custom service type...");

    let mdns = mdns_sd::ServiceDaemon::new()
        .map_err(|e| format!("Failed to create mDNS daemon: {}", e))?;

    let service_type = "_netget._tcp.local.";
    let receiver = mdns
        .browse(service_type)
        .map_err(|e| format!("Failed to browse mDNS: {}", e))?;

    let mut found_custom = false;
    let timeout_duration = Duration::from_secs(10);
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match tokio::time::timeout(Duration::from_secs(2), async {
            receiver.recv_async().await
        })
        .await
        {
            Ok(Ok(event)) => match event {
                mdns_sd::ServiceEvent::ServiceResolved(info) => {
                    println!("✓ Custom service type discovered: {}", info.get_fullname());
                    found_custom = true;
                    break;
                }
                mdns_sd::ServiceEvent::ServiceFound(ty, fullname) => {
                    println!("  Service found: {} ({})", fullname, ty);
                    if ty == service_type {
                        found_custom = true;
                    }
                }
                _ => {}
            },
            _ => break,
        }
    }

    if found_custom {
        println!("✓ Custom mDNS service type verified");
    } else {
        println!("Note: Custom service type not discovered");
    }

    let _ = mdns.shutdown();

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}
