//! DataLink client protocol actions implementation

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

/// The shortest thing that can be an Ethernet frame: 6 + 6 + 2.
///
/// Anything shorter has no EtherType and libpcap would either refuse it or put a runt on the
/// wire, so it is rejected here where the model can be told why.
pub const MIN_ETHERNET_FRAME_BYTES: usize = 14;

/// Upper bound on an injected frame, matching the capture snaplen. Well above any jumbo MTU;
/// the point is to refuse an accidental megabyte of hex before it reaches libpcap.
pub const MAX_ETHERNET_FRAME_BYTES: usize = 65535;

/// The frame fields every frame-carrying event exposes. Shared so the two events cannot drift
/// from each other or from what `mod.rs` actually puts in them.
fn frame_parameters(what: &str) -> Vec<Parameter> {
    vec![
        Parameter {
            name: "frame_hex".to_string(),
            type_hint: "string".to_string(),
            description: format!(
                "Hex of the first {} bytes of {} (dst MAC, src MAC, EtherType, payload). \
                 Longer frames are cut there - see `truncated`.",
                crate::client::datalink::MAX_HEX_BYTES_TO_MODEL,
                what
            ),
            required: true,
        },
        Parameter {
            name: "frame_length".to_string(),
            type_hint: "number".to_string(),
            description: "Full length of the frame in bytes, before any truncation".to_string(),
            required: true,
        },
        Parameter {
            name: "captured_length".to_string(),
            type_hint: "number".to_string(),
            description: "Number of bytes actually encoded in frame_hex".to_string(),
            required: true,
        },
        Parameter {
            name: "truncated".to_string(),
            type_hint: "boolean".to_string(),
            description: "True when frame_hex is a prefix of the frame rather than all of it"
                .to_string(),
            required: true,
        },
    ]
}

/// DataLink client frame captured event (for promiscuous mode listening)
pub static DATALINK_CLIENT_FRAME_CAPTURED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "datalink_frame_captured",
        "Raw Ethernet frame captured on interface",
        json!({"type": "inject_frame", "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"}),
    )
    .with_parameters(frame_parameters("the captured frame"))
    .with_actions(vec![inject_frame_action(true), wait_for_more_action()])
});

/// Raised once the capture handle is open, so the model can act on its instruction.
///
/// Without this the client opened the interface and asked the model nothing, so a client
/// created with "inject an ARP request for 10.0.0.2" sat there having done nothing.
pub static DATALINK_CLIENT_CONNECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "datalink_connected",
        "Capture handle open on the interface; ready to inject and capture",
        json!({"type": "inject_frame", "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"}),
    )
    .with_parameters(vec![
        Parameter {
            name: "interface".to_string(),
            type_hint: "string".to_string(),
            description: "The interface the capture was opened on".to_string(),
            required: true,
        },
        Parameter {
            name: "promiscuous".to_string(),
            type_hint: "bool".to_string(),
            description: "Whether the interface was opened in promiscuous mode".to_string(),
            required: true,
        },
    ])
    .with_actions(vec![inject_frame_action(false), wait_for_more_action()])
});

/// Raised after a frame really went out on the wire.
pub static DATALINK_CLIENT_FRAME_INJECTED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "datalink_frame_injected",
        "A raw frame was written to the interface",
        json!({"type": "wait_for_more"}),
    )
    .with_parameters(frame_parameters("the frame that was written"))
    .with_actions(vec![inject_frame_action(false), wait_for_more_action()])
});

