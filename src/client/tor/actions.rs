//! Tor client protocol actions implementation

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

/// Tor client connected event
pub static TOR_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "tor_connected",
        "Tor client successfully connected through Tor network",
        json!({
            "type": "send_tor_data",
            "data_hex": "48656c6c6f"
        }),
    )
    .with_parameters(vec![Parameter {
        name: "target".to_string(),
        type_hint: "string".to_string(),
        description: "Target address (can be regular hostname:port or .onion:port)".to_string(),
        required: true,
    }])
});

/// Tor client data received event
pub static TOR_CLIENT_DATA_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "tor_data_received",
        "Data received from destination through Tor",
        json!({
            "type": "send_tor_data",
            "data_hex": "48656c6c6f"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "data_hex".to_string(),
            type_hint: "string".to_string(),
            description: "The data received (as hex string)".to_string(),
            required: true,
        },
        Parameter {
            name: "data_length".to_string(),
            type_hint: "number".to_string(),
            description: "Length of data in bytes".to_string(),
            required: true,
        },
    ])
});

/// Tor bootstrap complete event (directory consensus downloaded)
#[cfg(feature = "tor")]
pub static TOR_BOOTSTRAP_COMPLETE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "tor_bootstrap_complete",
        "Tor client finished bootstrapping and downloaded network consensus",
        json!({
            "type": "send_tor_data",
            "data_hex": "48656c6c6f"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "relay_count".to_string(),
            type_hint: "number".to_string(),
            description: "Number of relays in consensus".to_string(),
            required: true,
        },
        Parameter {
            name: "valid_after".to_string(),
            type_hint: "string".to_string(),
            description: "Consensus valid-after timestamp".to_string(),
            required: true,
        },
    ])
});

/// Tor client protocol action handler
pub struct TorClientProtocol;

