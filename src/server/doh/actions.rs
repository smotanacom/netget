//! DNS-over-HTTPS protocol actions implementation
//!
//! Reuses DNS actions since DoH is just DNS delivered over HTTPS.

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::server::dns::actions::DnsProtocol;
use crate::state::app_state::AppState;
use anyhow::Result;
use serde_json::json;
use std::sync::LazyLock;

/// Event type constant for DoH queries
/// Reuses DNS action definitions since DoH delegates to DnsProtocol
pub static DOH_QUERY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    // Get DNS actions from DnsProtocol
    let dns_protocol = DnsProtocol::new();
    let dns_actions = dns_protocol.get_sync_actions();

    EventType::new(
        "doh_query",
        "Client sent DNS query over HTTPS",
        json!({"type": "placeholder", "event_id": "doh_query"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "query_id".to_string(),
            type_hint: "number".to_string(),
            description: "DNS query ID from the request packet".to_string(),
            required: true,
        },
        Parameter {
            name: "domain".to_string(),
            type_hint: "string".to_string(),
            description: "Domain name being queried".to_string(),
            required: true,
        },
        Parameter {
            name: "query_type".to_string(),
            type_hint: "string".to_string(),
            description: "DNS query type (A, AAAA, MX, TXT, CNAME, etc.)".to_string(),
            required: true,
        },
        Parameter {
            name: "peer_addr".to_string(),
            type_hint: "string".to_string(),
            description: "Client IP address and port".to_string(),
            required: true,
        },
        Parameter {
            name: "method".to_string(),
            type_hint: "string".to_string(),
            description: "HTTP method used (GET or POST)".to_string(),
            required: true,
        },
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("DoH {query_type} {domain} via {method}")
            .with_debug("DoH query from {peer_addr}: {query_type} {domain} via {method}")
            .with_trace("DoH: {json_pretty(.)}"),
    )
    .with_actions(dns_actions)
});

/// DoH protocol action handler
/// Delegates to DNS protocol for action execution since DoH is DNS over HTTPS
pub struct DohProtocol {
    dns_protocol: DnsProtocol,
}

impl DohProtocol {
    pub fn new() -> Self {
        Self {
            dns_protocol: DnsProtocol::new(),
        }
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for DohProtocol {
    fn get_async_actions(&self, state: &AppState) -> Vec<ActionDefinition> {
        self.dns_protocol.get_async_actions(state)
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        self.dns_protocol.get_sync_actions()
    }
    fn protocol_name(&self) -> &'static str {
        "DoH"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![DOH_QUERY_EVENT.clone()]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>TLS>HTTP2>DNS"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["doh", "dns-over-https", "dns over https"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Beta)
            // DoH is normally served on TCP/443, which is a privileged port.
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(443))
            .implementation("hickory-proto + hyper + tokio-rustls; DNS actions and action execution are delegated to the DNS protocol")
            .llm_control("Same as DNS (delegates to DNS protocol)")
            .e2e_testing("tests/server/doh/e2e_test.rs - reqwest (hyper+rustls) HTTP/2 client, mock-driven. It connects with http2_prior_knowledge(), so it asserts nothing about ALPN; server_advertises_h2_alpn covers that separately")
            .notes("GET/POST, HTTP/2 only (no HTTP/1.1), self-signed certs, any request path accepted. Advertises ALPN h2 - without it a negotiating client falls back to HTTP/1.1, which this server does not speak")
            .build()
    }
    fn description(&self) -> &'static str {
        "DNS-over-HTTPS server for secure domain resolution"
    }
    fn example_prompt(&self) -> &'static str {
        "DNS-over-HTTPS server on port 443 resolving all queries to 93.184.216.34"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }
    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        StartupExamples::new(
            // LLM-driven example
            json!({
                "type": "open_server",
                "port": 443,
                "base_stack": "doh",
                "instruction": "DNS-over-HTTPS server resolving all A queries for example.com to 93.184.216.34, NXDOMAIN for others"
            }),
            // Script-based example
            json!({
                "type": "open_server",
                "port": 443,
                "base_stack": "doh",
                "event_handlers": [{
                    "event_pattern": "doh_query",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "# Echo the client's transaction ID and queried name\nif event.get('domain', '').rstrip('.') == 'example.com':\n    respond([{'type': 'send_dns_a_response', 'query_id': event['query_id'], 'domain': event['domain'], 'ip': '93.184.216.34', 'ttl': 300}])\nelse:\n    respond([{'type': 'send_dns_nxdomain', 'query_id': event['query_id'], 'domain': event['domain'], 'query_type': event.get('query_type', 'A')}])"
                    }
                }]
            }),
            // Static handler example: static actions are fixed JSON with no
            // access to the event, so they cannot echo the client's random
            // transaction ID or the queried name. Dropping queries is the only
            // thing static mode does correctly for DNS-over-HTTPS; use script
            // mode for deterministic answers.
            json!({
                "type": "open_server",
                "port": 443,
                "base_stack": "doh",
                "event_handlers": [{
                    "event_pattern": "doh_query",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "ignore_query"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for DohProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::doh::DohServer;
            // DohServer::spawn binds the listener before returning, so bind
            // failures surface here and the address it returns is the real
            // bound address (resolving port 0 to the OS-assigned port).
            DohServer::spawn(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.server_id,
                ctx.status_tx,
            )
            .await
        })
    }
    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        self.dns_protocol.execute_action(action)
    }
}
