//! OSPF protocol actions implementation

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use serde_json::json;
use std::sync::LazyLock;
use tracing::debug;

/// The interface configuration supplied through `get_startup_parameters()`.
///
/// Every one of the six declared startup parameters lands in this struct and is read:
///
/// * `router_id` / `area_id` / `network_mask` / `hello_interval` /
///   `router_dead_interval` / `router_priority` become the **defaults for every packet
///   the model sends** ([`apply_defaults`](Self::apply_defaults)). An action that names a
///   field still wins - the model can deliberately lie about its timers - but an action
///   that omits one now gets the operator's configured value instead of a hardcoded
///   constant.
/// * `hello_interval`, `router_dead_interval` and `network_mask` are additionally checked
///   against every received Hello ([`hello_mismatches`](Self::hello_mismatches)), because
///   RFC 2328 10.5 makes those three a precondition for accepting a Hello at all.
///
/// Before this existed, four of the six parameters were declared to the model and never
/// read anywhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OspfInterfaceConfig {
    pub router_id: String,
    pub area_id: String,
    pub network_mask: String,
    pub hello_interval: u16,
    pub router_dead_interval: u32,
    pub router_priority: u8,
}

impl Default for OspfInterfaceConfig {
    fn default() -> Self {
        // RFC 2328 Appendix C.3 defaults for a broadcast interface.
        Self {
            router_id: "0.0.0.0".to_string(),
            area_id: "0.0.0.0".to_string(),
            network_mask: "255.255.255.0".to_string(),
            hello_interval: 10,
            router_dead_interval: 40,
            router_priority: 1,
        }
    }
}

impl OspfInterfaceConfig {
    /// Fill in the fields the model left out of an outgoing packet action.
    ///
    /// `router_id` and `area_id` apply to every OSPF packet type; the Hello body fields
    /// are only added to `send_hello`, where they exist. Fields the action already
    /// carries are left untouched.
    pub fn apply_defaults(&self, action: &mut serde_json::Value) {
        let Some(obj) = action.as_object_mut() else {
            return;
        };

        obj.entry("router_id")
            .or_insert_with(|| json!(self.router_id));
        obj.entry("area_id").or_insert_with(|| json!(self.area_id));

        if obj.get("type").and_then(|t| t.as_str()) == Some("send_hello") {
            obj.entry("network_mask")
                .or_insert_with(|| json!(self.network_mask));
            obj.entry("hello_interval")
                .or_insert_with(|| json!(self.hello_interval));
            obj.entry("router_dead_interval")
                .or_insert_with(|| json!(self.router_dead_interval));
            obj.entry("priority")
                .or_insert_with(|| json!(self.router_priority));
        }
    }

    /// RFC 2328 10.5: a received Hello whose HelloInterval, RouterDeadInterval or network
    /// mask differs from the receiving interface's must be rejected - adjacency cannot form
    /// across such a mismatch on a broadcast network.
    ///
    /// Returns one human-readable line per mismatching field, empty when the Hello is
    /// compatible. The caller reports these to the model and refuses to advance the
    /// neighbour state machine, rather than pretending an adjacency is forming that a real
    /// router would never complete.
    pub fn hello_mismatches(
        &self,
        hello_interval: u16,
        router_dead_interval: u32,
        network_mask: &str,
    ) -> Vec<String> {
        let mut out = Vec::new();
        if hello_interval != self.hello_interval {
            out.push(format!(
                "hello_interval {} does not match our configured {}",
                hello_interval, self.hello_interval
            ));
        }
        if router_dead_interval != self.router_dead_interval {
            out.push(format!(
                "router_dead_interval {} does not match our configured {}",
                router_dead_interval, self.router_dead_interval
            ));
        }
        if network_mask != self.network_mask {
            out.push(format!(
                "network_mask {} does not match our configured {}",
                network_mask, self.network_mask
            ));
        }
        out
    }
}

/// OSPF protocol action handler
pub struct OspfProtocol;

impl OspfProtocol {
    pub fn new() -> Self {
        Self
    }

