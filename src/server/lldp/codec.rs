//! Pure LLDP (IEEE 802.1AB) frame codec.
//!
//! Everything in this file is a **pure function over plain values**: it opens no socket, reads
//! no configuration and touches no global state. That is deliberate. The LLDP transport is raw
//! Ethernet and needs `CAP_NET_RAW` / `/dev/bpf*`, which no test in this repository has, so the
//! transport can never be executed here — but the frame format can be, and is, checked against
//! literal bytes from the specification and from a real capture
//! (`tests/server/lldp/codec_test.rs`).
//!
//! This is the `bluetooth_ble_beacon` split the root `CLAUDE.md` describes: payload construction
//! is pure and exhaustively tested; the platform transport is a thin layer over it that has
//! never run. `metadata().notes` says both.
//!
//! # Frame layout
//!
//! ```text
//! | dst MAC (6) | src MAC (6) | 0x88CC (2) | LLDPDU ... |
//! ```
//!
//! An LLDPDU is a sequence of TLVs. Each TLV starts with two octets holding a 7-bit type and a
//! 9-bit length:
//!
//! ```text
//!  0                   1
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |     type (7)  |   length (9)  |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! IEEE 802.1AB-2016 §8.5 requires the first three TLVs, in this order: Chassis ID (1),
//! Port ID (2), Time To Live (3). Optional TLVs follow, and End Of LLDPDU (type 0, length 0)
//! terminates. Chassis ID and Port ID each carry a **leading subtype octet** that decides how
//! the rest of the value is read — a MAC address, a network address or text — and getting that
//! wrong is the classic way to produce a frame a neighbour silently discards.
//!
//! # No bytes cross the LLM boundary
//!
//! Every identifier is carried in and out of here as a **string in its natural notation** —
//! `"00:01:30:f9:ad:a0"` for a MAC, `"192.0.2.1"` for an address, `"1/1"` for an interface
//! name — and every subtype and capability as a **name**. Models cannot reliably produce or
//! read a TLV blob, and a `tlv_hex` parameter would defeat the entire point of the protocol
//! (see the action & event design rules in the root `CLAUDE.md`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

/// EtherType assigned to LLDP (IEEE 802.1AB §8.1).
pub const LLDP_ETHERTYPE: u16 = 0x88CC;

/// The nearest-bridge group address every LLDP agent transmits to (802.1AB Table 7-1).
pub const LLDP_MULTICAST_MAC: [u8; 6] = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x0E];

/// Destination MAC | source MAC | EtherType.
pub const ETHERNET_HEADER_LEN: usize = 14;

// ---------------------------------------------------------------------------------------------
// TLV types (802.1AB-2016 Table 8-1)
// ---------------------------------------------------------------------------------------------

pub const TLV_END_OF_LLDPDU: u8 = 0;
pub const TLV_CHASSIS_ID: u8 = 1;
pub const TLV_PORT_ID: u8 = 2;
pub const TLV_TIME_TO_LIVE: u8 = 3;
pub const TLV_PORT_DESCRIPTION: u8 = 4;
pub const TLV_SYSTEM_NAME: u8 = 5;
pub const TLV_SYSTEM_DESCRIPTION: u8 = 6;
pub const TLV_SYSTEM_CAPABILITIES: u8 = 7;
pub const TLV_MANAGEMENT_ADDRESS: u8 = 8;

/// The 9-bit length field caps any single TLV value.
const MAX_TLV_VALUE: usize = 511;

/// 802.1AB caps identifiers and the text TLVs at 255 octets, well below the 9-bit field.
const MAX_STRING_TLV_VALUE: usize = 255;

/// Chassis ID subtypes, 802.1AB-2016 Table 8-2.
pub const CHASSIS_ID_SUBTYPES: &[(u8, &str)] = &[
    (1, "chassis_component"),
    (2, "interface_alias"),
    (3, "port_component"),
    (4, "mac_address"),
    (5, "network_address"),
    (6, "interface_name"),
    (7, "local"),
];

/// Port ID subtypes, 802.1AB-2016 Table 8-3. **Not the same numbering as Chassis ID** — a MAC
/// address is 4 for a chassis and 3 for a port, which is the single easiest thing to get wrong
/// in this protocol.
pub const PORT_ID_SUBTYPES: &[(u8, &str)] = &[
    (1, "interface_alias"),
    (2, "port_component"),
    (3, "mac_address"),
    (4, "network_address"),
    (5, "interface_name"),
    (6, "agent_circuit_id"),
    (7, "local"),
];

