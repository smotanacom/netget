//! gRPC client protocol actions implementation

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

/// gRPC client connected event
pub static GRPC_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "grpc_connected",
        "gRPC client initialized and ready to call RPC methods",
        json!({
            "type": "call_grpc_method",
            "service": "calculator.Calculator",
            "method": "Add",
            "request": {"a": 5, "b": 3}
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "server_addr".to_string(),
            type_hint: "string".to_string(),
            description: "gRPC server address".to_string(),
            required: true,
        },
        Parameter {
            name: "services".to_string(),
            type_hint: "array".to_string(),
            description: "Available service names from schema".to_string(),
            required: true,
        },
    ])
});

/// gRPC client response received event
pub static GRPC_CLIENT_RESPONSE_RECEIVED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "grpc_response_received",
        "gRPC response received from server",
        json!({
            "type": "call_grpc_method",
            "service": "calculator.Calculator",
            "method": "Multiply",
            "request": {"a": 2, "b": 3}
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "service".to_string(),
            type_hint: "string".to_string(),
            description: "Service name".to_string(),
            required: true,
        },
        Parameter {
            name: "method".to_string(),
            type_hint: "string".to_string(),
            description: "Method name".to_string(),
            required: true,
        },
        Parameter {
            name: "response".to_string(),
            type_hint: "object".to_string(),
            description: "Response message as JSON".to_string(),
            required: true,
        },
    ])
});

/// gRPC client error event
pub static GRPC_CLIENT_ERROR_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "grpc_error",
        "gRPC error received from server",
        json!({
            "type": "wait_for_more"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "service".to_string(),
            type_hint: "string".to_string(),
            description: "Service name".to_string(),
            required: true,
        },
        Parameter {
            name: "method".to_string(),
            type_hint: "string".to_string(),
            description: "Method name".to_string(),
            required: true,
        },
        Parameter {
            name: "code".to_string(),
            type_hint: "string".to_string(),
            description: "gRPC status code".to_string(),
            required: true,
        },
        Parameter {
            name: "message".to_string(),
            type_hint: "string".to_string(),
            description: "Error message".to_string(),
            required: true,
        },
    ])
});

/// gRPC client protocol action handler
pub struct GrpcClientProtocol;

