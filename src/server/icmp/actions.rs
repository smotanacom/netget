//! ICMP protocol actions implementation

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

/// Name of the loopback interface on this platform.
///
/// Linux and Windows call it `lo`; macOS and the BSDs call it `lo0`. Hardcoding `lo` made the
/// default binding unresolvable on macOS ("Device 'lo' not found").
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
const DEFAULT_LOOPBACK_INTERFACE: &str = "lo0";
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
const DEFAULT_LOOPBACK_INTERFACE: &str = "lo";

/// ICMP protocol action handler
pub struct IcmpProtocol;

impl IcmpProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for IcmpProtocol {
    fn default_binding(&self) -> Option<crate::protocol::BindingDefaults> {
        // ICMP uses interface-based binding (loopback by default)
        Some(crate::protocol::BindingDefaults::interface_based(
            DEFAULT_LOOPBACK_INTERFACE,
        ))
    }

    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![]
    }

    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_echo_reply_action(),
            send_destination_unreachable_action(),
            send_time_exceeded_action(),
            // send_timestamp_reply_action(), // TODO: Removed - timestamp support requires pnet timestamp packet types
            ignore_icmp_action(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "ICMP"
    }

    fn get_event_types(&self) -> Vec<EventType> {
        get_icmp_event_types()
    }

    fn stack_name(&self) -> &'static str {
        "IP>ICMP"
    }

    fn keywords(&self) -> Vec<&'static str> {
        vec!["icmp", "ping", "echo", "traceroute"]
    }

    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::RawSockets)
            .connectionless()
            .implementation("Raw IP sockets + pnet for ICMP packet handling")
            .llm_control("Full control - can respond to all ICMP message types")
            .e2e_testing(
                "tests/server/icmp/packet_codec_test.rs is the only ICMP server test that runs \
                 unprivileged: it asserts every emitted packet field-by-field against RFC 791 \
                 and RFC 792, that hostile action JSON is refused rather than panicking, and \
                 that each advertised example is accepted by its own executor. \
                 tests/capture_startup_reports_failure_test.rs asserts spawn() returns Err \
                 without raw-socket privilege. tests/server/icmp/e2e_test.rs crafts real echo \
                 requests and reads the replies off a raw socket, but is #[ignore]d for \
                 privilege and has never been run in CI.",
            )
            .notes(
                "Requires root/CAP_NET_RAW for raw socket access. Startup failure is reported: \
                 spawn_with_llm awaits the SOCK_RAW sockets, so a privilege failure lands in \
                 ServerStatus::Error rather than Running. UNVERIFIED, and this is the whole \
                 gap: no packet this server builds has ever been observed leaving a real \
                 socket. Until this pass none could have been correct - the send socket did not \
                 set IP_HDRINCL, so the kernel prepended its own header and every reply went out \
                 with our IPv4 header sitting where the ICMP message belongs. That is fixed and \
                 unit-tested up to the syscall, no further. The 'interface' argument is accepted \
                 and ignored - the socket receives ICMP from every interface. Note the kernel \
                 answers echo requests itself, so a userspace reply is a second reply on the \
                 wire.",
            )
            .build()
    }

    fn description(&self) -> &'static str {
        "ICMP (Internet Control Message Protocol) server"
    }

    fn example_prompt(&self) -> &'static str {
        "Listen for ICMP echo requests on eth0"
    }

    fn group_name(&self) -> &'static str {
        "Core"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        // Deterministic: reply to every echo request, swapping source/dest and
        // echoing the id/sequence/payload, no LLM call.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "icmp_echo_request":
    actions = [{"type": "send_echo_reply",
                "source_ip": event.get("destination_ip", ""),
                "destination_ip": event.get("source_ip", ""),
                "identifier": event.get("identifier", 0),
                "sequence": event.get("sequence", 0),
                "payload_hex": event.get("payload_hex", "")}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            json!({
                "type": "open_server",
                "interface": "eth0",
                "base_stack": "icmp",
                "instruction": "ICMP server that responds to ping requests"
            }),
            json!({
                "type": "open_server",
                "interface": "eth0",
                "base_stack": "icmp",
                "event_handlers": [{
                    "event_pattern": "icmp_echo_request",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }]
            }),
            json!({
                "type": "open_server",
                "interface": "eth0",
                "base_stack": "icmp",
                "event_handlers": [{
                    "event_pattern": "icmp_echo_request",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "ignore_icmp"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for IcmpProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::icmp::IcmpServer;

            // ICMP uses interface-based binding
            // Extract interface from context (defaults already applied)
            let interface = ctx
                .interface()
                .context("ICMP requires network interface")?
                .to_string();

            // Get listen address before moving ctx fields
            let listen_addr = ctx.legacy_listen_addr();

            // Spawn the ICMP server
            let _interface_name = IcmpServer::spawn_with_llm(
                interface,
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.server_id,
            )
            .await?;

            // ICMP doesn't bind to a socket, so return a dummy address
            // The listen_addr from context is just a placeholder
            Ok(listen_addr)
        })
    }

    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "send_echo_reply" => self.execute_send_echo_reply(action),
            "send_destination_unreachable" => self.execute_send_destination_unreachable(action),
            "send_time_exceeded" => self.execute_send_time_exceeded(action),
            // "send_timestamp_reply" => self.execute_send_timestamp_reply(action), // TODO: Removed - timestamp support requires pnet timestamp packet types
            "ignore_icmp" => Ok(ActionResult::NoAction),
            _ => Err(anyhow::anyhow!("Unknown ICMP action: {}", action_type)),
        }
    }
}