/// System capability bits, 802.1AB-2016 Table 8-4.
pub const SYSTEM_CAPABILITIES: &[(u16, &str)] = &[
    (0x0001, "other"),
    (0x0002, "repeater"),
    (0x0004, "bridge"),
    (0x0008, "wlan_access_point"),
    (0x0010, "router"),
    (0x0020, "telephone"),
    (0x0040, "docsis_cable_device"),
    (0x0080, "station_only"),
    (0x0100, "c_vlan_component"),
    (0x0200, "s_vlan_component"),
    (0x0400, "two_port_mac_relay"),
];

/// The IANA address family numbers LLDP management addresses use in practice.
const ADDRESS_FAMILIES: &[(u8, &str)] = &[(1, "ipv4"), (2, "ipv6"), (6, "mac_address")];

/// Which side of the frame an identifier belongs to. The two use different subtype numbers for
/// the same thing, so the value encoder has to know which table it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Chassis,
    Port,
}

impl IdKind {
    fn table(self) -> &'static [(u8, &'static str)] {
        match self {
            IdKind::Chassis => CHASSIS_ID_SUBTYPES,
            IdKind::Port => PORT_ID_SUBTYPES,
        }
    }

    fn mac_subtype(self) -> u8 {
        match self {
            IdKind::Chassis => 4,
            IdKind::Port => 3,
        }
    }

    fn network_address_subtype(self) -> u8 {
        match self {
            IdKind::Chassis => 5,
            IdKind::Port => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            IdKind::Chassis => "chassis_id",
            IdKind::Port => "port_id",
        }
    }
}

/// Look a subtype name up in the relevant table.
pub fn subtype_code(kind: IdKind, name: &str) -> Option<u8> {
    let wanted = name.trim().to_ascii_lowercase();
    kind.table()
        .iter()
        .find(|(_, n)| *n == wanted)
        .map(|(code, _)| *code)
}

/// Render a subtype number as its name, or `"reserved_<n>"` for one the spec does not define.
pub fn subtype_name(kind: IdKind, code: u8) -> String {
    kind.table()
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| format!("reserved_{code}"))
}

/// Split a capability bitmask into names. Bits with no assigned meaning are reported as
/// `reserved_bit_<n>` rather than dropped — a neighbour that sets one is telling us something,
/// even if the spec has not named it yet.
pub fn capability_names(bits: u16) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..16 {
        let bit = 1u16 << i;
        if bits & bit == 0 {
            continue;
        }
        match SYSTEM_CAPABILITIES.iter().find(|(b, _)| *b == bit) {
            Some((_, name)) => out.push((*name).to_string()),
            None => out.push(format!("reserved_bit_{i}")),
        }
    }
    out
}

/// Turn capability names back into a bitmask. An unrecognised name is an error, not a silently
/// dropped bit: a model that asks to advertise `"swtich"` must be told, or it will believe it
/// claimed to be a bridge.
pub fn capability_bits(names: &[String]) -> Result<u16> {
    let mut bits = 0u16;
    for name in names {
        let wanted = name.trim().to_ascii_lowercase();
        let found = SYSTEM_CAPABILITIES
            .iter()
            .find(|(_, n)| *n == wanted)
            .map(|(b, _)| *b);
        match found {
            Some(b) => bits |= b,
            None => bail!(
                "'{}' is not an LLDP system capability. Valid names: {}",
                name,
                SYSTEM_CAPABILITIES
                    .iter()
                    .map(|(_, n)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
    Ok(bits)
}

/// Parse `aa:bb:cc:dd:ee:ff` (or the `-`/`.`-free spellings) into six octets.
pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    if cleaned.len() != 12 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("'{text}' is not a MAC address (expected 6 hex octets, e.g. 00:01:30:f9:ad:a0)");
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("'{text}' is not a MAC address"))?;
    }
    Ok(out)
}

/// Render six octets as lowercase colon-separated hex.
pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

// ---------------------------------------------------------------------------------------------
// The decoded / to-be-encoded LLDPDU
// ---------------------------------------------------------------------------------------------

/// One management address, as carried by TLV type 8.
///
/// The trailing object identifier is decoded far enough to validate the TLV's length and then
/// discarded: it is an SNMP OID, of no use to a model, and nothing here can produce one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementAddress {
    /// IANA address family number: 1 = IPv4, 2 = IPv6, 6 = MAC.
    pub family: u8,
    /// The address in its natural notation for that family.
    pub address: String,
    /// Interface numbering subtype: 1 = unknown, 2 = ifIndex, 3 = system port number.
    pub interface_numbering_subtype: u8,
    /// The interface number itself.
    pub interface_number: u32,
}

