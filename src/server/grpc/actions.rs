//! gRPC protocol actions implementation

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;
use tracing::debug;

/// gRPC protocol action handler
pub struct GrpcProtocol;

impl GrpcProtocol {
    pub fn new() -> Self {
        Self
    }

    fn execute_grpc_unary_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let message = action
            .get("message")
            .context("Missing 'message' parameter in grpc_unary_response")?;

        debug!("gRPC unary response: {}", serde_json::to_string(message)?);

        // Return as Custom action result so server can encode to protobuf
        Ok(ActionResult::Custom {
            name: "grpc_unary_response".to_string(),
            data: json!({ "message": message }),
        })
    }

    fn execute_grpc_error(&self, action: serde_json::Value) -> Result<ActionResult> {
        let code = action
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("INTERNAL");

        let message = action
            .get("message")
            .and_then(|v| v.as_str())
            .context("Missing 'message' parameter in grpc_error")?;

        debug!("gRPC error response: {} - {}", code, message);

        // Return as Custom action result so server can construct proper gRPC error
        Ok(ActionResult::Custom {
            name: "grpc_error".to_string(),
            data: json!({
                "code": code,
                "message": message
            }),
        })
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for GrpcProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        use crate::llm::actions::ParameterDefinition;
        vec![
                ParameterDefinition {
                    name: "proto_schema".to_string(),
                    type_hint: "string".to_string(),
                    description: "Protobuf schema definition. IMPORTANT: For LLM responses, use inline .proto text (proto3 syntax). LLMs should NOT use base64-encoded FileDescriptorSet (truncation issues). Alternatively, provide path to .proto file on disk.".to_string(),
                    required: true,
                    example: json!("syntax = \"proto3\"; package test; service UserService { rpc GetUser(UserId) returns (User); } message UserId { int32 id = 1; } message User { int32 id = 1; string name = 2; string email = 3; }"),
                },
            ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // None. `reload_schema`, `list_services` and `describe_method` used to be declared
        // here. All three built an `ActionResult::Custom` that no consumer in mod.rs matched
        // (its loop handles `grpc_unary_response` and `grpc_error` and ignores the rest), so
        // they did nothing on any path. `reload_schema` could not have worked in any case:
        // `descriptor_pool` is an immutable `Arc<DescriptorPool>` with no reload channel.
        vec![]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![grpc_unary_response_action(), grpc_error_action()]
    }
    fn protocol_name(&self) -> &'static str {
        "gRPC"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_grpc_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>HTTP2>GRPC"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["grpc", "grpcserver", "protobuf"]
    }

    /// `protoc` must be on PATH at **runtime**, not merely at build time.
    ///
    /// `proto_schema` is a required startup parameter and every path that turns it into a
    /// descriptor set shells out to `protoc` — `compile_proto_file` for a path on disk and
    /// `compile_proto_text` for inline proto3 source (`mod.rs`, both via `Command::new`). So a
    /// host without `protoc` cannot start a gRPC server at all, whatever the schema looks like.
    /// gRPC is the only protocol that shells out to anything.
    ///
    /// Declaring it here means `server_startup` refuses with the installation hint before
    /// registering the server, and the TUI and the model's protocol list exclude gRPC on a host
    /// that lacks it, rather than everyone discovering it from a failure part-way through
    /// startup.
    ///
    /// Note this is unrelated to `etcd`/`kubernetes`/`zookeeper`, which need `protoc` to
    /// *compile* (prost/tonic build scripts) and not to run.
    fn get_dependencies(&self) -> Vec<crate::protocol::dependencies::ProtocolDependency> {
        let mut deps =
            crate::llm::actions::protocol_trait::default_dependencies_from_privilege(self);
        deps.push(crate::protocol::dependencies::ProtocolDependency::ToolInPath("protoc"));
        deps
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{DevelopmentState, ProtocolMetadataV2};

        ProtocolMetadataV2::builder()
            // Beta, September 2026. grpcurl — grpc-go, the reference implementation, and
            // deliberately NOT tonic, which this server links but does not use — completes a
            // real unary RPC and reads back a real error status. Neither test is #[ignore]d,
            // neither skips when the binary is absent, and the peer is not the crate the
            // server frames with.
            .state(DevelopmentState::Beta)
            .implementation(
                "prost-reflect over a hand-routed hyper HTTP/2 server. The schema is compiled \
                 once at startup by protoc, which must be on PATH unless a pre-built \
                 FileDescriptorSet is supplied.",
            )
            .llm_control(
                "The body of every unary RPC, as JSON keyed by protobuf field name, plus the \
                 gRPC status code on failure. The schema is fixed at startup and cannot be \
                 changed at runtime.",
            )
            .e2e_testing(
                "REAL THIRD-PARTY CLIENT, both paths: grpcurl 1.9.4 (grpc-go) in \
                 tests/server/grpc/real_client_test.rs. Neither test is #[ignore]d and neither \
                 is skip-gated — they FAIL, naming `brew install grpcurl`, when grpcurl is \
                 absent. One completes a unary RPC and asserts grpcurl decoded our protobuf \
                 response field by field; the other asserts it read back our NOT_FOUND code and \
                 grpc-message. grpc-go is deliberately not tonic, which this crate depends on \
                 but this server does not use, so the evidence is not circular. \
                 The success test was WRITTEN #[ignore]d, because when it was written it \
                 failed: grpcurl rejected EVERY successful unary RPC with `Internal: server \
                 closed the stream without sending trailers`. mod.rs wrote grpc-status into the \
                 INITIAL HEADERS and emitted no trailers at all; a success has a non-empty body \
                 so hyper sent HEADERS then DATA and ended the stream with nothing, which \
                 grpc-go refuses. An ERROR has an empty body and became a valid Trailers-Only \
                 response by accident, which is exactly why the error path worked and the \
                 success path did not — NetGet could report a FAILURE to a real gRPC client and \
                 could not report a SUCCESS. Fixed: the success reply is now a message frame \
                 followed by real trailers, the error reply stays Trailers-Only, and both \
                 placements are asserted so neither can drift. \
                 UNPROVEN: streaming of any kind (unary only), server reflection (not served), \
                 and compression (rejected). The rest of tests/server/grpc is reqwest with \
                 http2_prior_knowledge, which does not implement gRPC and never looks at \
                 trailers — which is why it could not see the bug and is not the evidence.",
            )
            .notes(
                "Unary RPCs only - no client, server or bidirectional streaming. Server \
                 reflection is NOT served (tonic-reflection is a dependency and is referenced \
                 nowhere in src/), so a reflection call gets 12 UNIMPLEMENTED and grpcurl needs \
                 -proto or -protoset. Request compression is rejected. bytes fields cross the \
                 action boundary as base64.",
            )
            .max_inbound_bytes(crate::server::grpc::MAX_REQUEST_BYTES)
            .build()
    }
    fn description(&self) -> &'static str {
        "gRPC server"
    }
    fn example_prompt(&self) -> &'static str {
        "Start a gRPC server on port 50051 with this schema: service UserService { rpc GetUser(UserId) returns (User); }"
    }
    fn group_name(&self) -> &'static str {
        "AI & API"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        StartupExamples::new(
            // LLM mode: instruction-based
            json!({
                "type": "open_server",
                "port": 50051,
                "base_stack": "grpc",
                "instruction": "gRPC server with UserService. Respond to GetUser with user details, CreateUser with success confirmation",
                "startup_params": {
                    "proto_schema": "syntax = \"proto3\"; package test; service UserService { rpc GetUser(UserId) returns (User); } message UserId { int32 id = 1; } message User { int32 id = 1; string name = 2; string email = 3; }"
                }
            }),
            // Script mode: event_handlers with script handler
            json!({
                "type": "open_server",
                "port": 50051,
                "base_stack": "grpc",
                "startup_params": {
                    "proto_schema": "syntax = \"proto3\"; package test; service Calculator { rpc Add(AddRequest) returns (AddResponse); } message AddRequest { int32 a = 1; int32 b = 2; } message AddResponse { int32 result = 1; }"
                },
                "event_handlers": [{
                    "event_pattern": "grpc_unary_request",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "req = event.get('request', {})\nresult = req.get('a', 0) + req.get('b', 0)\naction('grpc_unary_response', message={'result': result})"
                    }
                }]
            }),
            // Static mode: event_handlers with static actions
            json!({
                "type": "open_server",
                "port": 50051,
                "base_stack": "grpc",
                "startup_params": {
                    "proto_schema": "syntax = \"proto3\"; package test; service Greeter { rpc SayHello(HelloRequest) returns (HelloReply); } message HelloRequest { string name = 1; } message HelloReply { string message = 1; }"
                },
                "event_handlers": [{
                    "event_pattern": "grpc_unary_request",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "grpc_unary_response",
                            "message": {"message": "Hello, World!"}
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for GrpcProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::grpc::GrpcServer;
            GrpcServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.server_id,
                ctx.startup_params,
            )
            .await
        })
    }
    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "grpc_unary_response" => self.execute_grpc_unary_response(action),
            "grpc_error" => self.execute_grpc_error(action),
            _ => Err(anyhow::anyhow!("Unknown gRPC action: {}", action_type)),
        }
    }
}