/// Largest payload that still fits an ICMP message inside one unfragmented IPv4 datagram:
/// 65535 total, less the 20-byte IPv4 header and the 8-byte ICMP header.
const MAX_ICMP_PAYLOAD: usize = 65535 - 20 - 8;

/// Read a required 16-bit action parameter.
///
/// `as u16` on a `u64` truncates in silence: an identifier of 70000 becomes 4464 and the reply
/// no longer matches any request the peer sent. Refusing is the honest answer, and the model is
/// told the range.
fn u16_field(action: &serde_json::Value, name: &str) -> Result<u16> {
    let raw = action
        .get(name)
        .and_then(|v| v.as_u64())
        .with_context(|| format!("Missing '{name}' parameter"))?;
    u16::try_from(raw)
        .with_context(|| format!("'{name}' must be a 16-bit ICMP field (0-65535), got {raw}"))
}

/// Read an 8-bit action parameter, with `default` used when it is absent.
///
/// Same reasoning as [`u16_field`]: an ICMP code of 300 silently became 44, which is not a code
/// at all.
fn u8_field(action: &serde_json::Value, name: &str, default: Option<u8>) -> Result<u8> {
    let raw = match action.get(name).and_then(|v| v.as_u64()) {
        Some(raw) => raw,
        None => return default.with_context(|| format!("Missing '{name}' parameter")),
    };
    u8::try_from(raw).with_context(|| format!("'{name}' must be an ICMP code (0-255), got {raw}"))
}