impl ManagementAddress {
    /// Build one from an address string, deducing the family from its notation.
    pub fn new(address: &str, interface_number: u32) -> Result<Self> {
        let trimmed = address.trim();
        let family = if trimmed.parse::<std::net::Ipv4Addr>().is_ok() {
            1
        } else if trimmed.parse::<std::net::Ipv6Addr>().is_ok() {
            2
        } else if parse_mac(trimmed).is_ok() {
            6
        } else {
            bail!(
                "'{address}' is not a management address: expected an IPv4 address, an IPv6 \
                 address or a MAC address"
            );
        };
        Ok(Self {
            family,
            address: trimmed.to_string(),
            // ifIndex. The alternative (1, "unknown") tells a neighbour nothing, and every
            // capture of real equipment uses 2 whether or not the index is meaningful.
            interface_numbering_subtype: 2,
            interface_number,
        })
    }

    /// The family as a name, for event data.
    pub fn family_name(&self) -> String {
        ADDRESS_FAMILIES
            .iter()
            .find(|(f, _)| *f == self.family)
            .map(|(_, n)| (*n).to_string())
            .unwrap_or_else(|| format!("iana_family_{}", self.family))
    }

    fn encode_value(&self) -> Result<Vec<u8>> {
        let addr_bytes: Vec<u8> = match self.family {
            1 => self
                .address
                .parse::<std::net::Ipv4Addr>()
                .with_context(|| format!("'{}' is not an IPv4 address", self.address))?
                .octets()
                .to_vec(),
            2 => self
                .address
                .parse::<std::net::Ipv6Addr>()
                .with_context(|| format!("'{}' is not an IPv6 address", self.address))?
                .octets()
                .to_vec(),
            6 => parse_mac(&self.address)?.to_vec(),
            other => bail!("unsupported management address family {other}"),
        };

        let mut value = Vec::with_capacity(addr_bytes.len() + 8);
        // "Management address string length" counts the family octet plus the address.
        value.push((addr_bytes.len() + 1) as u8);
        value.push(self.family);
        value.extend_from_slice(&addr_bytes);
        value.push(self.interface_numbering_subtype);
        value.extend_from_slice(&self.interface_number.to_be_bytes());
        // OID string length. Zero: we have no OID to advertise and inventing one would be a
        // claim about an SNMP MIB that does not exist.
        value.push(0);
        Ok(value)
    }

    fn decode_value(value: &[u8]) -> Result<Self> {
        if value.len() < 9 {
            bail!(
                "management address TLV is {} octets, needs at least 9",
                value.len()
            );
        }
        let addr_string_len = value[0] as usize;
        if addr_string_len == 0 {
            bail!("management address TLV declares a zero-length address string");
        }
        if value.len() < 1 + addr_string_len + 6 {
            bail!(
                "management address TLV declares a {addr_string_len}-octet address string but \
                 carries only {} octets after it",
                value.len() - 1
            );
        }
        let family = value[1];
        let addr = &value[2..1 + addr_string_len];
        let address = match family {
            1 if addr.len() == 4 => {
                std::net::Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).to_string()
            }
            2 if addr.len() == 16 => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(addr);
                std::net::Ipv6Addr::from(octets).to_string()
            }
            6 if addr.len() == 6 => {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(addr);
                format_mac(&mac)
            }
            // An address family we do not model. Rendering it as hex here would put bytes in
            // front of the model, so it is reported by length instead and the caller decides.
            other => bail!(
                "management address family {other} with a {}-octet address is not supported",
                addr.len()
            ),
        };

        let rest = &value[1 + addr_string_len..];
        Ok(Self {
            family,
            address,
            interface_numbering_subtype: rest[0],
            interface_number: u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]),
        })
    }
}

