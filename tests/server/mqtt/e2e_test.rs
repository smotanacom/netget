//! E2E tests for MQTT protocol
//!
//! These tests verify MQTT broker functionality by starting NetGet with MQTT prompts
//! and using rumqttc client library to connect and publish/subscribe.
//!
//! NOTE: MQTT broker is currently a placeholder implementation. These tests verify
//! that the protocol is registered and returns appropriate error messages.
//! Once full broker implementation is complete, these tests will be updated to
//! validate actual MQTT functionality.

#![cfg(feature = "mqtt")]

use crate::server::helpers::*;
use std::time::Duration;

/// Test that MQTT broker starts successfully
#[tokio::test]
async fn test_mqtt_broker_starts() -> E2EResult<()> {
    let config = NetGetConfig::new("Start an MQTT broker on port 0")
        .with_log_level("off")
        .with_mock(|mock| {
            mock.on_instruction_containing("MQTT broker")
                .respond_with_actions(serde_json::json!([{"type": "open_server", "port": 0, "base_stack": "MQTT", "instruction": "MQTT broker"}]))
                .expect_calls(1)
                .and()
        });

    let test_state = start_netget_server(config).await?;

    println!("✓ MQTT broker started on port {}", test_state.port);

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    test_state.wait_for_mocks(30).await;
    test_state.verify_mocks().await?;
    test_state.stop().await?;
    Ok(())
}

/// Test MQTT protocol is detectable from prompt keywords
///
/// Verifies that the protocol registry can detect MQTT from various keywords
/// like "mqtt", "mosquitto", etc.
#[tokio::test]
async fn test_mqtt_keyword_detection() -> E2EResult<()> {
    // Test various MQTT keywords
    let mqtt_prompts = vec![
        "Start an MQTT broker on port 1883",
        "Create a mosquitto server for IoT devices",
        "Listen via MQTT on port 0",
        "Set up message queue telemetry transport on port 1883",
    ];

    for prompt in mqtt_prompts {
        println!("Testing prompt: {}", prompt);

        let config = NetGetConfig::new(prompt).with_log_level("off");

        // All should fail (placeholder), but for the right reason (MQTT detected)
        let result = start_netget_server(config).await;

        if let Err(e) = result {
            let error_msg = e.to_string();

            // Should not be "unknown protocol" - MQTT should be detected
            assert!(
                !error_msg.contains("unknown") && !error_msg.contains("Unknown"),
                "MQTT should be detected from prompt '{}', got: {}",
                prompt,
                error_msg
            );

            println!("  ✓ MQTT detected from: {}", prompt);
        } else {
            panic!("Expected error for placeholder MQTT broker");
        }
    }

    println!("✓ MQTT keyword detection working");
    Ok(())
}

// ============================================================================
// MQTT BROKER TESTS
// ============================================================================

#[tokio::test]
async fn test_mqtt_basic_connect() -> E2EResult<()> {
    use rumqttc::{AsyncClient, Event, MqttOptions, Packet};

    let config =
        NetGetConfig::new("Start an MQTT broker on port 0. Accept all client connections.")
            .with_log_level("debug")
            .with_mock(|mock| {
                mock.on_instruction_containing("MQTT broker")
                    .and_instruction_containing("Accept all client connections")
                    .respond_with_actions(serde_json::json!([{"type": "open_server", "port": 0, "base_stack": "MQTT", "instruction": "MQTT broker accepting connections"}]))
                    .expect_calls(1)
                    .and()
                    // The handler must actually run and accept. This rule used to be absent,
                    // so the test passed on the CONNECT fail-open default (CONNACK return code
                    // 0 whenever the handler produced nothing, including when it could not run
                    // at all) - i.e. it asserted that a broker with a dead backend lets clients
                    // in. A refusal is now the correct answer in that case, so the acceptance
                    // this test is about has to be asked for.
                    .on_event("mqtt_connect")
                    .respond_with_actions(serde_json::json!([
                        {"type": "mqtt_connack", "return_code": 0, "session_present": false}
                    ]))
                    .expect_calls(1)
                    .and()
            });

    let test_state = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create MQTT client
    let mut mqttoptions = MqttOptions::new("test_client", "127.0.0.1", test_state.port);
    mqttoptions.set_keep_alive(Duration::from_secs(5));

    let (_client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    // Try to connect and receive CONNACK
    let mut connected = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_secs(2), eventloop.poll()).await {
            Ok(Ok(Event::Incoming(Packet::ConnAck(_)))) => {
                println!("✓ Received CONNACK from MQTT broker");
                connected = true;
                break;
            }
            Ok(Ok(event)) => {
                println!("MQTT event: {:?}", event);
            }
            Ok(Err(e)) => {
                eprintln!("MQTT error: {}", e);
                break;
            }
            Err(_) => {
                eprintln!("Timeout waiting for CONNACK");
                break;
            }
        }
    }

    assert!(connected, "Should receive CONNACK from broker");

    println!("✓ MQTT client connected successfully");

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    test_state.wait_for_mocks(30).await;
    test_state.verify_mocks().await?;
    test_state.stop().await?;
    Ok(())
}

