//! Telnet client protocol actions implementation

use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, Parameter,
};
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;

/// Telnet client connected event
pub static TELNET_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "telnet_connected",
        "Telnet client successfully connected to server",
        json!({
            "type": "wait_for_more"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "remote_addr".to_string(),
        type_hint: "string".to_string(),
        description: "Remote server address".to_string(),
        required: true,
    }])
});

/// Telnet client data received event
pub static TELNET_CLIENT_DATA_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "telnet_data_received",
        "Data received from Telnet server",
        json!({"type": "placeholder", "event_id": "telnet_data_received"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "The text data received (Telnet commands stripped)".to_string(),
            required: true,
        },
        Parameter {
            name: "raw_hex".to_string(),
            type_hint: "string".to_string(),
            description: "Raw data including Telnet commands (as hex)".to_string(),
            required: false,
        },
    ])
});

/// Telnet client protocol action handler
pub struct TelnetClientProtocol;

impl TelnetClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for TelnetClientProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "send_command".to_string(),
                description: "Send a text command to the Telnet server".to_string(),
                parameters: vec![Parameter {
                    name: "command".to_string(),
                    type_hint: "string".to_string(),
                    description: "The command text to send (newline will be appended)".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "send_command",
                    "command": "ls -la"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "send_text".to_string(),
                description: "Send raw text to the Telnet server (no newline added)".to_string(),
                parameters: vec![Parameter {
                    name: "text".to_string(),
                    type_hint: "string".to_string(),
                    description: "The text to send".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "send_text",
                    "text": "yes"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from the Telnet server".to_string(),
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
                name: "send_command".to_string(),
                description: "Send command in response to server output".to_string(),
                parameters: vec![Parameter {
                    name: "command".to_string(),
                    type_hint: "string".to_string(),
                    description: "The command text to send".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "send_command",
                    "command": "whoami"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "send_text".to_string(),
                description: "Send raw text in response to server output".to_string(),
                parameters: vec![Parameter {
                    name: "text".to_string(),
                    type_hint: "string".to_string(),
                    description: "The text to send".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "send_text",
                    "text": "password123"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "wait_for_more".to_string(),
                description: "Wait for more data before responding".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "wait_for_more"
                }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "Telnet"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "telnet_connected",
                "Triggered when Telnet client connects to server",
                json!({"type": "placeholder", "event_id": "telnet_connected"}),
            ),
            EventType::new(
                "telnet_data_received",
                "Triggered when Telnet client receives data from server",
                json!({"type": "placeholder", "event_id": "telnet_data_received"}),
            ),
            EventType::new(
                "telnet_option_negotiated",
                "Triggered when Telnet option negotiation occurs",
                json!({"type": "placeholder", "event_id": "telnet_option_negotiated"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>Telnet"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "telnet",
            "telnet client",
            "connect to telnet",
            "remote shell",
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("Raw TCP with Telnet option negotiation")
            .llm_control("Send commands and respond to server output, automatic option negotiation")
            .e2e_testing("telnetd or netcat as test server")
            .build()
    }
    fn description(&self) -> &'static str {
        "Telnet client for connecting to Telnet servers and executing commands"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to Telnet at localhost:23 and run 'whoami' command"
    }
    fn group_name(&self) -> &'static str {
        "Infrastructure"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls Telnet session
            json!({
                "type": "open_client",
                "remote_addr": "localhost:23",
                "base_stack": "telnet",
                "instruction": "Login and execute 'whoami' command"
            }),
            // Script mode: Code-based command handling
            json!({
                "type": "open_client",
                "remote_addr": "localhost:23",
                "base_stack": "telnet",
                "event_handlers": [{
                    "event_pattern": "telnet_data_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<telnet_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed command sequence
            json!({
                "type": "open_client",
                "remote_addr": "localhost:23",
                "base_stack": "telnet",
                "event_handlers": [
                    {
                        "event_pattern": "telnet_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "send_command",
                                "command": "whoami"
                            }]
                        }
                    },
                    {
                        "event_pattern": "telnet_data_received",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "disconnect"
                            }]
                        }
                    }
                ]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for TelnetClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::telnet::TelnetClient;
            TelnetClient::connect_with_llm_actions(
                ctx.remote_addr,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.client_id,
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
            "send_command" => {
                let command = action
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing 'command' field")?;

                // Append newline for command
                let data = format!("{}\r\n", command).into_bytes();
                Ok(ClientActionResult::SendData(data))
            }
            "send_text" => {
                let text = action
                    .get("text")
                    .and_then(|v| v.as_str())
                    .context("Missing 'text' field")?;

                let data = text.as_bytes().to_vec();
                Ok(ClientActionResult::SendData(data))
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown Telnet client action: {}",
                action_type
            )),
        }
    }
}
