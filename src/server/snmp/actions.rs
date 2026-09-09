//! SNMP protocol actions implementation

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

/// SNMP protocol action handler
pub struct SnmpProtocol;

impl SnmpProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for SnmpProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        // SNMP has async action for sending traps
        vec![send_trap_action()]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_snmp_response_action(),
            send_snmp_error_action(),
            ignore_request_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "SNMP"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_snmp_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>UDP>SNMP"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["snmp", "snmp agent"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .connectionless()
            .state(DevelopmentState::Beta)
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(161))
            .implementation("rasn-snmp v0.18 for parsing + manual BER encoding")
            .llm_control("OID responses (sysDescr, ifTable, custom MIBs)")
            .e2e_testing("Net-SNMP's own snmpget/snmpgetnext (tests/server/snmp/test.rs), run with -On -Oe so the assertions pin the decoded type tag and value rather than the local MIB set. Not ignored, and it hard-fails rather than skipping when the binaries are absent. Not the `snmp` Rust crate, whatever the older notes said")
            .notes("SNMPv1/v2c only. request-id and community are echoed from the request automatically. send_trap is defined but never leaves the process. Incoming BER is depth- and length-screened before rasn decodes it: rasn 0.18 recurses without a bound on constructed OCTET STRINGs, and a 60 KB datagram was enough to stack-overflow the whole process")
            .build()
    }
    fn description(&self) -> &'static str {
        "SNMP agent for network monitoring"
    }
    fn example_prompt(&self) -> &'static str {
        "SNMP Port 8161 serve OID 1.3.6.1.2.1.1.1.0 (sysDescr) return 'NetGet SNMP Server v0.1'"
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
                "port": 161,
                "base_stack": "snmp",
                "instruction": "SNMP agent serving OID 1.3.6.1.2.1.1.1.0 (sysDescr) as 'NetGet SNMP Server v1.0'"
            }),
            // Script-based example
            json!({
                "type": "open_server",
                "port": 161,
                "base_stack": "snmp",
                "event_handlers": [{
                    "event_pattern": "snmp_request",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "# Handle SNMP GET request\noids = event.get('oids', [])\nvariables = [{'oid': oid, 'type': 'string', 'value': 'NetGet SNMP Server'} for oid in oids]\nrespond([{'type': 'send_snmp_response', 'variables': variables}])"
                    }
                }]
            }),
            // Static handler example
            json!({
                "type": "open_server",
                "port": 161,
                "base_stack": "snmp",
                "event_handlers": [{
                    "event_pattern": "snmp_request",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_snmp_response",
                            "variables": [{
                                "oid": "1.3.6.1.2.1.1.1.0",
                                "type": "string",
                                "value": "NetGet SNMP Server v1.0"
                            }]
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for SnmpProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::snmp::SnmpServer;
            SnmpServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.server_id,
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
            "send_trap" => self.execute_send_trap(action),
            "send_snmp_response" => self.execute_send_snmp_response(action),
            "send_snmp_error" => self.execute_send_snmp_error(action),
            "ignore_request" => Ok(ActionResult::NoAction),
            _ => Err(anyhow::anyhow!("Unknown SNMP action: {}", action_type)),
        }
    }
}

impl SnmpProtocol {
    /// Execute send_trap async action
    fn execute_send_trap(&self, action: serde_json::Value) -> Result<ActionResult> {
        let _target = action
            .get("target")
            .and_then(|v| v.as_str())
            .context("Missing 'target' parameter")?;

        let variables = action
            .get("variables")
            .and_then(|v| v.as_array())
            .context("Missing 'variables' parameter")?;

        // Encode trap data as JSON for now
        // The caller will need to convert this to actual SNMP trap format
        let trap_data = json!({
            "variables": variables
        });

        Ok(ActionResult::Output(
            serde_json::to_vec(&trap_data).context("Failed to serialize trap data")?,
        ))
    }

    /// Execute send_snmp_response sync action
    fn execute_send_snmp_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let variables = action
            .get("variables")
            .and_then(|v| v.as_array())
            .context("Missing 'variables' parameter")?;

        // Encode response data as JSON
        // The caller will convert this to actual SNMP response format
        let response_data = json!({
            "variables": variables,
            "error": false
        });

        Ok(ActionResult::Output(
            serde_json::to_vec(&response_data).context("Failed to serialize SNMP response")?,
        ))
    }

    /// Execute send_snmp_error sync action
    fn execute_send_snmp_error(&self, action: serde_json::Value) -> Result<ActionResult> {
        let error_message = action
            .get("error_message")
            .and_then(|v| v.as_str())
            .context("Missing 'error_message' parameter")?;

        // SNMP puts the reason on the wire as a number, not text: map the optional
        // 'error_status' (name or code) onto it, defaulting to genErr.
        let error_status = match action.get("error_status") {
            None => 5,
            Some(v) if v.is_null() => 5,
            Some(v) => {
                if let Some(n) = v.as_u64() {
                    if n > 5 {
                        return Err(anyhow::anyhow!(
                            "Invalid 'error_status' {n}: SNMPv1 defines 0-5 \
                             (0 noError, 1 tooBig, 2 noSuchName, 3 badValue, 4 readOnly, 5 genErr)"
                        ));
                    }
                    n as u8
                } else if let Some(s) = v.as_str() {
                    match s.trim().to_ascii_lowercase().as_str() {
                        "noerror" => 0,
                        "toobig" => 1,
                        "nosuchname" => 2,
                        "badvalue" => 3,
                        "readonly" => 4,
                        "generr" | "genericerror" => 5,
                        other => {
                            return Err(anyhow::anyhow!(
                                "Invalid 'error_status' {other:?}: expected one of noError, \
                                 tooBig, noSuchName, badValue, readOnly, genErr, or the \
                                 matching code 0-5"
                            ))
                        }
                    }
                } else {
                    return Err(anyhow::anyhow!(
                        "Invalid 'error_status': expected a name or a code 0-5, got {v}"
                    ));
                }
            }
        };

        let error_index = action
            .get("error_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Encode error response as JSON
        let response_data = json!({
            "error": true,
            "error_message": error_message,
            "error_status": error_status,
            "error_index": error_index
        });

        Ok(ActionResult::Output(
            serde_json::to_vec(&response_data).context("Failed to serialize SNMP error")?,
        ))
    }
}

/// Action definition for send_trap (async)
fn send_trap_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_trap".to_string(),
        description: "NOT IMPLEMENTED: this action validates its arguments and returns, but no trap datagram is ever sent to 'target'. It is listed here only so existing instructions do not fail outright - do not rely on it to notify anything.".to_string(),
        parameters: vec![
            Parameter {
                name: "target".to_string(),
                type_hint: "string".to_string(),
                description: "Target address in format 'IP:port'".to_string(),
                required: true,
            },
            Parameter {
                name: "variables".to_string(),
                type_hint: "array".to_string(),
                description: "Array of variable bindings with oid, type, and value".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "send_trap",
            "target": "127.0.0.1:162",
            "variables": [
                {"oid": "1.3.6.1.2.1.1.3.0", "type": "timeticks", "value": 12345}
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SNMP trap to {target}")
                .with_debug("SNMP send_trap: target={target}, {variables_len} vars"),
        ),
    }
}

/// Action definition for send_snmp_response (sync)
fn send_snmp_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_snmp_response".to_string(),
        description: "Answer the request with a set of OID values. The request-id, community string and SNMP version are copied from the request being answered, so you only supply the values. Normally return one entry per OID in the event's 'oids' list, in the same order.".to_string(),
        parameters: vec![Parameter {
            name: "variables".to_string(),
            type_hint: "array".to_string(),
            description: "Variable bindings, each an object {\"oid\": ..., \"type\": ..., \"value\": ...}. 'oid' is dotted decimal such as \"1.3.6.1.2.1.1.1.0\" (first component 0-2, every component numeric). 'type' is one of: \"string\" (text), \"integer\" (signed 32-bit), \"counter\" (Counter32, monotonically increasing), \"gauge\" (Gauge32, a value that goes up and down), \"timeticks\" (hundredths of a second since start-up, e.g. sysUpTime), \"null\" (no value for this OID). 'value' must match the type: a JSON string for \"string\", a JSON number for the numeric types, omitted for \"null\". An unrecognised type encodes as NULL".to_string(),
            required: true,
        }],
        example: json!({
            "type": "send_snmp_response",
            "variables": [
                {"oid": "1.3.6.1.2.1.1.1.0", "type": "string", "value": "System Description"},
                {"oid": "1.3.6.1.2.1.1.5.0", "type": "string", "value": "hostname"}
            ]
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SNMP response {variables_len} vars")
                .with_debug("SNMP send_snmp_response: {variables_len} variables"),
        ),
    }
}