    fn execute_send_hello(&self, action: serde_json::Value) -> Result<ActionResult> {
        let router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let _network_mask = action
            .get("network_mask")
            .and_then(|v| v.as_str())
            .unwrap_or("255.255.255.0");

        let _hello_interval = action
            .get("hello_interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as u16;

        let _router_dead_interval = action
            .get("router_dead_interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(40) as u32;

        let priority = action.get("priority").and_then(|v| v.as_u64()).unwrap_or(1) as u8;

        let _dr = action
            .get("dr")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let _bdr = action
            .get("bdr")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let destination = action
            .get("destination")
            .and_then(|v| v.as_str())
            .unwrap_or("multicast")
            .to_string();

        debug!(
            "OSPF sending Hello: router_id={}, area={}, priority={}, dest={}",
            router_id, area_id, priority, destination
        );

        // Return structured action data - packet will be built in mod.rs
        Ok(ActionResult::Custom {
            name: "ospf_action".to_string(),
            data: action.clone(),
        })
    }

    fn execute_send_database_description(&self, action: serde_json::Value) -> Result<ActionResult> {
        let _router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let _area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let sequence = action.get("sequence").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

        let init = action
            .get("init")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let more = action
            .get("more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let master = action
            .get("master")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let destination = action
            .get("destination")
            .and_then(|v| v.as_str())
            .unwrap_or("multicast")
            .to_string();

        debug!(
            "OSPF sending Database Description: seq={}, init={}, more={}, master={}, dest={}",
            sequence, init, more, master, destination
        );

        // Return structured action data - packet will be built in mod.rs
        Ok(ActionResult::Custom {
            name: "ospf_action".to_string(),
            data: action.clone(),
        })
    }

    fn execute_send_link_state_request(&self, action: serde_json::Value) -> Result<ActionResult> {
        let _router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let _area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let destination = action
            .get("destination")
            .and_then(|v| v.as_str())
            .unwrap_or("multicast")
            .to_string();

        debug!("OSPF sending Link State Request to {}", destination);

        // Return structured action data - packet will be built in mod.rs
        Ok(ActionResult::Custom {
            name: "ospf_action".to_string(),
            data: action.clone(),
        })
    }

    fn execute_send_link_state_update(&self, action: serde_json::Value) -> Result<ActionResult> {
        let _router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let _area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let destination = action
            .get("destination")
            .and_then(|v| v.as_str())
            .unwrap_or("multicast")
            .to_string();

        debug!("OSPF sending Link State Update to {}", destination);

        // Return structured action data - packet will be built in mod.rs
        Ok(ActionResult::Custom {
            name: "ospf_action".to_string(),
            data: action.clone(),
        })
    }

    fn execute_send_link_state_ack(&self, action: serde_json::Value) -> Result<ActionResult> {
        let _router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let _area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let destination = action
            .get("destination")
            .and_then(|v| v.as_str())
            .unwrap_or("multicast")
            .to_string();

        debug!("OSPF sending Link State Acknowledgment to {}", destination);

        // Return structured action data - packet will be built in mod.rs
        Ok(ActionResult::Custom {
            name: "ospf_action".to_string(),
            data: action.clone(),
        })
    }

    // Helper: Parse IPv4 address string to bytes
    fn parse_ipv4(ip: &str) -> Result<[u8; 4]> {
        let parts: Vec<u8> = ip.split('.').filter_map(|s| s.parse::<u8>().ok()).collect();

        if parts.len() == 4 {
            Ok([parts[0], parts[1], parts[2], parts[3]])
        } else {
            Ok([0, 0, 0, 0])
        }
    }

    /// Compute the OSPF packet checksum (RFC 2328 Section A.3.1).
    ///
    /// This is the **standard IP (one's complement) checksum** over the entire packet with the
    /// checksum field itself treated as zero and the 64-bit authentication field (header bytes
    /// 16..24) excluded. It is *not* the Fletcher checksum of Section D.4 — that one applies to
    /// LSA headers, not to packet headers. A previous implementation here used Fletcher, which
    /// meant every packet NetGet emitted failed the receiver's validity check and was silently
    /// dropped by real routers (FRR/BIRD).
    ///
    /// The defining property, and how a receiver validates: recomputing this sum over the packet
    /// with the checksum field left in place must yield 0.
    pub fn calculate_checksum(data: &[u8]) -> u16 {
        let mut sum: u32 = 0;
        // Accumulate 16-bit big-endian words, skipping the auth field and zeroing the
        // checksum field. `pending` holds the high byte of a word still being assembled,
        // which matters because excluding bytes 16..24 keeps 16-bit alignment intact only
        // because that range is itself even-aligned and even-length.
        let mut pending: Option<u8> = None;
        for (i, &raw) in data.iter().enumerate() {
            // Skip the 64-bit authentication field entirely.
            if (16..24).contains(&i) {
                continue;
            }
            // The checksum field contributes as zero.
            let byte = if i == 12 || i == 13 { 0 } else { raw };
            match pending.take() {
                None => pending = Some(byte),
                Some(hi) => sum += u16::from_be_bytes([hi, byte]) as u32,
            }
        }
        // Odd-length packet: pad with a zero byte.
        if let Some(hi) = pending {
            sum += u16::from_be_bytes([hi, 0]) as u32;
        }
        while (sum >> 16) != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    /// Build OSPF Hello packet from action data
    pub fn build_hello_packet(action: &serde_json::Value) -> Result<Vec<u8>> {
        let router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let network_mask = action
            .get("network_mask")
            .and_then(|v| v.as_str())
            .unwrap_or("255.255.255.0");
        let hello_interval = action
            .get("hello_interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(10) as u16;
        let router_dead_interval = action
            .get("router_dead_interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(40) as u32;
        let priority = action.get("priority").and_then(|v| v.as_u64()).unwrap_or(1) as u8;
        let dr = action
            .get("dr")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let bdr = action
            .get("bdr")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let mut msg = Vec::new();

        // OSPF Header (24 bytes)
        msg.push(2); // Version = 2 (OSPFv2)
        msg.push(1); // Type = 1 (Hello)
        msg.extend_from_slice(&[0, 0]); // Packet Length (placeholder)
        msg.extend_from_slice(&Self::parse_ipv4(router_id)?);
        msg.extend_from_slice(&Self::parse_ipv4(area_id)?);
        msg.extend_from_slice(&[0, 0]); // Checksum (placeholder)
        msg.extend_from_slice(&[0, 0]); // AuType = 0 (no authentication)
        msg.extend_from_slice(&[0; 8]); // Authentication (8 bytes, zeros)

        // Hello packet body
        msg.extend_from_slice(&Self::parse_ipv4(network_mask)?);
        msg.extend_from_slice(&hello_interval.to_be_bytes());
        msg.push(0); // Options
        msg.push(priority);
        msg.extend_from_slice(&router_dead_interval.to_be_bytes());
        msg.extend_from_slice(&Self::parse_ipv4(dr)?);
        msg.extend_from_slice(&Self::parse_ipv4(bdr)?);

        // Neighbor list
        if let Some(neighbors) = action.get("neighbors").and_then(|v| v.as_array()) {
            for neighbor in neighbors {
                if let Some(neighbor_id) = neighbor.as_str() {
                    msg.extend_from_slice(&Self::parse_ipv4(neighbor_id)?);
                }
            }
        }

        // Update packet length and checksum
        let packet_len = msg.len() as u16;
        msg[2..4].copy_from_slice(&packet_len.to_be_bytes());
        let checksum = Self::calculate_checksum(&msg);
        msg[12..14].copy_from_slice(&checksum.to_be_bytes());

        Ok(msg)
    }

    /// Build OSPF Database Description packet from action data
    pub fn build_database_description_packet(action: &serde_json::Value) -> Result<Vec<u8>> {
        let router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let sequence = action.get("sequence").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let init = action
            .get("init")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let more = action
            .get("more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let master = action
            .get("master")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut msg = Vec::new();

        // OSPF Header
        msg.push(2); // Version
        msg.push(2); // Type = 2 (Database Description)
        msg.extend_from_slice(&[0, 0]); // Packet Length (placeholder)
        msg.extend_from_slice(&Self::parse_ipv4(router_id)?);
        msg.extend_from_slice(&Self::parse_ipv4(area_id)?);
        msg.extend_from_slice(&[0, 0]); // Checksum (placeholder)
        msg.extend_from_slice(&[0, 0]); // AuType
        msg.extend_from_slice(&[0; 8]); // Authentication

        // DD packet body
        msg.extend_from_slice(&[0, 0]); // Interface MTU
        msg.push(0); // Options
        let mut flags: u8 = 0;
        if init {
            flags |= 0x04;
        }
        if more {
            flags |= 0x02;
        }
        if master {
            flags |= 0x01;
        }
        msg.push(flags);
        msg.extend_from_slice(&sequence.to_be_bytes());

        // LSA headers would go here (simplified - empty for now)

        // Update packet length and checksum
        let packet_len = msg.len() as u16;
        msg[2..4].copy_from_slice(&packet_len.to_be_bytes());
        let checksum = Self::calculate_checksum(&msg);
        msg[12..14].copy_from_slice(&checksum.to_be_bytes());

        Ok(msg)
    }

    /// Build OSPF Link State Request packet from action data
    pub fn build_link_state_request_packet(action: &serde_json::Value) -> Result<Vec<u8>> {
        let router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let mut msg = Vec::new();

        // OSPF Header
        msg.push(2); // Version
        msg.push(3); // Type = 3 (Link State Request)
        msg.extend_from_slice(&[0, 0]); // Packet Length (placeholder)
        msg.extend_from_slice(&Self::parse_ipv4(router_id)?);
        msg.extend_from_slice(&Self::parse_ipv4(area_id)?);
        msg.extend_from_slice(&[0, 0]); // Checksum (placeholder)
        msg.extend_from_slice(&[0, 0]); // AuType
        msg.extend_from_slice(&[0; 8]); // Authentication

        // LSR body (simplified - would contain list of requested LSAs)

        // Update packet length and checksum
        let packet_len = msg.len() as u16;
        msg[2..4].copy_from_slice(&packet_len.to_be_bytes());
        let checksum = Self::calculate_checksum(&msg);
        msg[12..14].copy_from_slice(&checksum.to_be_bytes());

        Ok(msg)
    }

    /// Build OSPF Link State Update packet from action data
    pub fn build_link_state_update_packet(action: &serde_json::Value) -> Result<Vec<u8>> {
        let router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let mut msg = Vec::new();

        // OSPF Header
        msg.push(2); // Version
        msg.push(4); // Type = 4 (Link State Update)
        msg.extend_from_slice(&[0, 0]); // Packet Length (placeholder)
        msg.extend_from_slice(&Self::parse_ipv4(router_id)?);
        msg.extend_from_slice(&Self::parse_ipv4(area_id)?);
        msg.extend_from_slice(&[0, 0]); // Checksum (placeholder)
        msg.extend_from_slice(&[0, 0]); // AuType
        msg.extend_from_slice(&[0; 8]); // Authentication

        // Number of LSAs
        msg.extend_from_slice(&[0, 0, 0, 0]); // 0 LSAs (simplified)

        // LSAs would go here

        // Update packet length and checksum
        let packet_len = msg.len() as u16;
        msg[2..4].copy_from_slice(&packet_len.to_be_bytes());
        let checksum = Self::calculate_checksum(&msg);
        msg[12..14].copy_from_slice(&checksum.to_be_bytes());

        Ok(msg)
    }

    /// Build OSPF Link State Acknowledgment packet from action data
    pub fn build_link_state_ack_packet(action: &serde_json::Value) -> Result<Vec<u8>> {
        let router_id = action
            .get("router_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");
        let area_id = action
            .get("area_id")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0");

        let mut msg = Vec::new();

        // OSPF Header
        msg.push(2); // Version
        msg.push(5); // Type = 5 (Link State Acknowledgment)
        msg.extend_from_slice(&[0, 0]); // Packet Length (placeholder)
        msg.extend_from_slice(&Self::parse_ipv4(router_id)?);
        msg.extend_from_slice(&Self::parse_ipv4(area_id)?);
        msg.extend_from_slice(&[0, 0]); // Checksum (placeholder)
        msg.extend_from_slice(&[0, 0]); // AuType
        msg.extend_from_slice(&[0; 8]); // Authentication

        // LSA headers to acknowledge would go here

        // Update packet length and checksum
        let packet_len = msg.len() as u16;
        msg[2..4].copy_from_slice(&packet_len.to_be_bytes());
        let checksum = Self::calculate_checksum(&msg);
        msg[12..14].copy_from_slice(&checksum.to_be_bytes());

        Ok(msg)
    }
}

// ============================================================================
// Action Definitions (shared between get_sync_actions() and the event types below).
//
// `call_llm` builds the model's tool list from `EventType::actions`, NOT from
// get_sync_actions(), so every event below must list the actions it accepts.
// ============================================================================

fn send_hello_action() -> ActionDefinition {
    ActionDefinition {
                name: "send_hello".to_string(),
                description: "Send OSPF Hello packet to discover/maintain neighbors".to_string(),
                parameters: vec![
                    Parameter {
                        name: "router_id".to_string(),
                        type_hint: "string".to_string(),
                        description: "OSPF router ID (IPv4 format)".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "area_id".to_string(),
                        type_hint: "string".to_string(),
                        description: "OSPF area ID (IPv4 format, 0.0.0.0 = backbone)".to_string(),
                        required: true,
                    },
                    Parameter {
                        name: "network_mask".to_string(),
                        type_hint: "string".to_string(),
                        description: "Network mask (e.g., 255.255.255.0)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "hello_interval".to_string(),
                        type_hint: "number".to_string(),
                        description: "Hello interval in seconds (default 10)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "router_dead_interval".to_string(),
                        type_hint: "number".to_string(),
                        description: "Router dead interval in seconds (default 40)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "priority".to_string(),
                        type_hint: "number".to_string(),
                        description: "Router priority for DR election (0-255, default 1)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "dr".to_string(),
                        type_hint: "string".to_string(),
                        description: "Designated Router IP (0.0.0.0 if none)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "bdr".to_string(),
                        type_hint: "string".to_string(),
                        description: "Backup Designated Router IP (0.0.0.0 if none)".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "neighbors".to_string(),
                        type_hint: "array".to_string(),
                        description: "List of neighbor router IDs".to_string(),
                        required: false,
                    },
                    Parameter {
                        name: "destination".to_string(),
                        type_hint: "string".to_string(),
                        description: "Destination IP: 'multicast' (default, 224.0.0.5), 'dr_multicast' (224.0.0.6), or unicast IP".to_string(),
                        required: false,
                    },
                ],
                example: json!({
                    "type": "send_hello",
                    "router_id": "1.1.1.1",
                    "area_id": "0.0.0.0",
                    "priority": 1,
                    "neighbors": ["2.2.2.2"],
                    "destination": "multicast"
                }),
                log_template: Some(
                    LogTemplate::new()
                        .with_info("-> OSPF Hello router={router_id} area={area_id}")
                        .with_debug("OSPF send_hello: router_id={router_id} area={area_id} priority={priority}"),
                ),
            }
}

fn send_database_description_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_database_description".to_string(),
        description: "Send OSPF Database Description packet".to_string(),
        parameters: vec![
            Parameter {
                name: "router_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF router ID".to_string(),
                required: true,
            },
            Parameter {
                name: "area_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF area ID".to_string(),
                required: true,
            },
            Parameter {
                name: "sequence".to_string(),
                type_hint: "number".to_string(),
                description: "DD sequence number".to_string(),
                required: true,
            },
            Parameter {
                name: "init".to_string(),
                type_hint: "boolean".to_string(),
                description: "Init flag (true for first DD packet)".to_string(),
                required: false,
            },
            Parameter {
                name: "more".to_string(),
                type_hint: "boolean".to_string(),
                description: "More flag (true if more DD packets follow)".to_string(),
                required: false,
            },
            Parameter {
                name: "master".to_string(),
                type_hint: "boolean".to_string(),
                description: "Master flag (true if this router is master)".to_string(),
                required: false,
            },
            Parameter {
                name: "destination".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP: 'multicast' (default) or unicast IP".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_database_description",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "sequence": 1,
            "init": true,
            "master": true,
            "destination": "192.168.1.2"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> OSPF DD seq={sequence}")
                .with_debug(
                "OSPF send_database_description: router_id={router_id} seq={sequence} init={init}",
            ),
        ),
    }
}

fn send_link_state_request_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_link_state_request".to_string(),
        description: "Send OSPF Link State Request packet".to_string(),
        parameters: vec![
            Parameter {
                name: "router_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF router ID".to_string(),
                required: true,
            },
            Parameter {
                name: "area_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF area ID".to_string(),
                required: true,
            },
            Parameter {
                name: "destination".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP: 'multicast' (default) or unicast IP".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_link_state_request",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "destination": "192.168.1.2"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> OSPF LSR to {destination}")
                .with_debug(
                    "OSPF send_link_state_request: router_id={router_id} dest={destination}",
                ),
        ),
    }
}

fn send_link_state_update_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_link_state_update".to_string(),
        description: "Send OSPF Link State Update packet".to_string(),
        parameters: vec![
            Parameter {
                name: "router_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF router ID".to_string(),
                required: true,
            },
            Parameter {
                name: "area_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF area ID".to_string(),
                required: true,
            },
            Parameter {
                name: "destination".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP: 'multicast' (default) or unicast IP".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_link_state_update",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "destination": "multicast"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> OSPF LSU to {destination}")
                .with_debug(
                    "OSPF send_link_state_update: router_id={router_id} dest={destination}",
                ),
        ),
    }
}

fn send_link_state_ack_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_link_state_ack".to_string(),
        description: "Send OSPF Link State Acknowledgment packet".to_string(),
        parameters: vec![
            Parameter {
                name: "router_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF router ID".to_string(),
                required: true,
            },
            Parameter {
                name: "area_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF area ID".to_string(),
                required: true,
            },
            Parameter {
                name: "destination".to_string(),
                type_hint: "string".to_string(),
                description: "Destination IP: 'multicast' (default) or unicast IP".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_link_state_ack",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "destination": "192.168.1.2"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> OSPF LSAck to {destination}")
                .with_debug("OSPF send_link_state_ack: router_id={router_id} dest={destination}"),
        ),
    }
}

fn wait_for_more_action() -> ActionDefinition {
    ActionDefinition {
        name: "wait_for_more".to_string(),
        description: "Wait for more OSPF packets before responding".to_string(),
        parameters: vec![],
        example: json!({
            "type": "wait_for_more"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("-> OSPF wait for more")
                .with_debug("OSPF wait_for_more"),
        ),
    }
}

// ============================================================================
// Event Types
//
// Every event advertises the actions it accepts. `call_llm` builds the model's
// tool list from `EventType::actions`; an event that leaves it empty offers the
// model nothing and trips a debug_assert. The response_example is rendered
// verbatim into the prompt, so it must be a real, executable action - never a
// `{"type": "placeholder"}` stub.
// ============================================================================

/// Actions any OSPF neighbor event can respond with.
fn ospf_response_actions() -> Vec<ActionDefinition> {
    vec![
        send_hello_action(),
        send_database_description_action(),
        send_link_state_request_action(),
        send_link_state_update_action(),
        send_link_state_ack_action(),
        wait_for_more_action(),
    ]
}

pub static OSPF_HELLO_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ospf_hello",
        "OSPF Hello received. Answer with send_hello listing this neighbour in your own \
         neighbor list — until your Hello names it back, the neighbour stays in Init and the \
         adjacency never forms (RFC 2328 §10.5).",
        json!({
            "type": "send_hello",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "network_mask": "255.255.255.0",
            "priority": 1,
            "dr": "0.0.0.0",
            "bdr": "0.0.0.0",
            "neighbors": ["2.2.2.2"],
            "destination": "multicast"
        }),
    )
    .with_actions(ospf_response_actions())
    .with_log_template(
        LogTemplate::new()
            .with_info("OSPF Hello from {neighbor_id}")
            .with_debug(
                "OSPF Hello: neighbor={neighbor_id} area={area_id} priority={router_priority}",
            )
            .with_trace("OSPF Hello: {json_pretty(.)}"),
    )
});