impl IcmpProtocol {
    /// Execute send_echo_reply action
    fn execute_send_echo_reply(&self, action: serde_json::Value) -> Result<ActionResult> {
        let source_ip = action
            .get("source_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'source_ip' parameter")?;

        let destination_ip = action
            .get("destination_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'destination_ip' parameter")?;

        let identifier = u16_field(&action, "identifier")?;
        let sequence = u16_field(&action, "sequence")?;

        let payload_hex = action
            .get("payload_hex")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let payload = if payload_hex.is_empty() {
            Vec::new()
        } else {
            hex::decode(payload_hex).context("Invalid hex in payload_hex")?
        };

        // `set_total_length(ip_size as u16)` would wrap silently past this, producing a header
        // whose length field disagrees with the buffer it describes.
        anyhow::ensure!(
            payload.len() <= MAX_ICMP_PAYLOAD,
            "payload_hex decodes to {} bytes; an ICMP echo reply carries at most {}",
            payload.len(),
            MAX_ICMP_PAYLOAD
        );

        // Parse IP addresses
        let source_ip_parsed: std::net::Ipv4Addr =
            source_ip.parse().context("Invalid source_ip format")?;
        let destination_ip_parsed: std::net::Ipv4Addr = destination_ip
            .parse()
            .context("Invalid destination_ip format")?;

        // Build ICMP echo reply packet
        use crate::server::icmp::IcmpServer;
        let packet = IcmpServer::build_echo_reply(
            source_ip_parsed,
            destination_ip_parsed,
            identifier,
            sequence,
            &payload,
        );

        Ok(ActionResult::Output(packet))
    }

    /// Execute send_destination_unreachable action
    fn execute_send_destination_unreachable(
        &self,
        action: serde_json::Value,
    ) -> Result<ActionResult> {
        let source_ip = action
            .get("source_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'source_ip' parameter")?;

        let destination_ip = action
            .get("destination_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'destination_ip' parameter")?;

        let code = u8_field(&action, "code", None)?;

        let original_packet_hex = action
            .get("original_packet_hex")
            .and_then(|v| v.as_str())
            .context("Missing 'original_packet_hex' parameter")?;

        let original_packet =
            hex::decode(original_packet_hex).context("Invalid hex in original_packet_hex")?;

        // Parse IP addresses
        let source_ip_parsed: std::net::Ipv4Addr =
            source_ip.parse().context("Invalid source_ip format")?;
        let destination_ip_parsed: std::net::Ipv4Addr = destination_ip
            .parse()
            .context("Invalid destination_ip format")?;

        // Build ICMP destination unreachable packet
        use crate::server::icmp::IcmpServer;
        let packet = IcmpServer::build_destination_unreachable(
            source_ip_parsed,
            destination_ip_parsed,
            code,
            &original_packet,
        );

        Ok(ActionResult::Output(packet))
    }

    /// Execute send_time_exceeded action
    fn execute_send_time_exceeded(&self, action: serde_json::Value) -> Result<ActionResult> {
        let source_ip = action
            .get("source_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'source_ip' parameter")?;

        let destination_ip = action
            .get("destination_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'destination_ip' parameter")?;

        // Default 0 = TTL exceeded in transit.
        let code = u8_field(&action, "code", Some(0))?;

        let original_packet_hex = action
            .get("original_packet_hex")
            .and_then(|v| v.as_str())
            .context("Missing 'original_packet_hex' parameter")?;

        let original_packet =
            hex::decode(original_packet_hex).context("Invalid hex in original_packet_hex")?;

        // Parse IP addresses
        let source_ip_parsed: std::net::Ipv4Addr =
            source_ip.parse().context("Invalid source_ip format")?;
        let destination_ip_parsed: std::net::Ipv4Addr = destination_ip
            .parse()
            .context("Invalid destination_ip format")?;

        // Build ICMP time exceeded packet
        use crate::server::icmp::IcmpServer;
        let packet = IcmpServer::build_time_exceeded(
            source_ip_parsed,
            destination_ip_parsed,
            code,
            &original_packet,
        );

        Ok(ActionResult::Output(packet))
    }