impl TorClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for TorClientProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "directory_server".to_string(),
                type_hint: "string".to_string(),
                description: "Custom Tor relay address (e.g., '127.0.0.1:9001') to bootstrap from, using BEGIN_DIR (directory over circuit) instead of the public Tor network. Pair with a local tor_relay server for offline use. Exactly one of this or allow_public_tor_network must be given; without either the client refuses to start.".to_string(),
                required: false,
                example: json!("127.0.0.1:9001"),
            },
            ParameterDefinition {
                name: crate::client::tor::ALLOW_PUBLIC_TOR_NETWORK_PARAM.to_string(),
                type_hint: "boolean".to_string(),
                description: "Opt in to bootstrapping against the REAL Tor directory authorities on the public internet. Off by default: bootstrapping contacts third parties before the requested destination is even looked at, so opening a Tor client would otherwise reach the internet no matter what you asked it to connect to. Set true only when you intend outbound public traffic.".to_string(),
                required: false,
                example: json!(true),
            },
        ]
    }

    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "send_tor_data".to_string(),
                description: "Send raw data to the destination through Tor".to_string(),
                parameters: vec![Parameter {
                    name: "data_hex".to_string(),
                    type_hint: "string".to_string(),
                    description: "Hexadecimal encoded data to send".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "send_tor_data",
                    "data_hex": "48656c6c6f"
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from the Tor circuit".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "disconnect"
                }),
                log_template: None,
            },
            #[cfg(feature = "tor")]
            ActionDefinition {
                name: "get_consensus_info".to_string(),
                description: "Get network consensus metadata (relay count, validity times)"
                    .to_string(),
                parameters: vec![],
                example: json!({
                    "type": "get_consensus_info"
                }),
                log_template: None,
            },
            #[cfg(feature = "tor")]
            ActionDefinition {
                name: "list_relays".to_string(),
                description: "List relays from the Tor network consensus".to_string(),
                parameters: vec![Parameter {
                    name: "limit".to_string(),
                    type_hint: "number".to_string(),
                    description: "Maximum number of relays to return (default: 100)".to_string(),
                    required: false,
                }],
                example: json!({
                    "type": "list_relays",
                    "limit": 50
                }),
                log_template: None,
            },
            #[cfg(feature = "tor")]
            ActionDefinition {
                name: "search_relays".to_string(),
                description: "Search for relays matching criteria (flags, nickname pattern)"
                    .to_string(),
                parameters: vec![
                    Parameter {
                        name: "flags".to_string(),
                        type_hint: "array".to_string(),
                        description: "Required flags (e.g., [\"Guard\", \"Exit\", \"Fast\"])"
                            .to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "nickname".to_string(),
                        type_hint: "string".to_string(),
                        description: "Nickname pattern to match".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "limit".to_string(),
                        type_hint: "number".to_string(),
                        description: "Maximum results (default: 100)".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "search_relays",
                    "flags": ["Exit", "Fast"],
                    "limit": 20
                }),
                log_template: None,
            },
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "send_tor_data".to_string(),
                description: "Send data in response to received data".to_string(),
                parameters: vec![Parameter {
                    name: "data_hex".to_string(),
                    type_hint: "string".to_string(),
                    description: "Hexadecimal encoded data to send".to_string(),
                    required: true,
                }],
                example: json!({
                    "type": "send_tor_data",
                    "data_hex": "48656c6c6f"
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
        "Tor"
    }
    /// Clone the statics the client actually emits, rather than rebuilding them.
    ///
    /// The two rebuilt copies carried `{"type": "placeholder", ...}` — an example
    /// `execute_action` refuses outright as `Unknown Tor client action: placeholder` — and
    /// no parameters at all. `get_event_types()` is the copy the model is shown, so it was
    /// given no fields and one example that cannot work, while the correct `send_tor_data`
    /// examples sat unused in the statics three screens above. The bootstrap event below
    /// was already cloned properly, which made the drift visible in the same function.
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            TOR_CLIENT_CONNECTED_EVENT.clone(),
            TOR_CLIENT_DATA_RECEIVED_EVENT.clone(),
            // Emitted but never advertised until now: mod.rs raises it once the Tor
            // directory bootstrap finishes, and nothing told the model it existed, so no
            // handler could be written for the one event that says the circuit is usable.
            TOR_BOOTSTRAP_COMPLETE_EVENT.clone(),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "Tor>TCP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["tor", "tor client", "onion", "anonymous", "privacy"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("Arti Tor client (pure Rust Tor implementation)")
            .llm_control("Full control over data sent/received through Tor circuits")
            .e2e_testing("Connect to onion services or regular hosts through Tor")
            .notes(
                "REACHES THE PUBLIC INTERNET ONLY ON EXPLICIT OPT-IN. arti's \
                 create_bootstrapped() contacts the real Tor directory authorities BEFORE it \
                 looks at the requested address (~14s), so merely opening this client used to \
                 make outbound connections to third parties whatever the destination was. It \
                 now refuses to start unless the caller passes either `directory_server` (a \
                 directory to bootstrap from, e.g. a local `127.0.0.1:9001` tor_relay) or \
                 `allow_public_tor_network: true`. Passing both is refused rather than \
                 guessed. Bootstrap is 10-30s even when permitted.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "Tor client for anonymous connections through the Tor network"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to example.onion:80 through Tor and send HTTP GET request"
    }
    fn group_name(&self) -> &'static str {
        "VPN & Tunneling"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls Tor connection.
            //
            // Every example carries a bootstrap choice, because without one the client refuses
            // to start. An example a model can copy verbatim into a refusal is worse than no
            // example. The public network is the opt-in here; swap in
            // `{"directory_server": "127.0.0.1:9001"}` to bootstrap from a local tor_relay.
            json!({
                "type": "open_client",
                "remote_addr": "example.com:80",
                "base_stack": "tor",
                "startup_params": {"allow_public_tor_network": true},
                "instruction": "Connect to the destination through Tor and send an HTTP GET request"
            }),
            // Script mode: Code-based Tor handling
            json!({
                "type": "open_client",
                "remote_addr": "example.com:80",
                "base_stack": "tor",
                "startup_params": {"allow_public_tor_network": true},
                "event_handlers": [{
                    "event_pattern": "tor_connected",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<tor_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed Tor action
            json!({
                "type": "open_client",
                "remote_addr": "example.com:80",
                "base_stack": "tor",
                "startup_params": {"allow_public_tor_network": true},
                "event_handlers": [
                    {
                        "event_pattern": "tor_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "send_tor_data",
                                "data_hex": "474554202f20485454502f312e310d0a486f73743a206578616d706c652e636f6d0d0a0d0a"
                            }]
                        }
                    },
                    {
                        "event_pattern": "tor_data_received",
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
impl Client for TorClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::tor::TorClient;
            TorClient::connect_with_llm_actions(
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
            "send_tor_data" => {
                let data_hex = action
                    .get("data_hex")
                    .and_then(|v| v.as_str())
                    .context("Missing 'data_hex' field")?;

                let data = hex::decode(data_hex).context("Invalid hex data")?;

                Ok(ClientActionResult::SendData(data))
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),

            // Directory query actions (processed async in connect loop)
            #[cfg(feature = "tor")]
            "get_consensus_info" | "list_relays" | "search_relays" => {
                Ok(ClientActionResult::Custom {
                    name: action_type.to_string(),
                    data: action.clone(),
                })
            }

            _ => Err(anyhow::anyhow!(
                "Unknown Tor client action: {}",
                action_type
            )),
        }
    }
}
