//! MQTT client protocol actions implementation

use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

/// MQTT client connected event
pub static MQTT_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mqtt_connected",
        "MQTT client successfully connected to broker",
        json!({"type": "subscribe", "topics": ["sensors/#"], "qos": 1}),
    )
    .with_parameters(vec![
        Parameter {
            name: "remote_addr".to_string(),
            type_hint: "string".to_string(),
            description: "MQTT broker address".to_string(),
            required: true,
        },
        Parameter {
            name: "client_id".to_string(),
            type_hint: "string".to_string(),
            description: "MQTT client ID used for connection".to_string(),
            required: true,
        },
    ])
});

/// MQTT message received event
pub static MQTT_MESSAGE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mqtt_message_received",
        "Message received from MQTT broker",
        json!({"type": "placeholder", "event_id": "mqtt_message_received"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "topic".to_string(),
            type_hint: "string".to_string(),
            description: "Topic where message was published".to_string(),
            required: true,
        },
        Parameter {
            name: "payload".to_string(),
            type_hint: "string".to_string(),
            description: "Message payload (UTF-8 string)".to_string(),
            required: true,
        },
        Parameter {
            name: "qos".to_string(),
            type_hint: "number".to_string(),
            description: "Quality of Service level (0, 1, or 2)".to_string(),
            required: true,
        },
        Parameter {
            name: "retain".to_string(),
            type_hint: "boolean".to_string(),
            description: "Whether this is a retained message".to_string(),
            required: true,
        },
    ])
});

/// MQTT client protocol action handler
pub struct MqttClientProtocol;