/// A decoded LLDPDU: the three mandatory TLVs plus whichever optional ones were present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lldpdu {
    pub chassis_id_subtype: u8,
    pub chassis_id: String,
    pub port_id_subtype: u8,
    pub port_id: String,
    pub ttl: u16,
    pub port_description: Option<String>,
    pub system_name: Option<String>,
    pub system_description: Option<String>,
    /// (supported, enabled) from the System Capabilities TLV.
    pub capabilities: Option<(u16, u16)>,
    pub management_address: Option<ManagementAddress>,
}

impl Lldpdu {
    /// The mandatory three, with everything optional left out.
    pub fn minimal(
        chassis_id_subtype: u8,
        chassis_id: impl Into<String>,
        port_id_subtype: u8,
        port_id: impl Into<String>,
        ttl: u16,
    ) -> Self {
        Self {
            chassis_id_subtype,
            chassis_id: chassis_id.into(),
            port_id_subtype,
            port_id: port_id.into(),
            ttl,
            port_description: None,
            system_name: None,
            system_description: None,
            capabilities: None,
            management_address: None,
        }
    }

    /// Serialise to LLDPDU octets (no Ethernet header).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(64);

        push_tlv(
            &mut out,
            TLV_CHASSIS_ID,
            &encode_id_value(IdKind::Chassis, self.chassis_id_subtype, &self.chassis_id)?,
        )?;
        push_tlv(
            &mut out,
            TLV_PORT_ID,
            &encode_id_value(IdKind::Port, self.port_id_subtype, &self.port_id)?,
        )?;
        push_tlv(&mut out, TLV_TIME_TO_LIVE, &self.ttl.to_be_bytes())?;

        if let Some(text) = &self.port_description {
            push_text_tlv(&mut out, TLV_PORT_DESCRIPTION, "port_description", text)?;
        }
        if let Some(text) = &self.system_name {
            push_text_tlv(&mut out, TLV_SYSTEM_NAME, "system_name", text)?;
        }
        if let Some(text) = &self.system_description {
            push_text_tlv(&mut out, TLV_SYSTEM_DESCRIPTION, "system_description", text)?;
        }
        if let Some((supported, enabled)) = self.capabilities {
            let mut value = Vec::with_capacity(4);
            value.extend_from_slice(&supported.to_be_bytes());
            value.extend_from_slice(&enabled.to_be_bytes());
            push_tlv(&mut out, TLV_SYSTEM_CAPABILITIES, &value)?;
        }
        if let Some(mgmt) = &self.management_address {
            push_tlv(&mut out, TLV_MANAGEMENT_ADDRESS, &mgmt.encode_value()?)?;
        }