/// `inject_frame`, in the two wordings the model sees it in.
///
/// One definition rather than the two hand-written near-copies that were here: the sync copy
/// documented `frame_hex` differently from the async one, and the async one told the model to
/// include the **FCS**. That is wrong for `pcap::sendpacket` — the driver appends the frame
/// check sequence, so four bytes of the model's arithmetic would go on the wire as payload.
fn inject_frame_action(in_response: bool) -> ActionDefinition {
    ActionDefinition {
        name: "inject_frame".to_string(),
        description: if in_response {
            "Inject a raw Ethernet frame in response to a captured frame".to_string()
        } else {
            "Inject a raw Ethernet frame onto the network".to_string()
        },
        parameters: vec![Parameter {
            name: "frame_hex".to_string(),
            type_hint: "string".to_string(),
            description: format!(
                "Hex-encoded Ethernet frame: destination MAC (6 bytes), source MAC (6 bytes), \
                 EtherType (2 bytes), then payload. Do NOT append the FCS - the interface \
                 computes it. At least {MIN_ETHERNET_FRAME_BYTES} and at most \
                 {MAX_ETHERNET_FRAME_BYTES} bytes, i.e. {} to {} hex characters.",
                MIN_ETHERNET_FRAME_BYTES * 2,
                MAX_ETHERNET_FRAME_BYTES * 2
            ),
            required: true,
        }],
        example: json!({
            "type": "inject_frame",
            "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"
        }),
        log_template: None,
    }
}

fn wait_for_more_action() -> ActionDefinition {
    ActionDefinition {
        name: "wait_for_more".to_string(),
        description: "Wait for more frames before responding".to_string(),
        parameters: vec![],
        example: json!({
            "type": "wait_for_more"
        }),
        log_template: None,
    }
}

/// DataLink client protocol action handler
pub struct DataLinkClientProtocol;