pub static OSPF_DATABASE_DESCRIPTION_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ospf_database_description",
        "OSPF Database Description received (adjacency Exchange phase). Answer with \
         send_database_description to continue the exchange; silence leaves both routers in \
         ExStart retransmitting their DD packets.",
        json!({
            "type": "send_database_description",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "sequence": 1,
            "init": false,
            "more": false,
            "master": false,
            "destination": "192.168.1.2"
        }),
    )
    .with_actions(ospf_response_actions())
    .with_log_template(
        LogTemplate::new()
            .with_info("OSPF DD from {neighbor_id} seq={dd_sequence}")
            .with_debug(
                "OSPF Database Description: neighbor={neighbor_id} seq={dd_sequence} \
                 init={init} more={more} master={master}",
            )
            .with_trace("OSPF DD: {json_pretty(.)}"),
    )
});

pub static OSPF_LINK_STATE_REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ospf_link_state_request",
        "OSPF Link State Request received: the neighbor is asking for LSAs it does not have. \
         Answer with send_link_state_update carrying them — a request left unanswered stalls \
         the adjacency in Loading and it never reaches Full (RFC 2328 §10.9). Describing what \
         you would send is not sending it: only the action puts a packet on the wire.",
        json!({
            "type": "send_link_state_update",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "destination": "192.168.1.2"
        }),
    )
    .with_actions(ospf_response_actions())
    .with_log_template(
        LogTemplate::new()
            .with_info("OSPF LSR from {neighbor_id} ({request_count} LSAs)")
            .with_debug("OSPF Link State Request: neighbor={neighbor_id} requests={request_count}")
            .with_trace("OSPF LSR: {json_pretty(.)}"),
    )
});