        // End Of LLDPDU: type 0, length 0, i.e. two zero octets.
        push_tlv(&mut out, TLV_END_OF_LLDPDU, &[])?;
        Ok(out)
    }

    /// Parse LLDPDU octets (no Ethernet header).
    ///
    /// Enforces 802.1AB §8.5: the first three TLVs must be Chassis ID, Port ID and TTL, in that
    /// order. Unknown and organisationally-specific TLVs are skipped; a duplicate optional TLV
    /// keeps the first occurrence, as a receiver must.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut chassis: Option<(u8, String)> = None;
        let mut port: Option<(u8, String)> = None;
        let mut ttl: Option<u16> = None;
        let mut port_description = None;
        let mut system_name = None;
        let mut system_description = None;
        let mut capabilities = None;
        let mut management_address = None;

        let mut offset = 0usize;
        let mut index = 0usize;

        while offset < data.len() {
            if offset + 2 > data.len() {
                bail!("truncated TLV header at octet {offset}");
            }
            let header = u16::from_be_bytes([data[offset], data[offset + 1]]);
            let tlv_type = (header >> 9) as u8;
            let tlv_len = (header & 0x01FF) as usize;
            offset += 2;

            if offset + tlv_len > data.len() {
                bail!(
                    "TLV type {tlv_type} declares {tlv_len} octets but only {} remain",
                    data.len() - offset
                );
            }
            let value = &data[offset..offset + tlv_len];
            offset += tlv_len;

            match (index, tlv_type) {
                (0, TLV_CHASSIS_ID) => {
                    chassis = Some(decode_id_value(IdKind::Chassis, value)?);
                }
                (1, TLV_PORT_ID) => {
                    port = Some(decode_id_value(IdKind::Port, value)?);
                }
                (2, TLV_TIME_TO_LIVE) => {
                    if value.len() != 2 {
                        bail!("Time To Live TLV must be 2 octets, got {}", value.len());
                    }
                    ttl = Some(u16::from_be_bytes([value[0], value[1]]));
                }
                (0, other) => {
                    bail!("first TLV must be Chassis ID (type {TLV_CHASSIS_ID}), got type {other}")
                }
                (1, other) => {
                    bail!("second TLV must be Port ID (type {TLV_PORT_ID}), got type {other}")
                }
                (2, other) => bail!(
                    "third TLV must be Time To Live (type {TLV_TIME_TO_LIVE}), got type {other}"
                ),
                (_, TLV_END_OF_LLDPDU) => break,
                (_, TLV_PORT_DESCRIPTION) => {
                    port_description.get_or_insert_with(|| text_of(value));
                }
                (_, TLV_SYSTEM_NAME) => {
                    system_name.get_or_insert_with(|| text_of(value));
                }
                (_, TLV_SYSTEM_DESCRIPTION) => {
                    system_description.get_or_insert_with(|| text_of(value));
                }
                (_, TLV_SYSTEM_CAPABILITIES) => {
                    if value.len() != 4 {
                        bail!(
                            "System Capabilities TLV must be 4 octets, got {}",
                            value.len()
                        );
                    }
                    capabilities.get_or_insert((
                        u16::from_be_bytes([value[0], value[1]]),
                        u16::from_be_bytes([value[2], value[3]]),
                    ));
                }
                (_, TLV_MANAGEMENT_ADDRESS) => {
                    // A management address we cannot render is not fatal to the whole PDU: the
                    // mandatory TLVs are still intact and the neighbour is still worth
                    // reporting. Drop the address, keep the advertisement.
                    if management_address.is_none() {
                        management_address = ManagementAddress::decode_value(value).ok();
                    }
                }
                // Organisationally specific (127) and anything else the spec adds later.
                (_, _) => {}
            }

            index += 1;
        }

        let (chassis_id_subtype, chassis_id) =
            chassis.ok_or_else(|| anyhow!("LLDPDU has no Chassis ID TLV"))?;
        let (port_id_subtype, port_id) =
            port.ok_or_else(|| anyhow!("LLDPDU has no Port ID TLV"))?;
        let ttl = ttl.ok_or_else(|| anyhow!("LLDPDU has no Time To Live TLV"))?;

        Ok(Self {
            chassis_id_subtype,
            chassis_id,
            port_id_subtype,
            port_id,
            ttl,
            port_description,
            system_name,
            system_description,
            capabilities,
            management_address,
        })
    }

    /// Everything the model is shown about a neighbour: names and natural notation, no octets.
    ///
    /// Optional fields the neighbour did not send are omitted rather than sent as `null`, so an
    /// event handler can test presence with a plain `in` / `get`.
    pub fn to_event_data(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("chassis_id".into(), json!(self.chassis_id));
        map.insert(
            "chassis_id_subtype".into(),
            json!(subtype_name(IdKind::Chassis, self.chassis_id_subtype)),
        );
        map.insert(
            "chassis_id_subtype_code".into(),
            json!(self.chassis_id_subtype),
        );
        map.insert("port_id".into(), json!(self.port_id));
        map.insert(
            "port_id_subtype".into(),
            json!(subtype_name(IdKind::Port, self.port_id_subtype)),
        );
        map.insert("port_id_subtype_code".into(), json!(self.port_id_subtype));
        map.insert("ttl".into(), json!(self.ttl));

        if let Some(v) = &self.port_description {
            map.insert("port_description".into(), json!(v));
        }
        if let Some(v) = &self.system_name {
            map.insert("system_name".into(), json!(v));
        }
        if let Some(v) = &self.system_description {
            map.insert("system_description".into(), json!(v));
        }
        if let Some((supported, enabled)) = self.capabilities {
            map.insert("capabilities".into(), json!(capability_names(supported)));
            map.insert(
                "capabilities_enabled".into(),
                json!(capability_names(enabled)),
            );
        }
        if let Some(mgmt) = &self.management_address {
            map.insert("management_address".into(), json!(mgmt.address));
            map.insert(
                "management_address_family".into(),
                json!(mgmt.family_name()),
            );
            map.insert(
                "management_interface_number".into(),
                json!(mgmt.interface_number),
            );
        }
        map
    }
}