    /* TODO: Timestamp support requires pnet to add timestamp packet types
    /// Execute send_timestamp_reply action
    fn execute_send_timestamp_reply(&self, action: serde_json::Value) -> Result<ActionResult> {
        let source_ip = action
            .get("source_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'source_ip' parameter")?;

        let destination_ip = action
            .get("destination_ip")
            .and_then(|v| v.as_str())
            .context("Missing 'destination_ip' parameter")?;

        let identifier = action
            .get("identifier")
            .and_then(|v| v.as_u64())
            .context("Missing 'identifier' parameter")? as u16;

        let sequence = action
            .get("sequence")
            .and_then(|v| v.as_u64())
            .context("Missing 'sequence' parameter")? as u16;

        let originate_timestamp = action
            .get("originate_timestamp")
            .and_then(|v| v.as_u64())
            .context("Missing 'originate_timestamp' parameter")? as u32;

        // Parse IP addresses
        let source_ip_parsed: std::net::Ipv4Addr =
            source_ip.parse().context("Invalid source_ip format")?;
        let destination_ip_parsed: std::net::Ipv4Addr =
            destination_ip.parse().context("Invalid destination_ip format")?;

        // Build ICMP timestamp reply packet
        use crate::server::icmp::IcmpServer;
        let packet = IcmpServer::build_timestamp_reply(
            source_ip_parsed,
            destination_ip_parsed,
            identifier,
            sequence,
            originate_timestamp,
        );

        Ok(ActionResult::Output(packet))
    }
    */
}

/// Action definition for send_echo_reply
fn send_echo_reply_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_echo_reply".to_string(),
        description: "Send an ICMP Echo Reply packet in response to an Echo Request (ping)"
            .to_string(),
        parameters: vec![
            Parameter {
                name: "source_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Source IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "destination_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "identifier".to_string(),
                type_hint: "number".to_string(),
                description: "ICMP identifier (must match request)".to_string(),
                required: true,
            },
            Parameter {
                name: "sequence".to_string(),
                type_hint: "number".to_string(),
                description: "ICMP sequence number (must match request)".to_string(),
                required: true,
            },
            Parameter {
                name: "payload_hex".to_string(),
                type_hint: "string".to_string(),
                description: "Payload data as hex string (must match request)".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_echo_reply",
            "source_ip": "192.168.1.100",
            "destination_ip": "192.168.1.50",
            "identifier": 1234,
            "sequence": 1,
            "payload_hex": "48656c6c6f"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> ICMP echo reply to {destination_ip}")
                .with_debug(
                    "ICMP send_echo_reply: dst={destination_ip} id={identifier} seq={sequence}",
                ),
        ),
    }
}

/// Action definition for send_destination_unreachable
fn send_destination_unreachable_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_destination_unreachable".to_string(),
        description: "Send an ICMP Destination Unreachable message".to_string(),
        parameters: vec![
            Parameter {
                name: "source_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Source IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "destination_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "code".to_string(),
                type_hint: "number".to_string(),
                description: "Unreachable code: 0=net, 1=host, 2=protocol, 3=port, 4=fragmentation needed, 5=source route failed".to_string(),
                required: true,
            },
            Parameter {
                name: "original_packet_hex".to_string(),
                type_hint: "string".to_string(),
                description: "Original IP header + first 8 bytes of original datagram (hex)"
                    .to_string(),
                required: true,
            },
        ],
        // RFC 792 wants the original IP header plus the next 64 bits. The value
        // here is exactly that and nothing is elided: a 20-byte IPv4 header
        // (192.168.1.50 -> 203.0.113.5, proto 17, total length 28, header
        // checksum 0x60ab) followed by the complete 8-byte UDP header
        // (41234 -> 53, length 8, checksum 0x60b6). Both checksums are real, so
        // the quoted datagram is one a client can actually match against.
        example: json!({
            "type": "send_destination_unreachable",
            "source_ip": "192.168.1.1",
            "destination_ip": "192.168.1.50",
            "code": 1,
            "original_packet_hex": "4500001c1c460000401160abc0a80132cb007105a1120035000860b6"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> ICMP unreachable code={code} to {destination_ip}")
                .with_debug("ICMP send_destination_unreachable: dst={destination_ip} code={code}"),
        ),
    }
}