pub static OSPF_LINK_STATE_UPDATE_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ospf_link_state_update",
        "OSPF Link State Update received: the neighbor is flooding LSAs. Answer with \
         send_link_state_ack — every LSA must be acknowledged or the neighbor retransmits it \
         every RxmtInterval until the adjacency fails (RFC 2328 §13.5). Describing the \
         acknowledgement is not sending it: only the action puts a packet on the wire.",
        json!({
            "type": "send_link_state_ack",
            "router_id": "1.1.1.1",
            "area_id": "0.0.0.0",
            "destination": "192.168.1.2"
        }),
    )
    .with_actions(ospf_response_actions())
    .with_log_template(
        LogTemplate::new()
            .with_info("OSPF LSU from {neighbor_id} ({lsa_count} LSAs)")
            .with_debug("OSPF Link State Update: neighbor={neighbor_id} lsas={lsa_count}")
            .with_trace("OSPF LSU: {json_pretty(.)}"),
    )
});

pub static OSPF_LINK_STATE_ACK_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "ospf_link_state_ack",
        "OSPF Link State Acknowledgment packet received",
        json!({ "type": "wait_for_more" }),
    )
    .with_actions(ospf_response_actions())
    .with_log_template(
        LogTemplate::new()
            .with_info("OSPF LSAck from {neighbor_id} ({lsa_count} LSAs)")
            .with_debug("OSPF Link State Acknowledgment: neighbor={neighbor_id} lsas={lsa_count}")
            .with_trace("OSPF LSAck: {json_pretty(.)}"),
    )
});

