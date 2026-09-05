//! mDNS protocol actions implementation

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

/// mDNS protocol action handler
pub struct MdnsProtocol;

impl MdnsProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for MdnsProtocol {
    fn get_startup_parameters(&self) -> Vec<crate::llm::actions::ParameterDefinition> {
        use crate::llm::actions::ParameterDefinition;
        vec![
            ParameterDefinition {
                name: "service_type".to_string(),
                type_hint: "string".to_string(),
                description: "Service type (e.g., '_http._tcp.local.')".to_string(),
                required: false,
                example: json!("_http._tcp.local."),
            },
            ParameterDefinition {
                name: "service_name".to_string(),
                type_hint: "string".to_string(),
                description: "Service instance name".to_string(),
                required: false,
                example: json!("My Web Server"),
            },
            ParameterDefinition {
                name: "port".to_string(),
                type_hint: "number".to_string(),
                description: "Port advertised in the SRV record - the port clients will connect to. Defaults to the server's own port, or 8080 if that is 0".to_string(),
                required: false,
                example: json!(8080),
            },
            ParameterDefinition {
                name: "properties".to_string(),
                type_hint: "object".to_string(),
                description: "TXT record properties (key-value pairs)".to_string(),
                required: false,
                example: json!({"path": "/", "version": "1.0"}),
            },
            ParameterDefinition {
                name: "services".to_string(),
                type_hint: "array".to_string(),
                description: "Array of service definitions (each with service_type, service_name, port, properties)"
                    .to_string(),
                required: false,
                example: json!([{"service_type": "_http._tcp.local.", "service_name": "Web", "port": 8080, "properties": {"path": "/"}}]),
            },
        ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![register_mdns_service_action()]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        // mDNS is advertisement-based, no sync actions needed
        Vec::new()
    }
    fn protocol_name(&self) -> &'static str {
        "mDNS"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_mdns_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>UDP>mDNS"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["mdns", "bonjour", "dns-sd", "zeroconf"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .connectionless()
            .state(DevelopmentState::Beta)
            // Announces on 224.0.0.251:5353 - an unprivileged port, and joining
            // a multicast group needs no elevated privileges.
            .privilege_requirement(PrivilegeRequirement::None)
            .implementation("mdns-sd ServiceDaemon (not hickory-proto); binds no listener of its own")
            .llm_control("Service registration at startup only - no query handling, no runtime updates")
            .e2e_testing("mdns-sd, an independent mDNS implementation, in tests/server/mdns/test.rs and not #[ignore]d: its ServiceDaemon browses and resolves the services this server advertises, asserting on ServiceResolved.")
            .notes("Multicast service discovery; advertisement-only, incoming mDNS queries are handled by the library, not by the LLM")
            .build()
    }
    fn description(&self) -> &'static str {
        "Multicast DNS service discovery server"
    }
    fn example_prompt(&self) -> &'static str {
        "Advertise a web service via mDNS on port 8080"
    }
    fn group_name(&self) -> &'static str {
        "Application"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        StartupExamples::new(
            // LLM-driven example
            json!({
                "type": "open_server",
                "port": 5353,
                "base_stack": "mdns",
                "instruction": "Advertise HTTP service 'My Web Server' on port 8080 via mDNS"
            }),
            // Script-based example
            json!({
                "type": "open_server",
                "port": 5353,
                "base_stack": "mdns",
                "event_handlers": [{
                    "event_pattern": "mdns_server_startup",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "# Register mDNS service\nrespond([{'type': 'register_mdns_service', 'service_type': '_http._tcp.local.', 'instance_name': 'My Web Server', 'port': 8080, 'properties': {'path': '/', 'version': '1.0'}}])"
                    }
                }]
            }),
            // Static handler example
            json!({
                "type": "open_server",
                "port": 5353,
                "base_stack": "mdns",
                "event_handlers": [{
                    "event_pattern": "mdns_server_startup",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "register_mdns_service",
                            "service_type": "_http._tcp.local.",
                            "instance_name": "My Web Server",
                            "port": 8080,
                            "properties": {"path": "/", "version": "1.0"}
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for MdnsProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::mdns::MdnsServer;
            MdnsServer::spawn_with_llm_actions(
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
            // Registration is performed by MdnsServer::spawn_with_llm_actions,
            // which reads `register_mdns_service` out of the raw action list at
            // startup. Reaching this arm means the action was produced outside
            // that startup pass (e.g. as a user-triggered async action), where
            // there is no daemon handle to register against - so it is a no-op,
            // not a registration.
            "register_mdns_service" => {
                debug!(
                    "register_mdns_service outside server startup is a no-op \
                     (services can only be registered when the mDNS server starts)"
                );
                Ok(ActionResult::NoAction)
            }
            _ => Err(anyhow::anyhow!("Unknown mDNS action: {}", action_type)),
        }
    }
}

// Action definitions

fn register_mdns_service_action() -> ActionDefinition {
    ActionDefinition {
        name: "register_mdns_service".to_string(),
        description: "Register an mDNS/DNS-SD service for network discovery. Only takes effect in response to the mdns_server_startup event - services cannot be added, changed or removed once the server is running.".to_string(),
        parameters: vec![
            Parameter {
                name: "service_type".to_string(),
                type_hint: "string".to_string(),
                description: "Service type (e.g., '_http._tcp.local.', '_ftp._tcp.local.')"
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "instance_name".to_string(),
                type_hint: "string".to_string(),
                description: "Service instance name (e.g., 'My Web Server')".to_string(),
                required: true,
            },
            Parameter {
                name: "port".to_string(),
                type_hint: "number".to_string(),
                description: "Port number where service is available".to_string(),
                required: true,
            },
            Parameter {
                name: "properties".to_string(),
                type_hint: "object".to_string(),
                description: "TXT record properties (key-value pairs)".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "register_mdns_service",
            "service_type": "_http._tcp.local.",
            "instance_name": "My Web Server",
            "port": 8080,
            "properties": {
                "path": "/",
                "version": "1.0"
            }
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> mDNS register {service_type} '{instance_name}' on port {port}")
                .with_debug("mDNS register_mdns_service: type={service_type}, name={instance_name}, port={port}"),
        ),
    }
}

// ============================================================================
// mDNS Action Constants
// ============================================================================

pub static REGISTER_MDNS_SERVICE_ACTION: LazyLock<ActionDefinition> =
    LazyLock::new(|| register_mdns_service_action());

// ============================================================================
// mDNS Event Type Constants
// ============================================================================

/// mDNS server startup event - triggered when mDNS server starts
pub static MDNS_SERVER_STARTUP_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "mdns_server_startup",
        "mDNS server starting - register services for network discovery",
        json!({
            "type": "register_mdns_service",
            "service_type": "_http._tcp.local.",
            "instance_name": "My Web Server",
            "port": 8080,
            "properties": {"path": "/", "version": "1.0"}
        }),
    )
    // No parameters - just startup notification
    .with_actions(vec![REGISTER_MDNS_SERVICE_ACTION.clone()])
    .with_log_template(
        LogTemplate::new()
            .with_info("mDNS server startup")
            .with_debug("mDNS server initializing for service registration")
            .with_trace("mDNS startup: {json_pretty(.)}"),
    )
});

/// Get mDNS event types
pub fn get_mdns_event_types() -> Vec<EventType> {
    vec![MDNS_SERVER_STARTUP_EVENT.clone()]
}