/// Action definition for send_snmp_error (sync)
fn send_snmp_error_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_snmp_error".to_string(),
        description: "Answer the request with an SNMP error instead of values - use it when the requested OID does not exist here, or when a SET is refused. The reply carries the numeric error-status and no variable bindings; the request-id and community are echoed automatically.".to_string(),
        parameters: vec![
            Parameter {
                name: "error_message".to_string(),
                type_hint: "string".to_string(),
                description: "Why the request failed. SNMP has no field for text, so this only reaches NetGet's logs and access log - the client sees 'error_status'".to_string(),
                required: true,
            },
            Parameter {
                name: "error_status".to_string(),
                type_hint: "string or number".to_string(),
                description: "Which error the client is told about: \"noSuchName\" (2) for an OID this agent does not serve, \"badValue\" (3) for a SET with an unusable value, \"readOnly\" (4) for a SET of a read-only OID, \"tooBig\" (1) if the answer would not fit, \"genErr\" (5) for anything else. Accepts the name or the number. Default: genErr".to_string(),
                required: false,
            },
            Parameter {
                name: "error_index".to_string(),
                type_hint: "number".to_string(),
                description: "1-based position of the offending OID in the request's list, or 0 when no single OID is to blame. Default 0".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_snmp_error",
            "error_message": "OID 1.3.6.1.4.1.9999.1.0 is not served by this agent",
            "error_status": "noSuchName",
            "error_index": 1
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> SNMP error: {error_message}")
                .with_debug("SNMP send_snmp_error: {error_message}"),
        ),
    }
}