/// Action definition for send_time_exceeded
fn send_time_exceeded_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_time_exceeded".to_string(),
        description: "Send an ICMP Time Exceeded message (used in traceroute)".to_string(),
        parameters: vec![
            Parameter {
                name: "source_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Source IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "destination_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "code".to_string(),
                type_hint: "number".to_string(),
                description: "Time exceeded code: 0=TTL exceeded in transit, 1=fragment reassembly time exceeded".to_string(),
                required: false,
            },
            Parameter {
                name: "original_packet_hex".to_string(),
                type_hint: "string".to_string(),
                description: "Original IP header + first 8 bytes of original datagram (hex)"
                    .to_string(),
                required: true,
            },
        ],
        // The original IP header plus the next 64 bits (RFC 792), complete and
        // with real checksums: a classic UDP traceroute probe from 192.168.1.50
        // to 203.0.113.5 with TTL 1 (header checksum 0x9faa), followed by the
        // whole 8-byte UDP header (41234 -> 33434, length 8, checksum 0xde50).
        example: json!({
            "type": "send_time_exceeded",
            "source_ip": "10.0.0.1",
            "destination_ip": "192.168.1.50",
            "code": 0,
            "original_packet_hex": "4500001c1c47000001119faac0a80132cb007105a112829a0008de50"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> ICMP TTL exceeded to {destination_ip}")
                .with_debug("ICMP send_time_exceeded: dst={destination_ip} code={code}"),
        ),
    }
}

/* TODO: Timestamp support requires pnet to add timestamp packet types
/// Action definition for send_timestamp_reply
fn send_timestamp_reply_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_timestamp_reply".to_string(),
        description: "Send an ICMP Timestamp Reply message".to_string(),
        parameters: vec![
            Parameter {
                name: "source_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Source IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "destination_ip".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP address (format: X.X.X.X)".to_string(),
                required: true,
            },
            Parameter {
                name: "identifier".to_string(),
                type_hint: "number".to_string(),
                description: "ICMP identifier (must match request)".to_string(),
                required: true,
            },
            Parameter {
                name: "sequence".to_string(),
                type_hint: "number".to_string(),
                description: "ICMP sequence number (must match request)".to_string(),
                required: true,
            },
            Parameter {
                name: "originate_timestamp".to_string(),
                type_hint: "number".to_string(),
                description:
                    "Originate timestamp from request (milliseconds since midnight UT)".to_string(),
                required: true,
            },
        ],
        example: json!({
            "type": "send_timestamp_reply",
            "source_ip": "192.168.1.1",
            "destination_ip": "192.168.1.50",
            "identifier": 1234,
            "sequence": 1,
            "originate_timestamp": 12345678
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> ICMP timestamp reply to {destination_ip}")
                .with_debug("ICMP send_timestamp_reply: dst={destination_ip} id={identifier} seq={sequence}"),
        ),
    }
}
*/

/// Action definition for ignore_icmp
fn ignore_icmp_action() -> ActionDefinition {
    ActionDefinition {
        name: "ignore_icmp".to_string(),
        description: "Ignore this ICMP packet (no action taken)".to_string(),
        parameters: vec![],
        example: json!({
            "type": "ignore_icmp"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> ICMP ignore")
                .with_debug("ICMP ignore_icmp"),
        ),
    }
}

// ============================================================================
// ICMP Event Type Constants
// ============================================================================

pub static ICMP_ECHO_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "icmp_echo_request",
        "ICMP Echo Request (ping) received from network",
        json!({
            "type": "send_echo_reply",
            "source_ip": "192.168.1.100",
            "destination_ip": "192.168.1.50",
            "identifier": 1234,
            "sequence": 1,
            "payload_hex": "48656c6c6f"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "source_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Source IP address of the ping request".to_string(),
            required: true,
        },
        Parameter {
            name: "destination_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Destination IP address (our server)".to_string(),
            required: true,
        },
        Parameter {
            name: "identifier".to_string(),
            type_hint: "number".to_string(),
            description: "ICMP identifier".to_string(),
            required: true,
        },
        Parameter {
            name: "sequence".to_string(),
            type_hint: "number".to_string(),
            description: "ICMP sequence number".to_string(),
            required: true,
        },
        Parameter {
            name: "payload_hex".to_string(),
            type_hint: "string".to_string(),
            description: "Hexadecimal representation of the payload data".to_string(),
            required: false,
        },
        Parameter {
            name: "ttl".to_string(),
            type_hint: "number".to_string(),
            description: "Time to live from IP header".to_string(),
            required: false,
        },
    ])
    .with_actions(vec![send_echo_reply_action(), ignore_icmp_action()])
});