/// A full LLDP Ethernet frame, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LldpFrame {
    pub destination_mac: [u8; 6],
    pub source_mac: [u8; 6],
    pub lldpdu: Lldpdu,
}

/// Wrap an LLDPDU in its Ethernet header.
pub fn encode_frame(
    destination_mac: [u8; 6],
    source_mac: [u8; 6],
    lldpdu: &Lldpdu,
) -> Result<Vec<u8>> {
    let body = lldpdu.encode()?;
    let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + body.len());
    frame.extend_from_slice(&destination_mac);
    frame.extend_from_slice(&source_mac);
    frame.extend_from_slice(&LLDP_ETHERTYPE.to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Parse an Ethernet frame carrying an LLDPDU.
///
/// The EtherType is checked: the BPF filter already restricts capture to 0x88CC, but the UDP
/// test transport carries whole frames from an arbitrary sender and must not trust them.
pub fn decode_frame(frame: &[u8]) -> Result<LldpFrame> {
    if frame.len() < ETHERNET_HEADER_LEN {
        bail!(
            "frame is {} octets, shorter than an Ethernet header",
            frame.len()
        );
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    if ethertype != LLDP_ETHERTYPE {
        bail!("EtherType is 0x{ethertype:04x}, not LLDP's 0x88CC");
    }
    let mut destination_mac = [0u8; 6];
    destination_mac.copy_from_slice(&frame[0..6]);
    let mut source_mac = [0u8; 6];
    source_mac.copy_from_slice(&frame[6..12]);

    Ok(LldpFrame {
        destination_mac,
        source_mac,
        lldpdu: Lldpdu::decode(&frame[ETHERNET_HEADER_LEN..])?,
    })
}

// ---------------------------------------------------------------------------------------------
// Action <-> LLDPDU
// ---------------------------------------------------------------------------------------------

/// What a `send_lldp_advertisement` action asks for, once validated.
///
/// `source_mac` is optional because the server knows its own interface address and the model
/// usually should not have to; `destination_mac` defaults to the nearest-bridge group address,
/// which is where an LLDP agent transmits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisementRequest {
    pub lldpdu: Lldpdu,
    pub source_mac: Option<[u8; 6]>,
    pub destination_mac: [u8; 6],
}

impl AdvertisementRequest {
    /// Validate a `send_lldp_advertisement` action into a frame-ready request.
    ///
    /// This is the *only* place action JSON is interpreted, and it is deliberately strict: an
    /// advertisement is a positive assertion about a device on the link, so a field the model
    /// got wrong must fail here rather than reach a neighbour's topology table half-formed.
    pub fn from_action(action: &Value) -> Result<Self> {
        let chassis_id = required_str(action, "chassis_id")?;
        let port_id = required_str(action, "port_id")?;

        let chassis_id_subtype = named_subtype(
            IdKind::Chassis,
            action.get("chassis_id_subtype").and_then(Value::as_str),
            &chassis_id,
        )?;
        let port_id_subtype = named_subtype(
            IdKind::Port,
            action.get("port_id_subtype").and_then(Value::as_str),
            &port_id,
        )?;

        let ttl = match action.get("ttl") {
            None | Some(Value::Null) => 120u16,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| anyhow!("'ttl' must be a whole number of seconds, got {v}"))?;
                u16::try_from(n).map_err(|_| anyhow!("'ttl' must be 0-65535 seconds, got {n}"))?
            }
        };

        let supported = string_array(action, "capabilities")?;
        let enabled = string_array(action, "capabilities_enabled")?;
        let capabilities = match (supported, enabled) {
            (None, None) => None,
            (Some(s), None) => {
                let bits = capability_bits(&s)?;
                // Nothing was said about which are switched on. Claiming the full supported
                // set is what real equipment advertises, and the alternative (enabled = 0)
                // would describe a device that supports being a bridge and is not one.
                Some((bits, bits))
            }
            (None, Some(e)) => {
                let bits = capability_bits(&e)?;
                Some((bits, bits))
            }
            (Some(s), Some(e)) => Some((capability_bits(&s)?, capability_bits(&e)?)),
        };

        let management_address = match action.get("management_address") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let addr = v
                    .as_str()
                    .ok_or_else(|| anyhow!("'management_address' must be a string, got {v}"))?;
                let iface = match action.get("management_interface_number") {
                    None | Some(Value::Null) => 0u32,
                    Some(n) => u32::try_from(n.as_u64().ok_or_else(|| {
                        anyhow!("'management_interface_number' must be a whole number, got {n}")
                    })?)
                    .map_err(|_| anyhow!("'management_interface_number' must fit in 32 bits"))?,
                };
                Some(ManagementAddress::new(addr, iface)?)
            }
        };

        let lldpdu = Lldpdu {
            chassis_id_subtype,
            chassis_id,
            port_id_subtype,
            port_id,
            ttl,
            port_description: optional_str(action, "port_description")?,
            system_name: optional_str(action, "system_name")?,
            system_description: optional_str(action, "system_description")?,
            capabilities,
            management_address,
        };

        let source_mac = match action.get("source_mac") {
            None | Some(Value::Null) => None,
            Some(v) => Some(parse_mac(v.as_str().ok_or_else(|| {
                anyhow!("'source_mac' must be a MAC address string, got {v}")
            })?)?),
        };
        let destination_mac = match action.get("destination_mac") {
            None | Some(Value::Null) => LLDP_MULTICAST_MAC,
            Some(v) => parse_mac(v.as_str().ok_or_else(|| {
                anyhow!("'destination_mac' must be a MAC address string, got {v}")
            })?)?,
        };

        // Encoding here is what makes this a validation rather than a hope: a value too long for
        // its TLV, or an address that does not match its subtype, fails now.
        lldpdu.encode()?;

        Ok(Self {
            lldpdu,
            source_mac,
            destination_mac,
        })
    }

    /// Produce the frame, using `fallback_source` when the action named no source MAC.
    pub fn to_frame(&self, fallback_source: [u8; 6]) -> Result<Vec<u8>> {
        encode_frame(
            self.destination_mac,
            self.source_mac.unwrap_or(fallback_source),
            &self.lldpdu,
        )
    }
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