/// Action definition for ignore_request (sync)
fn ignore_request_action() -> ActionDefinition {
    ActionDefinition {
        name: "ignore_request".to_string(),
        description: "Ignore this SNMP request and don't send a response".to_string(),
        parameters: vec![],
        example: json!({
            "type": "ignore_request"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("SNMP request ignored")
                .with_debug("SNMP ignore_request"),
        ),
    }
}

// ============================================================================
// SNMP Event Type Constants
// ============================================================================

pub static SNMP_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "snmp_request",
        "SNMP client sent a GET/GETNEXT/GETBULK request",
        json!({
            "type": "send_snmp_response",
            "variables": [
                {"oid": "1.3.6.1.2.1.1.1.0", "type": "string", "value": "System Description"}
            ]
        })
    )
    .with_parameters(vec![
        Parameter {
            name: "request_type".to_string(),
            type_hint: "string".to_string(),
            description: "PDU type of the request, spelled exactly: 'GetRequest', 'GetNextRequest', 'GetBulkRequest' or 'SetRequest'. GetNextRequest asks for the OID that follows the one given (table walking); GetBulkRequest asks for several at once".to_string(),
            required: true,
        },
        Parameter {
            name: "oids".to_string(),
            type_hint: "array".to_string(),
            description: "OIDs the client asked about, in dotted decimal, in request order. Answer with one variable binding per OID in the same order".to_string(),
            required: true,
        },
        Parameter {
            name: "community".to_string(),
            type_hint: "string".to_string(),
            description: "Community string the client sent - SNMPv1/v2c's only credential, typically 'public'. It is echoed back automatically; check it here if the instruction says to reject unknown communities".to_string(),
            required: false,
        },
        Parameter {
            name: "request_id".to_string(),
            type_hint: "number".to_string(),
            description: "Request id chosen by the client. The response echoes it automatically - it is exposed only for logging".to_string(),
            required: false,
        },
        Parameter {
            name: "version".to_string(),
            type_hint: "string".to_string(),
            description: "'v1' or 'v2c'. The reply uses the same version".to_string(),
            required: false,
        },
        Parameter {
            name: "client_ip".to_string(),
            type_hint: "string".to_string(),
            description: "IP address the request came from".to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        send_snmp_response_action(),
        send_snmp_error_action(),
        ignore_request_action(),
    ])
    .with_log_template(
        LogTemplate::new()
            .with_info("SNMP {request_type} from {client_ip}")
            .with_debug("SNMP {request_type} from {client_ip} ({version}, community={community}): {oids}")
            .with_trace("SNMP: {json_pretty(.)}"),
    )
});

pub fn get_snmp_event_types() -> Vec<EventType> {
    vec![SNMP_REQUEST_EVENT.clone()]
}