// ============================================================================
// Action Definitions
// ============================================================================

fn grpc_unary_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "grpc_unary_response".to_string(),
        description: "Send gRPC unary response with JSON message".to_string(),
        parameters: vec![Parameter {
            name: "message".to_string(),
            type_hint: "object".to_string(),
            description: "Response message as JSON object matching protobuf schema".to_string(),
            required: true,
        }],
        example: json!({
            "type": "grpc_unary_response",
            "message": {
                "id": 123,
                "name": "Alice",
                "email": "alice@example.com"
            }
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> gRPC response")
                .with_debug("gRPC grpc_unary_response"),
        ),
    }
}

fn grpc_error_action() -> ActionDefinition {
    ActionDefinition {
        name: "grpc_error".to_string(),
        description: "Return gRPC error with status code and message".to_string(),
        parameters: vec![
            Parameter {
                name: "code".to_string(),
                type_hint: "string".to_string(),
                description:
                    "gRPC status code (OK, CANCELLED, INVALID_ARGUMENT, NOT_FOUND, INTERNAL, etc.)"
                        .to_string(),
                required: false,
            },
            Parameter {
                name: "message".to_string(),
                type_hint: "string".to_string(),
                description: "Error message".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "grpc_error",
            "code": "NOT_FOUND",
            "message": "User not found"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> gRPC error {code}")
                .with_debug("gRPC grpc_error: code={code}, message={message}"),
        ),
    }
}

// ============================================================================
// gRPC Action Constants
// ============================================================================

pub static GRPC_UNARY_RESPONSE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| grpc_unary_response_action());
pub static GRPC_ERROR_ACTION: LazyLock<ActionDefinition> = LazyLock::new(|| grpc_error_action());