// Future tests - to be implemented when pub/sub is added
/*
#[tokio::test]
#[ignore = "Pub/sub not yet implemented"]
async fn test_mqtt_publish_subscribe() -> E2EResult<()> {
    use rumqttc::{AsyncClient, MqttOptions, QoS};
    use tokio::sync::mpsc;

    let config = NetGetConfig::new(
        "Start an MQTT broker on port 0. \
         Accept all connections. \
         Allow publishing to 'test/topic' and subscribing to 'test/#' wildcard."
    )
    .with_log_level("debug");

    let test_state = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create subscriber client
    let mut sub_options = MqttOptions::new("subscriber", "127.0.0.1", test_state.port);
    sub_options.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(sub_options, 10);

    // Channel to receive published messages
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();

    // Subscribe to topic
    sub_client.subscribe("test/#", QoS::AtMostOnce).await?;

    // Spawn subscriber event loop
    tokio::spawn(async move {
        while let Ok(notification) = sub_eventloop.poll().await {
            if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = notification {
                println!("Received: {} = {:?}", publish.topic, publish.payload);
                let _ = msg_tx.send(publish);
            }
        }
    });

    // Wait for subscription to take effect
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create publisher client
    let mut pub_options = MqttOptions::new("publisher", "127.0.0.1", test_state.port);
    pub_options.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_options, 10);

    tokio::spawn(async move {
        while let Ok(_) = pub_eventloop.poll().await {
            // Keep connection alive
        }
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish message
    pub_client
        .publish("test/topic", QoS::AtMostOnce, false, b"Hello MQTT")
        .await?;

    println!("Published message to test/topic");

    // Wait for message to be received
    tokio::time::timeout(Duration::from_secs(5), msg_rx.recv())
        .await
        .expect("Timeout waiting for message")
        .expect("Channel closed");

    println!("✓ MQTT publish/subscribe successful");

    test_state.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "MQTT broker not yet implemented"]
async fn test_mqtt_qos_levels() -> E2EResult<()> {
    use rumqttc::{AsyncClient, MqttOptions, QoS};

    let config = NetGetConfig::new(
        "Start an MQTT broker on port 0. \
         Support QoS levels 0, 1, and 2. \
         Accept all connections."
    )
    .with_log_level("debug");

    let test_state = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Create client
    let mut mqttoptions = MqttOptions::new("qos_test", "127.0.0.1", test_state.port);
    mqttoptions.set_keep_alive(Duration::from_secs(5));
    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    tokio::spawn(async move {
        while let Ok(_) = eventloop.poll().await {}
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Test QoS 0 (at most once)
    client
        .publish("test/qos0", QoS::AtMostOnce, false, b"QoS 0")
        .await?;
    println!("✓ QoS 0 publish succeeded");

    // Test QoS 1 (at least once)
    client
        .publish("test/qos1", QoS::AtLeastOnce, false, b"QoS 1")
        .await?;
    println!("✓ QoS 1 publish succeeded");

    // Test QoS 2 (exactly once)
    client
        .publish("test/qos2", QoS::ExactlyOnce, false, b"QoS 2")
        .await?;
    println!("✓ QoS 2 publish succeeded");

    test_state.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "MQTT broker not yet implemented"]
async fn test_mqtt_retained_messages() -> E2EResult<()> {
    use rumqttc::{AsyncClient, MqttOptions, QoS};
    use tokio::sync::mpsc;

    let config = NetGetConfig::new(
        "Start an MQTT broker on port 0. \
         Support retained messages. \
         When a client subscribes to a topic with a retained message, \
         send the retained message immediately."
    )
    .with_log_level("debug");

    let test_state = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publisher: Send retained message
    let mut pub_options = MqttOptions::new("publisher", "127.0.0.1", test_state.port);
    pub_options.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_options, 10);

    tokio::spawn(async move {
        while let Ok(_) = pub_eventloop.poll().await {}
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish retained message
    pub_client
        .publish("test/retained", QoS::AtLeastOnce, true, b"Retained message")
        .await?;

    println!("Published retained message");

    // Wait for broker to process
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Subscriber: Connect and subscribe (should receive retained message)
    let mut sub_options = MqttOptions::new("late_subscriber", "127.0.0.1", test_state.port);
    sub_options.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(sub_options, 10);

    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();

    sub_client.subscribe("test/retained", QoS::AtMostOnce).await?;

    tokio::spawn(async move {
        while let Ok(notification) = sub_eventloop.poll().await {
            if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = notification {
                let _ = msg_tx.send(publish);
            }
        }
    });

    // Wait for retained message
    let received = tokio::time::timeout(Duration::from_secs(5), msg_rx.recv())
        .await
        .expect("Timeout waiting for retained message")
        .expect("Channel closed");

    assert_eq!(received.topic, "test/retained");
    assert_eq!(received.payload.as_ref(), b"Retained message");

    println!("✓ Retained message received by late subscriber");

    test_state.stop().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "MQTT broker not yet implemented"]
async fn test_mqtt_wildcard_subscriptions() -> E2EResult<()> {
    use rumqttc::{AsyncClient, MqttOptions, QoS};
    use tokio::sync::mpsc;

    let config = NetGetConfig::new(
        "Start an MQTT broker on port 0. \
         Support wildcard subscriptions with + (single level) and # (multi-level). \
         Allow publishing to any topic under 'devices/'."
    )
    .with_log_level("debug");

    let test_state = start_netget_server(config).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Subscriber with wildcard
    let mut sub_options = MqttOptions::new("subscriber", "127.0.0.1", test_state.port);
    sub_options.set_keep_alive(Duration::from_secs(5));
    let (sub_client, mut sub_eventloop) = AsyncClient::new(sub_options, 10);

    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();

    // Subscribe to "devices/+/temp" (single-level wildcard)
    sub_client.subscribe("devices/+/temp", QoS::AtMostOnce).await?;

    tokio::spawn(async move {
        while let Ok(notification) = sub_eventloop.poll().await {
            if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish)) = notification {
                let _ = msg_tx.send(publish.topic);
            }
        }
    });

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Publisher
    let mut pub_options = MqttOptions::new("publisher", "127.0.0.1", test_state.port);
    pub_options.set_keep_alive(Duration::from_secs(5));
    let (pub_client, mut pub_eventloop) = AsyncClient::new(pub_options, 10);

    tokio::spawn(async move {
        while let Ok(_) = pub_eventloop.poll().await {}
    });

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Publish to matching topics
    pub_client.publish("devices/sensor1/temp", QoS::AtMostOnce, false, b"25.5").await?;
    pub_client.publish("devices/sensor2/temp", QoS::AtMostOnce, false, b"26.0").await?;

    // This should NOT match (different subtopic)
    pub_client.publish("devices/sensor1/humidity", QoS::AtMostOnce, false, b"60").await?;

    // Receive messages
    let mut received_topics = vec![];
    for _ in 0..2 {
        if let Ok(Some(topic)) = tokio::time::timeout(Duration::from_secs(3), msg_rx.recv()).await {
            received_topics.push(topic);
        }
    }

    assert_eq!(received_topics.len(), 2, "Should receive 2 messages matching wildcard");
    assert!(received_topics.contains(&"devices/sensor1/temp".to_string()));
    assert!(received_topics.contains(&"devices/sensor2/temp".to_string()));

    println!("✓ Wildcard subscriptions working correctly");

    test_state.stop().await?;
    Ok(())
}
*/
