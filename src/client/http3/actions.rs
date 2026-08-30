//! HTTP/3 client protocol actions implementation

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

/// HTTP/3 client connected event
pub static HTTP3_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "http3_connected",
        "HTTP/3 client connected via QUIC and ready to send requests",
        json!({
            "type": "send_http3_request",
            "method": "GET",
            "path": "/api/status",
            "priority": 5
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "base_url".to_string(),
            type_hint: "string".to_string(),
            description: "Base URL for HTTP/3 requests".to_string(),
            required: true,
        },
        Parameter {
            name: "connection_id".to_string(),
            type_hint: "string".to_string(),
            description: "QUIC connection ID".to_string(),
            required: true,
        },
    ])
});

/// HTTP/3 client response received event
pub static HTTP3_CLIENT_RESPONSE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "http3_response_received",
        "HTTP/3 response received from server",
        json!({
            "type": "send_http3_request",
            "method": "POST",
            "path": "/api/data",
            "body": "{\"key\": \"value\"}",
            "priority": 3
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "status_code".to_string(),
            type_hint: "number".to_string(),
            description: "HTTP status code".to_string(),
            required: true,
        },
        Parameter {
            name: "headers".to_string(),
            type_hint: "object".to_string(),
            description: "Response headers".to_string(),
            required: true,
        },
        Parameter {
            name: "body".to_string(),
            type_hint: "string".to_string(),
            description: "Response body".to_string(),
            required: true,
        },
        Parameter {
            name: "stream_id".to_string(),
            type_hint: "number".to_string(),
            description: "HTTP/3 stream ID".to_string(),
            required: true,
        },
    ])
});

/// HTTP/3 client protocol action handler
#[derive(Default)]
pub struct Http3ClientProtocol;

impl Http3ClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for Http3ClientProtocol {
    /// `enable_0rtt` used to be declared here and was removed rather than wired up: this
    /// client builds a fresh `quinn::Endpoint` per request and closes it before returning,
    /// and keeps no session-ticket cache, so there is never a previous session to resume
    /// from. 0-RTT is *only* resumption, so the knob could not have done anything on any
    /// request no matter what it was set to.
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![ParameterDefinition {
            name: "default_headers".to_string(),
            description: "Default headers to include in all requests".to_string(),
            type_hint: "object".to_string(),
            required: false,
            example: json!({
                "User-Agent": "NetGet-HTTP3/1.0",
                "Accept": "application/json"
            }),
        }]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "send_http3_request".to_string(),
                description: "Send an HTTP/3 request to the server".to_string(),
                parameters: vec![
                    Parameter {
                        name: "method".to_string(),
                        type_hint: "string".to_string(),
                        description: "HTTP method (GET, POST, PUT, DELETE, etc.)".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "path".to_string(),
                        type_hint: "string".to_string(),
                        description: "Request path (e.g., /api/users)".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "headers".to_string(),
                        type_hint: "object".to_string(),
                        description: "Request headers".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "body".to_string(),
                        type_hint: "string".to_string(),
                        description: "Request body".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "priority".to_string(),
                        type_hint: "number".to_string(),
                        description: "Stream priority (0-7, higher is more urgent)".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "send_http3_request",
                    "method": "GET",
                    "path": "/api/status",
                    "headers": {
                        "Accept": "application/json"
                    },
                    "priority": 5
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Close the QUIC connection".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "disconnect"
                }),
                log_template: None,
            },
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![ActionDefinition {
            name: "send_http3_request".to_string(),
            description: "Send another HTTP/3 request in response to received data".to_string(),
            parameters: vec![
                Parameter {
                    name: "method".to_string(),
                    type_hint: "string".to_string(),
                    description: "HTTP method".to_string(),
                    required: true,
                },
                Parameter {
                    name: "path".to_string(),
                    type_hint: "string".to_string(),
                    description: "Request path".to_string(),
                    required: true,
                },
                Parameter {
                    name: "headers".to_string(),
                    type_hint: "object".to_string(),
                    description: "Request headers".to_string(),
                    required: false,
                },
                Parameter {
                    name: "body".to_string(),
                    type_hint: "string".to_string(),
                    description: "Request body".to_string(),
                    required: false,
                },
                Parameter {
                    name: "priority".to_string(),
                    type_hint: "number".to_string(),
                    description: "Stream priority (0-7)".to_string(),
                    required: false,
                },
            ],
            example: json!({
                "type": "send_http3_request",
                "method": "POST",
                "path": "/api/data",
                "body": "{\"key\": \"value\"}",
                "priority": 3
            }),
            log_template: None,
        }]
    }
    fn protocol_name(&self) -> &'static str {
        "HTTP3"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "http3_connected",
                "Triggered when HTTP/3 client is connected via QUIC",
                json!({"type": "placeholder", "event_id": "http3_connected"}),
            ),
            EventType::new(
                "http3_response_received",
                "Triggered when HTTP/3 client receives a response",
                json!({"type": "placeholder", "event_id": "http3_response_received"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>UDP>QUIC>HTTP3"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["http3", "http/3", "quic", "h3", "connect to http3"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("quinn (QUIC) + h3 (HTTP/3) libraries")
            .llm_control("Full control over requests, stream priorities, 0-RTT decision")
            .e2e_testing("HTTP/3 capable server (e.g., Cloudflare, Google)")
            .build()
    }
    fn description(&self) -> &'static str {
        "HTTP/3 client for making web requests over QUIC"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to https://cloudflare-quic.com and fetch /cdn-cgi/trace using HTTP/3"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM handles HTTP/3 client
            json!({
                "type": "open_client",
                "remote_addr": "https://cloudflare-quic.com",
                "base_stack": "http3",
                "instruction": "Fetch /cdn-cgi/trace and display QUIC connection info"
            }),
            // Script mode: Code-based HTTP/3 handling
            json!({
                "type": "open_client",
                "remote_addr": "https://cloudflare-quic.com",
                "base_stack": "http3",
                "event_handlers": [{
                    "event_pattern": "http3_response_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<http3_handler>"
                    }
                }]
            }),
            // Static mode: Fixed HTTP/3 request
            json!({
                "type": "open_client",
                "remote_addr": "https://cloudflare-quic.com",
                "base_stack": "http3",
                "event_handlers": [{
                    "event_pattern": "http3_connected",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_http3_request",
                            "method": "GET",
                            "path": "/cdn-cgi/trace",
                            "priority": 5
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for Http3ClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::http3::Http3Client;
            Http3Client::connect_with_llm_actions(
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
            "send_http3_request" => {
                let method = action
                    .get("method")
                    .and_then(|v| v.as_str())
                    .context("Missing 'method' field")?
                    .to_string();

                let path = action
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Missing 'path' field")?
                    .to_string();

                let headers = action.get("headers").and_then(|v| v.as_object()).cloned();

                let body = action
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());

                let priority = action
                    .get("priority")
                    .and_then(|v| v.as_u64())
                    .map(|p| p as u8);

                // Return custom result with request data
                Ok(ClientActionResult::Custom {
                    name: "http3_request".to_string(),
                    data: json!({
                        "method": method,
                        "path": path,
                        "headers": headers,
                        "body": body,
                        "priority": priority,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            _ => Err(anyhow::anyhow!(
                "Unknown HTTP/3 client action: {}",
                action_type
            )),
        }
    }
}