// Implement Protocol trait (common functionality)
impl Protocol for OspfProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        vec![
            ActionDefinition {
                name: "list_neighbors".to_string(),
                description: "List all OSPF neighbors and their states".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "list_neighbors"
                }),
                log_template: Some(
                    LogTemplate::new()
                        .with_info("-> OSPF list neighbors")
                        .with_debug("OSPF list_neighbors"),
                ),
            },
            ActionDefinition {
                name: "list_lsdb".to_string(),
                description: "List Link State Database entries".to_string(),
                parameters: vec![],
                example: json!({
                    "type": "list_lsdb"
                }),
                log_template: Some(
                    LogTemplate::new()
                        .with_info("-> OSPF list LSDB")
                        .with_debug("OSPF list_lsdb"),
                ),
            },
        ]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_hello_action(),
            send_database_description_action(),
            send_link_state_request_action(),
            send_link_state_update_action(),
            send_link_state_ack_action(),
            wait_for_more_action(),
        ]
    }

    fn protocol_name(&self) -> &'static str {
        "OSPF"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            OSPF_HELLO_EVENT.clone(),
            OSPF_DATABASE_DESCRIPTION_EVENT.clone(),
            OSPF_LINK_STATE_REQUEST_EVENT.clone(),
            OSPF_LINK_STATE_UPDATE_EVENT.clone(),
            OSPF_LINK_STATE_ACK_EVENT.clone(),
        ]
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP(89)>OSPF"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["ospf", "open shortest path first"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
                .state(DevelopmentState::Experimental)
                // Raw socket on IP protocol 89 - CAP_NET_RAW is sufficient, full root is not
                // required, so declaring Root would refuse to start on a capability-only process.
                .privilege_requirement(PrivilegeRequirement::RawSockets)
                .implementation("Manual OSPFv2 (RFC 2328) over a raw IP-protocol-89 socket. Hello is parsed in full; DD/LSR/LSU/LSAck are parsed to their headers only. Outgoing DD carries no LSA headers and outgoing LSR/LSU/LSAck carry empty bodies - they are valid packets that advertise nothing.")
                .llm_control("Optional: whether to engage with an OSPF speaker (respond to a Hello, claim DR/BDR, act as a honeypot) is a policy decision. With no operator policy (no instruction, no handler) the server observes passively and does NOT respond, with no LLM round-trip per packet. When the operator opts in, every received packet type raises an event carrying parsed fields and the LLM chooses the reply packet (it cannot yet put LSA contents into a reply). Fields the reply omits are filled from the interface configuration given at startup (router_id, area_id, network_mask, hello_interval, router_dead_interval, router_priority).")
                .e2e_testing("None against a real router. The E2E suite runs the OSPF wire format over a plain UDP server and never exercises the raw-socket path, so it proves nothing about the raw-socket implementation. What IS verified at byte level (tests/server/ospf/e2e_test.rs): build_hello_packet writes the configured hello_interval, router_dead_interval, priority and network mask into the Hello body at RFC 2328 A.3.2 offsets when the action omits them, an action-supplied value overrides the configured one, and the RFC 2328 10.5 mismatch check fires on exactly the three fields the RFC names.")
                .notes("Hello-level simulator, not a router. Whether to respond at all is policy, so with no operator policy the server is a passive listener (no response, no LLM call); the passive default is compile-verified since the raw-socket path needs root and the E2E suite never touches it. No LSDB, no SPF, no routing table, no LSA construction, no DR/BDR election, no periodic Hello timer and no dead-neighbor timeout. Adjacency cannot progress past 2-Way, and a Hello that fails the RFC 2328 10.5 interval/mask check does not advance it at all - the event still reaches the model, carrying the mismatch, so the refusal is visible rather than silent. On an LLM failure the server deliberately sends nothing: OSPF has no error or NAK packet, and every one of its five packet types is a positive routing assertion, so any fabricated reply would claim an adjacency, DR role or database state netget cannot back - the peer's own RouterDeadInterval covers a router that goes quiet. The failure is reported to the operator only, tagged decision=fail_closed_overloaded / fail_closed_unavailable and distinct from decision=model_wait / model_no_action.")
                .build()
    }
    fn description(&self) -> &'static str {
        "OSPF routing protocol server"
    }
    fn example_prompt(&self) -> &'static str {
        "Start an OSPF server on interface 192.168.1.100 as router 1.1.1.1 in area 0.0.0.0"
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        // All six are read in ProtocolConfig terms by OspfServer::spawn_with_llm_actions into
        // an OspfInterfaceConfig, which supplies the defaults for every outgoing packet and
        // the RFC 2328 10.5 acceptance check for every incoming Hello. Four of them used to
        // be declared here and read nowhere.
        vec![
            ParameterDefinition {
                name: "router_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF router ID in IPv4 address format (e.g., 1.1.1.1). Used as the Router ID of every packet sent unless the action overrides it. Defaults to the interface address.".to_string(),
                required: false,
                example: json!("1.1.1.1"),
            },
            ParameterDefinition {
                name: "area_id".to_string(),
                type_hint: "string".to_string(),
                description: "OSPF area ID in IPv4 format (0.0.0.0 = backbone area). Used as the Area ID of every packet sent unless the action overrides it.".to_string(),
                required: false,
                example: json!("0.0.0.0"),
            },
            ParameterDefinition {
                name: "network_mask".to_string(),
                type_hint: "string".to_string(),
                description: "Network mask of this interface (e.g., 255.255.255.0). Placed in outgoing Hello packets, and compared against incoming ones: RFC 2328 10.5 requires a Hello whose mask differs to be rejected.".to_string(),
                required: false,
                example: json!("255.255.255.0"),
            },
            ParameterDefinition {
                name: "hello_interval".to_string(),
                type_hint: "integer".to_string(),
                description: "HelloInterval in seconds (default 10). Placed in outgoing Hello packets. A neighbour whose Hello advertises a different value is rejected per RFC 2328 10.5 and its adjacency is not advanced.".to_string(),
                required: false,
                example: json!(10),
            },
            ParameterDefinition {
                name: "router_dead_interval".to_string(),
                type_hint: "integer".to_string(),
                description: "RouterDeadInterval in seconds (default 40). Placed in outgoing Hello packets and, like hello_interval, must match a neighbour's for its Hello to be accepted.".to_string(),
                required: false,
                example: json!(40),
            },
            ParameterDefinition {
                name: "router_priority".to_string(),
                type_hint: "integer".to_string(),
                description: "Router priority for DR election (0-255, default 1). Placed in outgoing Hello packets unless the send_hello action sets its own 'priority'. No election algorithm runs - the value is advertised, nothing more.".to_string(),
                required: false,
                example: json!(1),
            },
        ]
    }
    fn group_name(&self) -> &'static str {
        "VPN & Routing"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;

        StartupExamples::new(
            // LLM mode: LLM simulates OSPF router behavior
            json!({
                "type": "open_server",
                "port": 0,
                "base_stack": "ospf",
                "instruction": "OSPF router with ID 192.168.1.1 in area 0. Respond to Hello packets from neighbors. Claim DR role with priority 100."
            }),
            // Script mode: Scripted OSPF Hello response
            json!({
                "type": "open_server",
                "port": 0,
                "base_stack": "ospf",
                "event_handlers": [{
                    "event_pattern": "ospf_hello",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": "return {type='send_hello', router_id='192.168.1.1', area_id='0.0.0.0', network_mask='255.255.255.0', priority=100, dr='192.168.1.1', bdr='0.0.0.0', neighbors={event.neighbor_id}}"
                    }
                }]
            }),
            // Static mode: Fixed OSPF Hello response
            json!({
                "type": "open_server",
                "port": 0,
                "base_stack": "ospf",
                "event_handlers": [{
                    "event_pattern": "ospf_hello",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "send_hello",
                            "router_id": "192.168.1.1",
                            "area_id": "0.0.0.0",
                            "network_mask": "255.255.255.0",
                            "priority": 1,
                            "dr": "0.0.0.0",
                            "bdr": "0.0.0.0"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for OspfProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::ospf::OspfServer;
            OspfServer::spawn_with_llm_actions(
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
            .context("Missing action type")?;

        match action_type {
            "send_hello" => self.execute_send_hello(action),
            "send_database_description" => self.execute_send_database_description(action),
            "send_link_state_request" => self.execute_send_link_state_request(action),
            "send_link_state_update" => self.execute_send_link_state_update(action),
            "send_link_state_ack" => self.execute_send_link_state_ack(action),
            "wait_for_more" => Ok(ActionResult::WaitForMore),
            _ => Err(anyhow::anyhow!("Unknown OSPF action type: {}", action_type)),
        }
    }
}