impl DataLinkClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for DataLinkClientProtocol {
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "interface".to_string(),
                type_hint: "string".to_string(),
                description:
                    "Network interface name for frame injection (e.g., 'eth0', 'en0', 'wlan0')"
                        .to_string(),
                required: true,
                example: json!("eth0"),
            },
            ParameterDefinition {
                name: "promiscuous".to_string(),
                type_hint: "boolean".to_string(),
                description:
                    "Enable promiscuous mode to capture frames (requires root/CAP_NET_RAW)"
                        .to_string(),
                required: false,
                example: json!(false),
            },
        ]
    }
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            inject_frame_action(false),
            ActionDefinition {
                name: "disconnect".to_string(),
                description: "Close the DataLink client and release the interface".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "disconnect"
                }),
                log_template: None,
            },
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![inject_frame_action(true), wait_for_more_action()]
    }
    fn protocol_name(&self) -> &'static str {
        "DataLink"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        // Clones of the statics this client actually raises, so a declaration cannot
        // drift from what is emitted. These were hand-written duplicates, and
        // `datalink_frame_injected` named an event nothing raised at all.
        vec![
            DATALINK_CLIENT_CONNECTED_EVENT.clone(),
            DATALINK_CLIENT_FRAME_INJECTED_EVENT.clone(),
            DATALINK_CLIENT_FRAME_CAPTURED_EVENT.clone(),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "datalink",
            "data link",
            "layer 2",
            "layer2",
            "l2",
            "ethernet",
            "frame",
            "inject",
            "pcap",
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            // Not RawSockets. This opens a libpcap handle, exactly like the DataLink *server*
            // (which declares PacketCapture) - never a SOCK_RAW socket. The two are separate
            // capabilities in `SystemCapabilities`, and a macOS user in the ChmodBPF group has
            // capture without raw sockets, so the wrong variant refuses a host that works.
            .privilege_requirement(PrivilegeRequirement::PacketCapture)
            .implementation("libpcap (pcap crate) for Layer 2 frame injection and capture")
            .llm_control("Full control over Ethernet frames (inject/capture)")
            .e2e_testing(
                "tests/client/datalink: `action_test.rs` pins the action/event surface with no \
                 privilege at all (every declared example is accepted by execute_action, and a \
                 runt, an over-long frame and bad hex are refused by name). \
                 `command_channel_test.rs` covers the dashboard's [ send ] path, including a \
                 real acknowledged `sendpacket` in its privileged half. The subprocess e2e \
                 tests open a real capture on loopback and inject real frames; they fail with \
                 an explicit message on a host without capture access rather than skipping.",
            )
            .notes(
                "Needs layer-2 capture access (/dev/bpf* on macOS/BSD, root or CAP_NET_RAW on \
                 Linux) for both injection and promiscuous mode. connect() awaits the pcap \
                 handle and returns Err when it cannot be opened, so a host without that \
                 access lands in ClientStatus::Error rather than reporting Connected having \
                 opened nothing. Frame injection has been observed end to end on macOS \
                 (2026-09-08) but only through tests that need that access. On an LLM failure \
                 nothing is injected - the outcome is in the log as decision=model_inject / \
                 model_silent / model_reject / fail_closed_llm_error. The inject -> \
                 datalink_frame_injected -> inject chain is capped at 4 hops; captured frames \
                 arriving while a model turn is running are dropped and counted, not queued.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "DataLink client for raw Ethernet frame injection"
    }
    fn example_prompt(&self) -> &'static str {
        "Inject ARP request on eth0"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        StartupExamples::new(
            // LLM mode: LLM handles DataLink client
            json!({
                "type": "open_client",
                "remote_addr": "eth0",
                "base_stack": "datalink",
                "instruction": "Inject an ARP request and capture responses",
                "startup_params": {
                    "interface": "eth0",
                    "promiscuous": true
                }
            }),
            // Script mode: Code-based frame handling
            json!({
                "type": "open_client",
                "remote_addr": "eth0",
                "base_stack": "datalink",
                "startup_params": {
                    "interface": "eth0",
                    "promiscuous": true
                },
                "event_handlers": [{
                    "event_pattern": "datalink_frame_captured",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "<datalink_handler>"
                    }
                }]
            }),
            // Static mode: Fixed frame injection
            json!({
                "type": "open_client",
                "remote_addr": "eth0",
                "base_stack": "datalink",
                "startup_params": {
                    "interface": "eth0",
                    "promiscuous": false
                },
                "event_handlers": [{
                    "event_pattern": "datalink_frame_injected",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "wait_for_more"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Client trait (client-specific functionality)
impl Client for DataLinkClientProtocol {
    fn connect(
        &self,
        ctx: crate::protocol::ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::client::datalink::DataLinkClient;
            DataLinkClient::connect_with_llm_actions(
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
            "inject_frame" => {
                let frame_hex = action
                    .get("frame_hex")
                    .and_then(|v| v.as_str())
                    .context("Missing 'frame_hex' field")?;

                // Tolerate the separators a model reading a packet dump naturally writes
                // ("ff:ff:ff:ff:ff:ff 00 11 …"): they carry no information and rejecting them
                // teaches the model nothing it can act on.
                let cleaned: String = frame_hex
                    .chars()
                    .filter(|c| !matches!(c, ' ' | ':' | '-' | '.' | '\n' | '\r' | '\t'))
                    .collect();

                let frame = hex::decode(&cleaned).context("Invalid hex frame data")?;

                // Bounds before libpcap sees it: a runt has no EtherType, and a frame the
                // size of a prompt is a mistake worth naming rather than an ENOBUFS from a
                // syscall. The model is told the actual length so it can fix the frame.
                if frame.len() < MIN_ETHERNET_FRAME_BYTES {
                    anyhow::bail!(
                        "frame_hex decodes to {} bytes; an Ethernet frame needs at least {} \
                         (6-byte destination MAC, 6-byte source MAC, 2-byte EtherType)",
                        frame.len(),
                        MIN_ETHERNET_FRAME_BYTES
                    );
                }
                if frame.len() > MAX_ETHERNET_FRAME_BYTES {
                    anyhow::bail!(
                        "frame_hex decodes to {} bytes; the maximum injectable frame is {}",
                        frame.len(),
                        MAX_ETHERNET_FRAME_BYTES
                    );
                }

                Ok(ClientActionResult::SendData(frame))
            }
            "disconnect" => Ok(ClientActionResult::Disconnect),
            "wait_for_more" => Ok(ClientActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!(
                "Unknown DataLink client action: {}",
                action_type
            )),
        }
    }
}
