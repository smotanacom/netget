//! MQTT client end-to-end tests
//!
//! Tests the MQTT client by connecting to a real Mosquitto broker
//! and performing publish/subscribe operations under LLM control.

#![cfg(all(test, feature = "mqtt"))]

use netget::llm::ollama_client::OllamaClient;
use netget::state::app_state::AppState;
use netget::state::{ClientId, ClientInstance};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;

/// Helper to start a test Mosquitto broker via Docker
async fn start_mosquitto_broker() -> Result<(), Box<dyn std::error::Error>> {
    // Kill any existing mosquitto containers
    let _ = tokio::process::Command::new("docker")
        .args(["kill", "netget-test-mosquitto"])
        .output()
        .await;

    let _ = tokio::process::Command::new("docker")
        .args(["rm", "netget-test-mosquitto"])
        .output()
        .await;

    // Start Mosquitto broker
    let output = tokio::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            "netget-test-mosquitto",
            "-p",
            "1883:1883",
            "eclipse-mosquitto:2.0",
            "mosquitto",
            "-c",
            "/mosquitto-no-auth.conf",
        ])
        .output()
        .await?;

    if !output.status.success() {
        return Err(format!(
            "Failed to start Mosquitto: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    // Wait for broker to be ready
    sleep(Duration::from_secs(2)).await;

    Ok(())
}

/// Helper to stop the test Mosquitto broker
async fn stop_mosquitto_broker() {
    let _ = tokio::process::Command::new("docker")
        .args(["kill", "netget-test-mosquitto"])
        .output()
        .await;

    let _ = tokio::process::Command::new("docker")
        .args(["rm", "netget-test-mosquitto"])
        .output()
        .await;
}

/// Test basic MQTT client connection and LLM-controlled subscription
#[tokio::test]
#[ignore = "Requires Docker (Mosquitto broker) and real Ollama (no .with_mock() configured)"]
async fn test_mqtt_client_basic() -> Result<(), Box<dyn std::error::Error>> {
    start_mosquitto_broker().await?;

    // Initialize app state
    let app_state = Arc::new(AppState::new());
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();

    // Open MQTT client with instruction to subscribe to test topic
    let mut client = ClientInstance::new(
        ClientId::new(0), // overwritten by add_client with the real allocated id
        "localhost:1883".to_string(),
        "MQTT".to_string(),
        "Connect to broker and subscribe to 'test/topic'. When you receive a message, publish 'response' to 'test/response'.".to_string(),
    );
    client.startup_params = Some(serde_json::json!({
        "client_id": "netget-test-client",
        "clean_session": true,
    }));
    let client_id = app_state.add_client(client).await;

    // Start the client
    netget::cli::client_startup::start_client_by_id(&app_state, client_id, &llm_client, &status_tx)
        .await?;

    // Wait for client to connect and subscribe
    sleep(Duration::from_secs(3)).await;

    // Check that client is connected
    let client = app_state.get_client(client_id).await.unwrap();
    assert!(matches!(
        client.status,
        netget::state::ClientStatus::Connected
    ));

    // Use mosquitto_pub to publish a test message
    let output = tokio::process::Command::new("docker")
        .args([
            "exec",
            "netget-test-mosquitto",
            "mosquitto_pub",
            "-h",
            "localhost",
            "-t",
            "test/topic",
            "-m",
            "hello from mosquitto",
        ])
        .output()
        .await?;

    assert!(
        output.status.success(),
        "Failed to publish test message: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for LLM to process and respond
    sleep(Duration::from_secs(5)).await;

    // Check that a response was published (we'd need mosquitto_sub to verify this properly)
    // For now, just verify no errors in status messages
    let mut had_error = false;
    while let Ok(msg) = status_rx.try_recv() {
        if msg.contains("[ERROR]") {
            println!("Error message: {}", msg);
            had_error = true;
        }
        if msg.contains("published to 'test/response'") {
            println!("✓ Response published successfully");
        }
    }

    assert!(!had_error, "Errors occurred during test");

    // Cleanup
    stop_mosquitto_broker().await;

    Ok(())
}

/// Test MQTT client with QoS levels
#[tokio::test]
#[ignore = "Requires Docker (Mosquitto broker) and real Ollama (no .with_mock() configured)"]
async fn test_mqtt_client_qos() -> Result<(), Box<dyn std::error::Error>> {
    start_mosquitto_broker().await?;

    let app_state = Arc::new(AppState::new());
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());
    let (status_tx, _status_rx) = mpsc::unbounded_channel();

    // Open MQTT client
    let mut client = ClientInstance::new(
        ClientId::new(0), // overwritten by add_client with the real allocated id
        "localhost:1883".to_string(),
        "MQTT".to_string(),
        "Subscribe to 'qos/#' with QoS 2 (ExactlyOnce). For each message, publish it back to 'echo/+' with QoS 1.".to_string(),
    );
    client.startup_params = Some(serde_json::json!({
        "client_id": "netget-qos-test",
    }));
    let client_id = app_state.add_client(client).await;

    netget::cli::client_startup::start_client_by_id(&app_state, client_id, &llm_client, &status_tx)
        .await?;

    sleep(Duration::from_secs(3)).await;

    // Publish messages with different QoS levels
    for qos in 0..=2 {
        let output = tokio::process::Command::new("docker")
            .args([
                "exec",
                "netget-test-mosquitto",
                "mosquitto_pub",
                "-h",
                "localhost",
                "-t",
                &format!("qos/level{}", qos),
                "-m",
                &format!("QoS {} message", qos),
                "-q",
                &qos.to_string(),
            ])
            .output()
            .await?;

        assert!(output.status.success());
    }

    // Wait for processing
    sleep(Duration::from_secs(5)).await;

    // Cleanup
    stop_mosquitto_broker().await;

    Ok(())
}

/// Test MQTT client with wildcards
#[tokio::test]
#[ignore = "Requires Docker (Mosquitto broker) and real Ollama (no .with_mock() configured)"]
async fn test_mqtt_client_wildcards() -> Result<(), Box<dyn std::error::Error>> {
    start_mosquitto_broker().await?;

    let app_state = Arc::new(AppState::new());
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());
    let (status_tx, _status_rx) = mpsc::unbounded_channel();

    // Test multi-level wildcard (#)
    let mut client = ClientInstance::new(
        ClientId::new(0), // overwritten by add_client with the real allocated id
        "localhost:1883".to_string(),
        "MQTT".to_string(),
        "Subscribe to 'sensors/#' to receive all sensor data. Count the messages received."
            .to_string(),
    );
    client.startup_params = Some(serde_json::json!({
        "client_id": "netget-wildcard-test",
    }));
    let client_id = app_state.add_client(client).await;

    netget::cli::client_startup::start_client_by_id(&app_state, client_id, &llm_client, &status_tx)
        .await?;

    sleep(Duration::from_secs(3)).await;

    // Publish to multiple topics under sensors/
    let topics = [
        "sensors/temperature",
        "sensors/humidity",
        "sensors/room1/temperature",
        "sensors/room2/humidity",
    ];

    for topic in &topics {
        let output = tokio::process::Command::new("docker")
            .args([
                "exec",
                "netget-test-mosquitto",
                "mosquitto_pub",
                "-h",
                "localhost",
                "-t",
                topic,
                "-m",
                "sensor data",
            ])
            .output()
            .await?;

        assert!(output.status.success());
    }

    sleep(Duration::from_secs(5)).await;

    // Cleanup
    stop_mosquitto_broker().await;

    Ok(())
}

/// Test retained messages
#[tokio::test]
#[ignore = "Requires Docker (Mosquitto broker) and real Ollama (no .with_mock() configured)"]
async fn test_mqtt_client_retained() -> Result<(), Box<dyn std::error::Error>> {
    start_mosquitto_broker().await?;

    let app_state = Arc::new(AppState::new());
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());
    let (status_tx, _status_rx) = mpsc::unbounded_channel();

    // First, publish a retained message using mosquitto_pub
    let output = tokio::process::Command::new("docker")
        .args([
            "exec",
            "netget-test-mosquitto",
            "mosquitto_pub",
            "-h",
            "localhost",
            "-t",
            "retained/status",
            "-m",
            "system online",
            "-r", // retained flag
        ])
        .output()
        .await?;

    assert!(output.status.success());

    sleep(Duration::from_secs(1)).await;

    // Now connect a client that subscribes to the retained topic
    let mut client = ClientInstance::new(
        ClientId::new(0), // overwritten by add_client with the real allocated id
        "localhost:1883".to_string(),
        "MQTT".to_string(),
        "Subscribe to 'retained/status'. You should immediately receive the retained message."
            .to_string(),
    );
    client.startup_params = Some(serde_json::json!({
        "client_id": "netget-retained-test",
    }));
    let client_id = app_state.add_client(client).await;

    netget::cli::client_startup::start_client_by_id(&app_state, client_id, &llm_client, &status_tx)
        .await?;

    // The client should receive the retained message immediately upon subscription
    sleep(Duration::from_secs(3)).await;

    // Cleanup
    stop_mosquitto_broker().await;

    Ok(())
}