impl MqttClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for MqttClientProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
                ActionDefinition {
                    name: "subscribe".to_string(),
                    description: "Subscribe to one or more MQTT topics".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "topics".to_string(),
                            type_hint: "array".to_string(),
                            description: "Array of topic patterns (supports wildcards: + for single level, # for multi-level)".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "qos".to_string(),
                            type_hint: "number".to_string(),
                            description: "Quality of Service level (0=AtMostOnce, 1=AtLeastOnce, 2=ExactlyOnce). Default: 0".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "subscribe",
                        "topics": ["sensors/temperature", "sensors/+/status", "devices/#"],
                        "qos": 1
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "publish".to_string(),
                    description: "Publish a message to an MQTT topic".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "topic".to_string(),
                            type_hint: "string".to_string(),
                            description: "Topic to publish to (no wildcards allowed)".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "payload".to_string(),
                            type_hint: "string".to_string(),
                            description: "Message payload (UTF-8 string)".to_string(),
                            required: true,
                        },
                        Parameter {
                            name: "qos".to_string(),
                            type_hint: "number".to_string(),
                            description: "Quality of Service level (0, 1, or 2). Default: 0".to_string(),
                            required: false,
                        },
                        Parameter {
                            name: "retain".to_string(),
                            type_hint: "boolean".to_string(),
                            description: "Whether to retain the message on the broker. Default: false".to_string(),
                            required: false,
                        },
                    ],
                    example: json!({
                        "type": "publish",
                        "topic": "sensors/temperature",
                        "payload": "25.5",
                        "qos": 1,
                        "retain": false
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "unsubscribe".to_string(),
                    description: "Unsubscribe from MQTT topics".to_string(),
                    parameters: vec![
                        Parameter {
                            name: "topics".to_string(),
                            type_hint: "array".to_string(),
                            description: "Array of topics to unsubscribe from".to_string(),
                            required: true,
                        },
                    ],
                    example: json!({
                        "type": "unsubscribe",
                        "topics": ["sensors/temperature"]
                    }),
                log_template: None,
                },
                ActionDefinition {
                    name: "disconnect".to_string(),
                    description: "Disconnect from the MQTT broker".to_string(),
                    parameters: vec![],
                    example: json!({
                        "type": "disconnect"
                    }),
                log_template: None,
                },
            ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "publish".to_string(),
                description: "Publish a message in response to received data".to_string(),
                parameters: vec![
                    Parameter {
                        name: "topic".to_string(),
                        type_hint: "string".to_string(),
                        description: "Topic to publish to".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "payload".to_string(),
                        type_hint: "string".to_string(),
                        description: "Message payload".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "qos".to_string(),
                        type_hint: "number".to_string(),
                        description: "Quality of Service level (0, 1, or 2)".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "publish",
                    "topic": "sensors/response",
                    "payload": "acknowledged",
                    "qos": 0
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "subscribe".to_string(),
                description: "Subscribe to additional topics in response to events".to_string(),
                parameters: vec![
                    Parameter {
                        name: "topics".to_string(),
                        type_hint: "array".to_string(),
                        description: "Array of topic patterns".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "qos".to_string(),
                        type_hint: "number".to_string(),
                        description: "Quality of Service level".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "subscribe",
                    "topics": ["responses/#"],
                    "qos": 1
                }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "MQTT"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "mqtt_connected",
                "Triggered when MQTT client connects to broker",
                json!({"type": "placeholder", "event_id": "mqtt_connected"}),
            ),
            EventType::new(
                "mqtt_message_received",
                "Triggered when MQTT client receives a published message",
                json!({"type": "placeholder", "event_id": "mqtt_message_received"}),
            ),
            EventType::new(
                "mqtt_subscribed",
                "Triggered when MQTT client successfully subscribes to topics",
                json!({"type": "placeholder", "event_id": "mqtt_subscribed"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>MQTT"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["mqtt", "mqtt client", "connect to mqtt", "iot", "messaging"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("rumqttc async client library")
            .llm_control("Full control over subscriptions, publications, and QoS levels")
            .e2e_testing("Mosquitto MQTT broker in Docker")
            .build()
    }
    fn description(&self) -> &'static str {
        "MQTT client for IoT messaging and pub/sub communication"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to MQTT broker at localhost:1883, subscribe to sensors/# and publish a test message"
    }
    fn group_name(&self) -> &'static str {
        "Messaging"
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "client_id".to_string(),
                type_hint: "string".to_string(),
                description: "MQTT client identifier (default: auto-generated)".to_string(),
                required: false,
                example: json!("netget-sensor-monitor"),
            },
            ParameterDefinition {
                name: "username".to_string(),
                type_hint: "string".to_string(),
                description: "MQTT authentication username (optional)".to_string(),
                required: false,
                example: json!("admin"),
            },
            ParameterDefinition {
                name: "password".to_string(),
                type_hint: "string".to_string(),
                description: "MQTT authentication password (optional)".to_string(),
                required: false,
                example: json!("secret"),
            },
            ParameterDefinition {
                name: "keep_alive".to_string(),
                type_hint: "number".to_string(),
                description: "Keep-alive interval in seconds (default: 60)".to_string(),
                required: false,
                example: json!(60),
            },
            ParameterDefinition {
                name: "clean_session".to_string(),
                type_hint: "boolean".to_string(),
                description: "Start with a clean session (default: true)".to_string(),
                required: false,
                example: json!(true),
            },
        ]
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls MQTT pub/sub
            json!({
                "type": "open_client",
                "remote_addr": "localhost:1883",
                "base_stack": "mqtt",
                "instruction": "Subscribe to sensors/# and respond to temperature readings above 30"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_client",
                "remote_addr": "localhost:1883",
                "base_stack": "mqtt",
                "event_handlers": [{
                    "event_pattern": "mqtt_message_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<mqtt_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed MQTT subscription on connect
            json!({
                "type": "open_client",
                "remote_addr": "localhost:1883",
                "base_stack": "mqtt",
                "event_handlers": [
                    {
                        "event_pattern": "mqtt_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "subscribe",
                                "topics": ["sensors/#"],
                                "qos": 1
                            }]
                        }
                    },
                    {
                        "event_pattern": "mqtt_message_received",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "publish",
                                "topic": "ack",
                                "payload": "received"
                            }]
                        }
                    }
                ]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for MqttClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::mqtt::MqttClient;
            MqttClient::connect_with_llm_actions(
                ctx.remote_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.client_id,
                ctx.startup_params,
            )
            .await
        })
    }
    fn execute_action(&self, action: serde_json::Value) -> Result<ClientActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "subscribe" => {
                let topics = action
                    .get("topics")
                    .and_then(|v| v.as_array())
                    .context("Missing or invalid 'topics' field")?;

                let topic_strings: Vec<String> = topics
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();

                if topic_strings.is_empty() {
                    return Err(anyhow::anyhow!("No valid topics provided"));
                }

                let qos = action.get("qos").and_then(|v| v.as_u64()).unwrap_or(0) as u8;

                Ok(ClientActionResult::Custom {
                    name: "mqtt_subscribe".to_string(),
                    data: json!({
                        "topics": topic_strings,
                        "qos": qos,
                    }),
                })
            }
            "publish" => {
                let topic = action
                    .get("topic")
                    .and_then(|v| v.as_str())
                    .context("Missing 'topic' field")?
                    .to_string();

                let payload = action
                    .get("payload")
                    .and_then(|v| v.as_str())
                    .context("Missing 'payload' field")?
                    .to_string();

                let qos = action.get("qos").and_then(|v| v.as_u64()).unwrap_or(0) as u8;

                let retain = action
                    .get("retain")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                Ok(ClientActionResult::Custom {
                    name: "mqtt_publish".to_string(),
                    data: json!({
                        "topic": topic,
                        "payload": payload,
                        "qos": qos,
                        "retain": retain,
                    }),
                })
            }
            "unsubscribe" => {
                let topics = action
                    .get("topics")
                    .and_then(|v| v.as_array())
                    .context("Missing or invalid 'topics' field")?;

                let topic_strings: Vec<String> = topics
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();

                Ok(ClientActionResult::Custom {
                    name: "mqtt_unsubscribe".to_string(),
                    data: json!({
                        "topics": topic_strings,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            _ => Err(anyhow::anyhow!(
                "Unknown MQTT client action: {}",
                action_type
            )),
        }
    }
}