impl GrpcClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for GrpcClientProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
                ParameterDefinition {
                    name: "proto_schema".to_string(),
                    description: "Protobuf schema definition (base64 FileDescriptorSet, .proto file path, or inline .proto text)".to_string(),
                    type_hint: "string".to_string(),
                    required: true,
                    example: json!("CpUCCg9jYWxjdWxhdG9yLnByb3RvEgpjYWxjdWxhdG9yIikKCkFkZFJlcXVlc3QSCwoDYQgBIAEoBVIBYRILCgNiCAIgASgFUgFiIiIKC0FkZFJlc3BvbnNlEhMKBnJlc3VsdBgBIAEoBVIGcmVzdWx0MkIKCkNhbGN1bGF0b3ISNAoDQWRkEhYuY2FsY3VsYXRvci5BZGRSZXF1ZXN0Gh0uY2FsY3VsYXRvci5BZGRSZXNwb25zZSIAYgZwcm90bzM="),
                },
                ParameterDefinition {
                    name: "use_tls".to_string(),
                    description: "Whether to use TLS for connection (default: false)".to_string(),
                    type_hint: "boolean".to_string(),
                    required: false,
                    example: json!(false),
                },
            ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "call_grpc_method".to_string(),
                description: "Call a gRPC method with the given request".to_string(),
                parameters: vec![
                    Parameter {
                        name: "service".to_string(),
                        type_hint: "string".to_string(),
                        description: "Fully qualified service name (e.g., 'calculator.Calculator')"
                            .to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "method".to_string(),
                        type_hint: "string".to_string(),
                        description: "Method name (e.g., 'Add')".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "request".to_string(),
                        type_hint: "object".to_string(),
                        description: "Request message as JSON object".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "metadata".to_string(),
                        type_hint: "object".to_string(),
                        description: "Optional gRPC metadata (headers)".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "call_grpc_method",
                    "service": "calculator.Calculator",
                    "method": "Add",
                    "request": {"a": 5, "b": 3},
                    "metadata": {"auth-token": "secret"}
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Disconnect from the gRPC server".to_string(),
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
                name: "call_grpc_method".to_string(),
                description: "Call another gRPC method in response to received data".to_string(),
                parameters: vec![
                    Parameter {
                        name: "service".to_string(),
                        type_hint: "string".to_string(),
                        description: "Fully qualified service name".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "method".to_string(),
                        type_hint: "string".to_string(),
                        description: "Method name".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "request".to_string(),
                        type_hint: "object".to_string(),
                        description: "Request message as JSON object".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "metadata".to_string(),
                        type_hint: "object".to_string(),
                        description: "Optional gRPC metadata (headers)".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "call_grpc_method",
                    "service": "calculator.Calculator",
                    "method": "Multiply",
                    "request": {"a": 2, "b": 3}
                }),
                log_template: None,
            },
            ActionDefinition {
                name: "wait_for_more".to_string(),
                description: "Wait without making another call".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "wait_for_more"
                }),
                log_template: None,
            },
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "gRPC"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            EventType::new(
                "grpc_connected",
                "Triggered when gRPC client connects to server",
                json!({"type": "placeholder", "event_id": "grpc_connected"}),
            ),
            EventType::new(
                "grpc_response_received",
                "Triggered when gRPC client receives a response",
                json!({"type": "placeholder", "event_id": "grpc_response_received"}),
            ),
            EventType::new(
                "grpc_error",
                "Triggered when gRPC client receives an error",
                json!({"type": "placeholder", "event_id": "grpc_error"}),
            ),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP/2>gRPC"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["grpc", "grpc client", "connect to grpc", "rpc"]
    }
    /// The gRPC client shells out to `protoc`, exactly as the server does.
    ///
    /// `src/client/grpc/mod.rs` runs `protoc --descriptor_set_out=/dev/stdout` to compile the
    /// inline `.proto` text a caller supplies, so without the binary on PATH that schema form
    /// cannot load at all. The *server* has declared this since the dependency mechanism was
    /// adopted; the client shells out to the same binary and did not, so a host without
    /// `protoc` was told about one half of the pair and discovered the other from a failure.
    ///
    /// Note this is a genuine **runtime** dependency, unlike `etcd`/`kubernetes`/`zookeeper`,
    /// whose `protoc` use is in a build script and is finished before the binary exists.
    fn get_dependencies(&self) -> Vec<crate::protocol::dependencies::ProtocolDependency> {
        let mut deps =
            crate::llm::actions::protocol_trait::default_dependencies_from_privilege(self);
        deps.push(crate::protocol::dependencies::ProtocolDependency::ToolInPath("protoc"));
        deps
    }

    /// A base64 `FileDescriptorSet` is decoded directly (`mod.rs::load_schema`) and needs no
    /// `protoc`; every other schema form is compiled by it.
    fn startup_dependencies(
        &self,
        startup_params: Option<&serde_json::Value>,
    ) -> Vec<crate::protocol::dependencies::ProtocolDependency> {
        let precompiled = startup_params
            .and_then(|p| p.get("proto_schema"))
            .and_then(|s| s.as_str())
            .map(is_base64_descriptor_set)
            .unwrap_or(false);
        let mut deps = self.get_dependencies();
        if precompiled {
            deps.retain(|d| {
                *d != crate::protocol::dependencies::ProtocolDependency::ToolInPath("protoc")
            });
        }
        deps
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .implementation("tonic gRPC client with dynamic protobuf schema support")
            .llm_control("Full control over RPC calls (service, method, request data)")
            .e2e_testing("Local gRPC server or public gRPC APIs")
            .build()
    }
    fn description(&self) -> &'static str {
        "gRPC client for calling RPC services"
    }
    fn example_prompt(&self) -> &'static str {
        "Connect to gRPC server at localhost:50051 and call Calculator.Add with a=5, b=3"
    }
    fn group_name(&self) -> &'static str {
        "RPC & API"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM controls gRPC method calls
            json!({
                "type": "open_client",
                "remote_addr": "localhost:50051",
                "base_stack": "grpc",
                "instruction": "Call the Calculator.Add method with a=5 and b=3",
                "startup_params": {
                    "proto_schema": "CpUCCg9jYWxjdWxhdG9yLnByb3RvEgpjYWxjdWxhdG9yIikKCkFkZFJlcXVlc3QSCwoDYQgBIAEoBVIBYRILCgNiCAIgASgFUgFiIiIKC0FkZFJlc3BvbnNlEhMKBnJlc3VsdBgBIAEoBVIGcmVzdWx0MkIKCkNhbGN1bGF0b3ISNAoDQWRkEhYuY2FsY3VsYXRvci5BZGRSZXF1ZXN0Gh0uY2FsY3VsYXRvci5BZGRSZXNwb25zZSIAYgZwcm90bzM="
                }
            }),
            // Script mode: Code-based gRPC call handling
            json!({
                "type": "open_client",
                "remote_addr": "localhost:50051",
                "base_stack": "grpc",
                "startup_params": {
                    "proto_schema": "CpUCCg9jYWxjdWxhdG9yLnByb3RvEgpjYWxjdWxhdG9yIikKCkFkZFJlcXVlc3QSCwoDYQgBIAEoBVIBYRILCgNiCAIgASgFUgFiIiIKC0FkZFJlc3BvbnNlEhMKBnJlc3VsdBgBIAEoBVIGcmVzdWx0MkIKCkNhbGN1bGF0b3ISNAoDQWRkEhYuY2FsY3VsYXRvci5BZGRSZXF1ZXN0Gh0uY2FsY3VsYXRvci5BZGRSZXNwb25zZSIAYgZwcm90bzM="
                },
                "event_handlers": [{
                    "event_pattern": "grpc_response_received",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<grpc_client_handler>"
                    }
                }]
            }),
            // Static mode: Fixed gRPC method call
            json!({
                "type": "open_client",
                "remote_addr": "localhost:50051",
                "base_stack": "grpc",
                "startup_params": {
                    "proto_schema": "CpUCCg9jYWxjdWxhdG9yLnByb3RvEgpjYWxjdWxhdG9yIikKCkFkZFJlcXVlc3QSCwoDYQgBIAEoBVIBYRILCgNiCAIgASgFUgFiIiIKC0FkZFJlc3BvbnNlEhMKBnJlc3VsdBgBIAEoBVIGcmVzdWx0MkIKCkNhbGN1bGF0b3ISNAoDQWRkEhYuY2FsY3VsYXRvci5BZGRSZXF1ZXN0Gh0uY2FsY3VsYXRvci5BZGRSZXNwb25zZSIAYgZwcm90bzM="
                },
                "event_handlers": [
                    {
                        "event_pattern": "grpc_connected",
                        "handler": {
                            "type": "static",
                            "actions": [{
                                "type": "call_grpc_method",
                                "service": "calculator.Calculator",
                                "method": "Add",
                                "request": {"a": 5, "b": 3}
                            }]
                        }
                    },
                    {
                        "event_pattern": "grpc_response_received",
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
impl Client for GrpcClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::grpc::GrpcClient;
            GrpcClient::connect_with_llm_actions(
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
            "call_grpc_method" => {
                let service = action
                    .get("service")
                    .and_then(|v| v.as_str())
                    .context("Missing 'service' field")?
                    .to_string();

                let method = action
                    .get("method")
                    .and_then(|v| v.as_str())
                    .context("Missing 'method' field")?
                    .to_string();

                let request = action
                    .get("request")
                    .context("Missing 'request' field")?
                    .clone();

                let metadata = action.get("metadata").and_then(|v| v.as_object()).cloned();

                // Return custom result with RPC call data
                Ok(ClientActionResult::Custom {
                    name: "grpc_call".to_string(),
                    data: json!({
                        "service": service,
                        "method": method,
                        "request": request,
                        "metadata": metadata,
                    }),
                })
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown gRPC client action: {}",
                action_type
            )),
        }
    }
}

/// Whether `schema` is a base64-encoded `FileDescriptorSet`, the one form the client loads
/// without `protoc`.
fn is_base64_descriptor_set(schema: &str) -> bool {
    use base64::Engine as _;
    use prost::Message as _;
    base64::engine::general_purpose::STANDARD
        .decode(schema)
        .map(|bytes| prost_types::FileDescriptorSet::decode(bytes.as_slice()).is_ok())
        .unwrap_or(false)
}