fn push_tlv(out: &mut Vec<u8>, tlv_type: u8, value: &[u8]) -> Result<()> {
    if value.len() > MAX_TLV_VALUE {
        bail!(
            "TLV type {tlv_type} value is {} octets; the 9-bit length field caps it at \
             {MAX_TLV_VALUE}",
            value.len()
        );
    }
    let header = ((tlv_type as u16) << 9) | value.len() as u16;
    out.extend_from_slice(&header.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

fn push_text_tlv(out: &mut Vec<u8>, tlv_type: u8, field: &str, text: &str) -> Result<()> {
    let bytes = text.as_bytes();
    if bytes.len() > MAX_STRING_TLV_VALUE {
        bail!(
            "'{field}' is {} octets; IEEE 802.1AB caps it at {MAX_STRING_TLV_VALUE}",
            bytes.len()
        );
    }
    push_tlv(out, tlv_type, bytes)
}

/// Encode a Chassis ID or Port ID value: the leading subtype octet, then the identifier in
/// whatever representation that subtype selects.
fn encode_id_value(kind: IdKind, subtype: u8, value: &str) -> Result<Vec<u8>> {
    if !kind.table().iter().any(|(c, _)| *c == subtype) {
        bail!(
            "{} subtype {subtype} is not defined by IEEE 802.1AB. Valid subtypes: {}",
            kind.label(),
            kind.table()
                .iter()
                .map(|(c, n)| format!("{n} ({c})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mut out = vec![subtype];
    if subtype == kind.mac_subtype() {
        out.extend_from_slice(&parse_mac(value).with_context(|| {
            format!("{} subtype 'mac_address' needs a MAC address", kind.label())
        })?);
    } else if subtype == kind.network_address_subtype() {
        // 802.1AB §8.5.2.3: an IANA address family octet, then the address.
        if let Ok(v4) = value.trim().parse::<std::net::Ipv4Addr>() {
            out.push(1);
            out.extend_from_slice(&v4.octets());
        } else if let Ok(v6) = value.trim().parse::<std::net::Ipv6Addr>() {
            out.push(2);
            out.extend_from_slice(&v6.octets());
        } else {
            bail!(
                "{} subtype 'network_address' needs an IP address, got '{value}'",
                kind.label()
            );
        }
    } else {
        if value.is_empty() {
            bail!("{} must not be empty", kind.label());
        }
        out.extend_from_slice(value.as_bytes());
    }

    if out.len() > MAX_STRING_TLV_VALUE {
        bail!(
            "{} is {} octets including its subtype; IEEE 802.1AB caps it at \
             {MAX_STRING_TLV_VALUE}",
            kind.label(),
            out.len()
        );
    }
    Ok(out)
}

/// The inverse of [`encode_id_value`].
fn decode_id_value(kind: IdKind, value: &[u8]) -> Result<(u8, String)> {
    if value.is_empty() {
        bail!("{} TLV carries no subtype octet", kind.label());
    }
    let subtype = value[0];
    let body = &value[1..];
    if body.is_empty() {
        bail!("{} TLV carries a subtype and no identifier", kind.label());
    }

    let text = if subtype == kind.mac_subtype() {
        if body.len() != 6 {
            bail!(
                "{} subtype 'mac_address' must carry 6 octets, got {}",
                kind.label(),
                body.len()
            );
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(body);
        format_mac(&mac)
    } else if subtype == kind.network_address_subtype() {
        match (body[0], body.len()) {
            (1, 5) => std::net::Ipv4Addr::new(body[1], body[2], body[3], body[4]).to_string(),
            (2, 17) => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&body[1..]);
                std::net::Ipv6Addr::from(octets).to_string()
            }
            (family, len) => bail!(
                "{} subtype 'network_address' has IANA family {family} with {} address octets, \
                 which is not IPv4 or IPv6",
                kind.label(),
                len - 1
            ),
        }
    } else {
        text_of(body)
    };

    Ok((subtype, text))
}

/// TLV text is "not null terminated" ASCII/UTF-8 per 802.1AB. Lossy is right here: a neighbour
/// that sends invalid UTF-8 should still be reported, not dropped.
fn text_of(value: &[u8]) -> String {
    String::from_utf8_lossy(value)
        .trim_end_matches('\0')
        .to_string()
}

fn required_str(action: &Value, key: &str) -> Result<String> {
    match action.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => bail!("'{key}' must not be empty"),
        Some(other) => bail!("'{key}' must be a string, got {other}"),
        None => bail!("'{key}' is required"),
    }
}

fn optional_str(action: &Value, key: &str) -> Result<Option<String>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => bail!("'{key}' must be a string, got {other}"),
    }
}

fn string_array(action: &Value, key: &str) -> Result<Option<Vec<String>>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    other => bail!("'{key}' must be an array of strings, found {other}"),
                }
            }
            Ok(Some(out))
        }
        Some(other) => bail!("'{key}' must be an array of strings, got {other}"),
    }
}

/// Resolve a subtype name, or infer one from the value's shape when none was given.
///
/// Inference exists because the subtype is the field a model is most likely to omit, and the
/// wrong default is worse than a guess from the value: sending a MAC address under the
/// `interface_name` subtype produces a frame whose chassis ID reads as the literal text
/// `"00:1b:21:..."` in a neighbour's table.
fn named_subtype(kind: IdKind, name: Option<&str>, value: &str) -> Result<u8> {
    match name {
        Some(name) => subtype_code(kind, name).ok_or_else(|| {
            anyhow!(
                "'{}_subtype' value '{name}' is not an IEEE 802.1AB subtype. Valid names: {}",
                kind.label(),
                kind.table()
                    .iter()
                    .map(|(_, n)| *n)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        None if parse_mac(value).is_ok() => Ok(kind.mac_subtype()),
        None if value.trim().parse::<std::net::IpAddr>().is_ok() => {
            Ok(kind.network_address_subtype())
        }
        // "locally assigned" is the honest default for free text: it claims nothing about what
        // the string means, which is exactly the situation when nobody said.
        None => Ok(7),
    }
}
