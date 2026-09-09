//! Redis client protocol actions implementation

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

/// Redis client connected event
pub static REDIS_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "redis_connected",
        "Redis client successfully connected to server",
        json!({
            "type": "execute_redis_command",
            "command": "GET mykey"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "remote_addr".to_string(),
        type_hint: "string".to_string(),
        description: "Redis server address".to_string(),
        required: true,
    }])
});

/// Redis client response received event
pub static REDIS_CLIENT_RESPONSE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "redis_response_received",
        "Response received from Redis server",
        json!({
            "type": "execute_redis_command",
            "command": "SET result OK"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "response".to_string(),
        type_hint: "string".to_string(),
        description: "The response line from Redis".to_string(),
        required: true,
    }])
});

/// Redis client protocol action handler
pub struct RedisClientProtocol;

impl RedisClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for RedisClientProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "execute_redis_command".to_string(),
                description: "Execute a Redis command".to_string(),
                parameters: vec![Parameter {
                    name: "command".to_string(),
                    type_hint: "string".to_string(),
                    description: "Redis command (e.g., GET key, SET key value)".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "execute_redis_command",
                    "command": "GET mykey"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from the Redis server".to_string(),
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
                name: "execute_redis_command".to_string(),
                description: "Execute a Redis command in response to received data".to_string(),
                parameters: vec![Parameter {
                    name: "command".to_string(),
                    type_hint: "string".to_string(),
                    description: "Redis command".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "execute_redis_command",
                    "command": "SET result OK"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "wait_for_more".to_string(),
                description:
                    "Take no action and wait for more data from the server. Use when the reply so \
                 far is incomplete."
                        .to_string(),
                parameters: vec![],
                example: json!({ "type": "wait_for_more" }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "Redis"
    }
    /// The **statics**, not fresh `EventType::new(..)` copies.
    ///
    /// This used to build a second, different `EventType` for each id: no parameters, and a
    /// `{"type": "placeholder"}` example. Those are what the registry and the model-facing
    /// docs surface, while the emitted events carry the statics above — two sources of truth
    /// for the same event id, and they had already drifted.
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            REDIS_CLIENT_CONNECTED_EVENT.clone(),
            REDIS_CLIENT_RESPONSE_RECEIVED_EVENT.clone(),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>Redis"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["redis", "redis client", "connect to redis"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("Direct TCP with simplified RESP parsing")
            .llm_control("Full control over Redis commands")
            .e2e_testing("Docker Redis container")
            .build()
    }
    fn description(&self) -> &'static str {
        "Redis client for key-value operations"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to Redis at localhost:6379 and get the value of 'user:123'"
    }
    fn group_name(&self) -> &'static str {
        "Database"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls Redis commands
            json!({
                "type": "open_client",
                "remote_addr": "localhost:6379",
                "base_stack": "redis",
                "instruction": "Get the value of 'user:123' and report its contents"
            }),
            // Script mode: Code-based deterministic responses
            json!({
                "type": "open_client",
                "remote_addr": "localhost:6379",
                "base_stack": "redis",
                "event_handlers": [{
                    "event_pattern": "redis_response_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<redis_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed Redis command on connect
            json!({
                "type": "open_client",
                "remote_addr": "localhost:6379",
                "base_stack": "redis",
                "event_handlers": [
                    {
                        "event_pattern": "redis_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "execute_redis_command",
                                "command": "GET status"
                            }]
                        }
                    },
                    {
                        "event_pattern": "redis_response_received",
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
impl Client for RedisClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::redis::RedisClient;
            RedisClient::connect_with_llm_actions(
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
            "execute_redis_command" => {
                let command = action
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("Missing 'command' field")?
                    .to_string();

                Ok(ClientActionResult::Custom {
                    name: "redis_command".to_string(),
                    data: json!({
                        "command": command,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            // CLAUDE.md lists wait_for_more as one of the three standard client
            // sync actions, and ftp/imap/doh/rip/tcp/dns all implement it. These two
            // did not, so returning it was an "unknown action" error the client then
            // swallowed -- two suites passed while sending an action the client
            // could never obey.
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown Redis client action: {}",
                action_type
            )),
        }
    }
}