/* TODO: Timestamp support requires pnet to add timestamp packet types
pub static ICMP_TIMESTAMP_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "icmp_timestamp_request",
        "ICMP Timestamp Request received from network",
        json!({
            "type": "send_timestamp_reply",
            "source_ip": "192.168.1.100",
            "destination_ip": "192.168.1.50",
            "identifier": 1234,
            "sequence": 1,
            "originate_timestamp": 12345678
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "source_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Source IP address".to_string(),
            required: true,
        },
        Parameter {
            name: "destination_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Destination IP address".to_string(),
            required: true,
        },
        Parameter {
            name: "identifier".to_string(),
            type_hint: "number".to_string(),
            description: "ICMP identifier".to_string(),
            required: true,
        },
        Parameter {
            name: "sequence".to_string(),
            type_hint: "number".to_string(),
            description: "ICMP sequence number".to_string(),
            required: true,
        },
        Parameter {
            name: "originate_timestamp".to_string(),
            type_hint: "number".to_string(),
            description: "Originate timestamp (milliseconds since midnight UT)".to_string(),
            required: true,
        },
    ])
    .with_actions(vec![send_timestamp_reply_action(), ignore_icmp_action()])
    .with_log_template(
        LogTemplate::new()
            .with_info("ICMP timestamp request from {source_ip}")
            .with_debug("ICMP timestamp_request: src={source_ip} id={identifier} seq={sequence}"),
    )
});
*/

pub static ICMP_OTHER_MESSAGE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "icmp_other_message",
        "Other ICMP message type received (not echo or timestamp)",
        json!({
            "type": "send_destination_unreachable",
            "source_ip": "192.168.1.1",
            "destination_ip": "192.168.1.50",
            "code": 1,
            "original_packet_hex": "4500001c1c460000401160abc0a80132cb007105a1120035000860b6"
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "source_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Source IP address".to_string(),
            required: true,
        },
        Parameter {
            name: "destination_ip".to_string(),
            type_hint: "string".to_string(),
            description: "Destination IP address".to_string(),
            required: true,
        },
        Parameter {
            name: "icmp_type".to_string(),
            type_hint: "number".to_string(),
            description: "ICMP message type".to_string(),
            required: true,
        },
        Parameter {
            name: "icmp_code".to_string(),
            type_hint: "number".to_string(),
            description: "ICMP message code".to_string(),
            required: true,
        },
        Parameter {
            name: "packet_hex".to_string(),
            type_hint: "string".to_string(),
            description: "Full ICMP packet as hex".to_string(),
            required: false,
        },
    ])
    .with_actions(vec![
        send_destination_unreachable_action(),
        send_time_exceeded_action(),
        ignore_icmp_action(),
    ])
});

pub fn get_icmp_event_types() -> Vec<EventType> {
    vec![
        ICMP_ECHO_REQUEST_EVENT.clone(),
        // ICMP_TIMESTAMP_REQUEST_EVENT.clone(), // TODO: Removed - timestamp support requires pnet timestamp packet types
        ICMP_OTHER_MESSAGE_EVENT.clone(),
    ]
}