// ============================================================================
// gRPC Event Type Constants
// ============================================================================

/// gRPC unary request event - triggered when client makes a unary RPC call
pub static GRPC_UNARY_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "grpc_unary_request",
        "gRPC unary RPC request received from client",
        json!({
            "type": "grpc_unary_response",
            "message": {
                "id": 123,
                "name": "Alice",
                "email": "alice@example.com"
            }
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "service".to_string(),
            type_hint: "string".to_string(),
            description: "Service name (e.g., 'UserService')".to_string(),
            required: true,
        },
        Parameter {
            name: "method".to_string(),
            type_hint: "string".to_string(),
            description: "Method name (e.g., 'GetUser')".to_string(),
            required: true,
        },
        Parameter {
            name: "request".to_string(),
            type_hint: "object".to_string(),
            description: "Request message as JSON object".to_string(),
            required: true,
        },
        Parameter {
            name: "expected_response_schema".to_string(),
            type_hint: "object".to_string(),
            description: "Expected response schema as JSON Schema".to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        GRPC_UNARY_RESPONSE_ACTION.clone(),
        GRPC_ERROR_ACTION.clone(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("gRPC {client_ip} {service}/{method}")
            .with_debug("gRPC method {service}/{method} from {client_ip}:{client_port}")
            .with_trace("gRPC: {json_pretty(.)}"),
    )
});

/// Get gRPC event types
pub fn get_grpc_event_types() -> Vec<EventType> {
    vec![GRPC_UNARY_REQUEST_EVENT.clone()]
}
