//! Pure BPDU codec — IEEE 802.1D-2004 (STP) and 802.1w (RSTP).
//!
//! Nothing in this file touches a socket, a capture handle, the LLM or `AppState`. It is a
//! total function from structured values to bytes and back, which is the only part of this
//! protocol that can be proved correct in this environment: the raw 802.3 transport in
//! `mod.rs` needs `CAP_NET_RAW`/`/dev/bpf*` and has never been executed. See
//! `src/server/stp/CLAUDE.md` for exactly what is proved and what is not.
//!
//! # Wire layout
//!
//! A BPDU is carried in an **802.3 length-encapsulated** frame with an 802.2 LLC header,
//! *not* in an EtherType-II frame:
//!
//! ```text
//!  0.. 6  destination MAC   01:80:C2:00:00:00 (the Bridge Group Address)
//!  6..12  source MAC        the transmitting bridge port
//! 12..14  length            octets of MAC client data: 3 (LLC) + BPDU length
//! 14..17  LLC               DSAP 0x42, SSAP 0x42, control 0x03 (unnumbered information)
//! 17..    BPDU              35 bytes (config), 36 (RST), or 4 (TCN)
//! ```
//!
//! Configuration BPDU body (802.1D-2004 §9.3.1), offsets relative to the start of the BPDU:
//!
//! ```text
//!  0.. 2  protocol identifier   always 0x0000
//!  2      protocol version      0 = STP, 2 = RSTP
//!  3      BPDU type             0x00 config, 0x02 RST, 0x80 TCN
//!  4      flags                 see BpduFlags
//!  5..13  root identifier       priority(4b) | system ID extension(12b) | MAC(6)
//! 13..17  root path cost
//! 17..25  bridge identifier     same packing as the root identifier
//! 25..27  port identifier       priority(4b) | port number(12b)
//! 27..29  message age           1/256 s
//! 29..31  max age               1/256 s
//! 31..33  hello time            1/256 s
//! 33..35  forward delay         1/256 s
//! 35      version 1 length      RST BPDUs only, always 0x00
//! ```
//!
//! # Two things this file exists to get right
//!
//! **Timers are in 1/256-second units, not seconds.** The default values every capture shows
//! are max age 20 s = `0x1400`, hello time 2 s = `0x0200`, forward delay 15 s = `0x0F00`. An
//! implementation that writes seconds directly produces a BPDU claiming a 20/256-second max
//! age, which a real bridge will act on. `tests/server/stp/codec_test.rs` asserts these byte
//! pairs against literals.
//!
//! **The 16-bit priority field is two fields.** Since 802.1t / 802.1Q the high 4 bits are the
//! bridge priority and the low 12 bits are the *system ID extension* (in practice the VLAN
//! id). So priority is only expressible in steps of 4096, and `0x8001` is priority 32768 on
//! VLAN 1 — not "priority 32769". Both halves are surfaced separately by [`BridgeId`] so the
//! model never has to pack them itself.

use anyhow::{anyhow, bail, Result};

/// The Bridge Group Address every BPDU is sent to (802.1D-2004 Table 7-9).
pub const STP_MULTICAST_MAC: [u8; 6] = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x00];

/// 802.2 LLC header for the Spanning Tree Protocol: DSAP/SSAP 0x42, UI control 0x03.
pub const LLC_DSAP: u8 = 0x42;
pub const LLC_SSAP: u8 = 0x42;
pub const LLC_CONTROL: u8 = 0x03;
pub const LLC_HEADER_LEN: usize = 3;

/// 802.3 MAC header: destination(6) + source(6) + length(2).
pub const ETHERNET_HEADER_LEN: usize = 14;

/// Minimum 802.3 frame excluding the FCS. Shorter frames are padded on transmit.
pub const MIN_ETHERNET_FRAME_LEN: usize = 60;

pub const PROTOCOL_ID: u16 = 0x0000;

pub const VERSION_STP: u8 = 0;
pub const VERSION_RSTP: u8 = 2;

pub const BPDU_TYPE_CONFIG: u8 = 0x00;
pub const BPDU_TYPE_RST: u8 = 0x02;
pub const BPDU_TYPE_TCN: u8 = 0x80;

pub const CONFIG_BPDU_LEN: usize = 35;
pub const RST_BPDU_LEN: usize = 36;
pub const TCN_BPDU_LEN: usize = 4;

/// Timer fields are expressed in units of 1/256 second.
pub const TIMER_TICKS_PER_SECOND: f64 = 256.0;

/// Bridge priority occupies the top 4 bits of a 16-bit field, so it moves in steps of 4096.
pub const BRIDGE_PRIORITY_STEP: u16 = 4096;
/// The largest expressible bridge priority (15 * 4096).
pub const BRIDGE_PRIORITY_MAX: u16 = 61440;
/// The largest expressible system ID extension (12 bits).
pub const SYSTEM_ID_EXTENSION_MAX: u16 = 4095;

/// Port priority occupies the top 4 bits of the 16-bit port identifier.
pub const PORT_PRIORITY_STEP: u8 = 16;
/// The largest expressible port priority (15 * 16).
pub const PORT_PRIORITY_MAX: u8 = 240;
/// The largest expressible port number (12 bits).
pub const PORT_NUMBER_MAX: u16 = 4095;

// ---------------------------------------------------------------------------
// MAC helpers
// ---------------------------------------------------------------------------

/// Parse `aa:bb:cc:dd:ee:ff` (or the `-` separated form) into six octets.
pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("MAC address must have 6 octets separated by ':' or '-', got '{s}'");
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(part, 16)
            .map_err(|_| anyhow!("invalid hex octet '{part}' in MAC address '{s}'"))?;
    }
    Ok(out)
}

/// Render six octets as `aa:bb:cc:dd:ee:ff`.
pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

// ---------------------------------------------------------------------------
// Timers
// ---------------------------------------------------------------------------

/// Seconds → the 1/256-second unit the wire uses.
///
/// This is the conversion an implementation gets wrong by omitting it, so it lives in one
/// place and is asserted against literal bytes.
pub fn seconds_to_ticks(seconds: f64) -> Result<u16> {
    if !seconds.is_finite() || seconds < 0.0 {
        bail!("timer value must be a finite, non-negative number of seconds, got {seconds}");
    }
    let ticks = (seconds * TIMER_TICKS_PER_SECOND).round();
    if ticks > u16::MAX as f64 {
        bail!(
            "timer value {seconds}s does not fit the 16-bit 1/256s field (maximum {:.4}s)",
            u16::MAX as f64 / TIMER_TICKS_PER_SECOND
        );
    }
    Ok(ticks as u16)
}

/// The 1/256-second unit the wire uses → seconds.
pub fn ticks_to_seconds(ticks: u16) -> f64 {
    ticks as f64 / TIMER_TICKS_PER_SECOND
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// RSTP port role, flags bits 2-3 (802.1w §9.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortRole {
    /// 0b00 — sent by an 802.1D bridge, or a role not yet assigned.
    #[default]
    Unknown,
    /// 0b01 — alternate or backup.
    Alternate,
    /// 0b10 — root port.
    Root,
    /// 0b11 — designated port. This is the role a bridge claims when it believes it owns the
    /// segment, and the one a rogue root bridge asserts.
    Designated,
}

impl PortRole {
    pub fn as_str(self) -> &'static str {
        match self {
            PortRole::Unknown => "unknown",
            PortRole::Alternate => "alternate",
            PortRole::Root => "root",
            PortRole::Designated => "designated",
        }
    }

    /// Parse the name used in events and actions. `backup` is accepted as a synonym for
    /// `alternate` because they share one encoding.
    pub fn from_name(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "unknown" => Ok(PortRole::Unknown),
            "alternate" | "backup" => Ok(PortRole::Alternate),
            "root" => Ok(PortRole::Root),
            "designated" => Ok(PortRole::Designated),
            other => bail!(
                "unknown port_role '{other}' (expected unknown, alternate, backup, root or designated)"
            ),
        }
    }

    fn to_bits(self) -> u8 {
        match self {
            PortRole::Unknown => 0b00,
            PortRole::Alternate => 0b01,
            PortRole::Root => 0b10,
            PortRole::Designated => 0b11,
        }
    }

    fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b01 => PortRole::Alternate,
            0b10 => PortRole::Root,
            0b11 => PortRole::Designated,
            _ => PortRole::Unknown,
        }
    }
}

/// The BPDU flags octet, as structured booleans plus the port role.
///
/// 802.1D uses only bit 0 (topology change) and bit 7 (topology change acknowledgement); 802.1w
/// gives every other bit a meaning. Decoding an 802.1D BPDU therefore yields
/// `port_role: Unknown` and the four RSTP booleans false, which is exactly what the wire says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BpduFlags {
    /// Bit 0 (0x01) — topology change.
    pub topology_change: bool,
    /// Bit 1 (0x02) — proposal (RSTP).
    pub proposal: bool,
    /// Bits 2-3 (0x0C) — port role (RSTP).
    pub port_role: PortRole,
    /// Bit 4 (0x10) — learning (RSTP).
    pub learning: bool,
    /// Bit 5 (0x20) — forwarding (RSTP).
    pub forwarding: bool,
    /// Bit 6 (0x40) — agreement (RSTP).
    pub agreement: bool,
    /// Bit 7 (0x80) — topology change acknowledgement.
    pub topology_change_ack: bool,
}

impl BpduFlags {
    pub fn to_byte(self) -> u8 {
        let mut byte = 0u8;
        if self.topology_change {
            byte |= 0x01;
        }
        if self.proposal {
            byte |= 0x02;
        }
        byte |= self.port_role.to_bits() << 2;
        if self.learning {
            byte |= 0x10;
        }
        if self.forwarding {
            byte |= 0x20;
        }
        if self.agreement {
            byte |= 0x40;
        }
        if self.topology_change_ack {
            byte |= 0x80;
        }
        byte
    }

    pub fn from_byte(byte: u8) -> Self {
        Self {
            topology_change: byte & 0x01 != 0,
            proposal: byte & 0x02 != 0,
            port_role: PortRole::from_bits(byte >> 2),
            learning: byte & 0x10 != 0,
            forwarding: byte & 0x20 != 0,
            agreement: byte & 0x40 != 0,
            topology_change_ack: byte & 0x80 != 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge and port identifiers
// ---------------------------------------------------------------------------

/// A bridge identifier: 4-bit priority, 12-bit system ID extension, 6-byte MAC.
///
/// The two halves of the leading 16-bit field are kept apart deliberately. A single opaque
/// number is what makes people write `priority: 32769` when they mean "priority 32768 on
/// VLAN 1", and the lower bits are not priority at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeId {
    /// 0..=61440 in steps of 4096. **Lower wins the root election.**
    pub priority: u16,
    /// 0..=4095. The VLAN id in per-VLAN spanning tree; 0 for a single instance.
    pub system_id_extension: u16,
    pub mac: [u8; 6],
}

impl BridgeId {
    /// Construct with validation. Rejects a priority that is not a multiple of 4096, because
    /// there is no way to put one on the wire — the low bits belong to the VLAN.
    pub fn new(priority: u16, system_id_extension: u16, mac: [u8; 6]) -> Result<Self> {
        if priority % BRIDGE_PRIORITY_STEP != 0 {
            bail!(
                "bridge priority must be a multiple of {} (0, 4096, 8192, … {}); got {}. The \
                 low 12 bits of the field are the system ID extension (VLAN), not priority.",
                BRIDGE_PRIORITY_STEP,
                BRIDGE_PRIORITY_MAX,
                priority
            );
        }
        if priority > BRIDGE_PRIORITY_MAX {
            bail!(
                "bridge priority must be at most {}, got {}",
                BRIDGE_PRIORITY_MAX,
                priority
            );
        }
        if system_id_extension > SYSTEM_ID_EXTENSION_MAX {
            bail!(
                "system_id_extension must be at most {} (12 bits), got {}",
                SYSTEM_ID_EXTENSION_MAX,
                system_id_extension
            );
        }
        Ok(Self {
            priority,
            system_id_extension,
            mac,
        })
    }

    pub fn encode(&self) -> [u8; 8] {
        let packed = (self.priority & 0xF000) | (self.system_id_extension & 0x0FFF);
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&packed.to_be_bytes());
        out[2..8].copy_from_slice(&self.mac);
        out
    }

    /// Decode eight octets. Never fails on a well-sized slice: every 16-bit value splits into
    /// a valid priority and a valid extension, so a peer cannot send an "invalid" identifier.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 8 {
            bail!("bridge identifier needs 8 octets, got {}", bytes.len());
        }
        let packed = u16::from_be_bytes([bytes[0], bytes[1]]);
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&bytes[2..8]);
        Ok(Self {
            priority: packed & 0xF000,
            system_id_extension: packed & 0x0FFF,
            mac,
        })
    }

    pub fn mac_string(&self) -> String {
        format_mac(&self.mac)
    }
}

/// A port identifier: 4-bit priority, 12-bit port number.
///
/// 802.1D-1998 split this octet-for-octet (8-bit priority, 8-bit port number); 802.1t
/// re-split it 4/12 so more than 255 ports are addressable. The two agree byte-for-byte for
/// the common values — the canonical `0x8001` is priority 128, port 1 under either reading —
/// and this codec implements the 802.1t/802.1D-2004 split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortId {
    /// 0..=240 in steps of 16.
    pub priority: u8,
    /// 0..=4095.
    pub number: u16,
}

impl PortId {
    pub fn new(priority: u8, number: u16) -> Result<Self> {
        if priority % PORT_PRIORITY_STEP != 0 {
            bail!(
                "port priority must be a multiple of {} (0, 16, 32, … {}); got {}. The low 12 \
                 bits of the field are the port number, not priority.",
                PORT_PRIORITY_STEP,
                PORT_PRIORITY_MAX,
                priority
            );
        }
        if number > PORT_NUMBER_MAX {
            bail!(
                "port number must be at most {} (12 bits), got {}",
                PORT_NUMBER_MAX,
                number
            );
        }
        Ok(Self { priority, number })
    }

    pub fn encode(&self) -> [u8; 2] {
        let packed = (((self.priority & 0xF0) as u16) << 8) | (self.number & 0x0FFF);
        packed.to_be_bytes()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 2 {
            bail!("port identifier needs 2 octets, got {}", bytes.len());
        }
        let packed = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Self {
            priority: ((packed >> 8) as u8) & 0xF0,
            number: packed & 0x0FFF,
        })
    }
}

// ---------------------------------------------------------------------------
// BPDUs
// ---------------------------------------------------------------------------

/// A Configuration BPDU (802.1D type 0x00) or a Rapid Spanning Tree BPDU (802.1w type 0x02).
///
/// The two differ only in the version/type octets and a trailing `version 1 length` octet, so
/// they share one struct; [`ConfigBpdu::is_rstp`] reports which is on the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigBpdu {
    /// 0 = STP, 2 = RSTP. Values above 2 are accepted on decode (MSTP sends 3) and reported
    /// verbatim rather than being normalised away.
    pub version: u8,
    /// 0x00 (config) or 0x02 (RST).
    pub bpdu_type: u8,
    pub flags: BpduFlags,
    pub root: BridgeId,
    pub root_path_cost: u32,
    pub bridge: BridgeId,
    pub port: PortId,
    pub message_age_seconds: f64,
    pub max_age_seconds: f64,
    pub hello_time_seconds: f64,
    pub forward_delay_seconds: f64,
}

impl ConfigBpdu {
    /// True when this is a Rapid Spanning Tree BPDU rather than an 802.1D configuration BPDU.
    pub fn is_rstp(&self) -> bool {
        self.bpdu_type == BPDU_TYPE_RST
    }

    /// Serialise the BPDU body — 35 octets for a configuration BPDU, 36 for an RST BPDU.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.bpdu_type != BPDU_TYPE_CONFIG && self.bpdu_type != BPDU_TYPE_RST {
            bail!(
                "BPDU type must be 0x00 (configuration) or 0x02 (RST) here; 0x{:02x} is not a \
                 configuration-shaped BPDU",
                self.bpdu_type
            );
        }
        // Re-validate the identifiers: they are public fields, so a caller can build a
        // ConfigBpdu without going through the checked constructors.
        let root = BridgeId::new(
            self.root.priority,
            self.root.system_id_extension,
            self.root.mac,
        )?;
        let bridge = BridgeId::new(
            self.bridge.priority,
            self.bridge.system_id_extension,
            self.bridge.mac,
        )?;
        let port = PortId::new(self.port.priority, self.port.number)?;

        let mut out = Vec::with_capacity(RST_BPDU_LEN);
        out.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
        out.push(self.version);
        out.push(self.bpdu_type);
        out.push(self.flags.to_byte());
        out.extend_from_slice(&root.encode());
        out.extend_from_slice(&self.root_path_cost.to_be_bytes());
        out.extend_from_slice(&bridge.encode());
        out.extend_from_slice(&port.encode());
        out.extend_from_slice(&seconds_to_ticks(self.message_age_seconds)?.to_be_bytes());
        out.extend_from_slice(&seconds_to_ticks(self.max_age_seconds)?.to_be_bytes());
        out.extend_from_slice(&seconds_to_ticks(self.hello_time_seconds)?.to_be_bytes());
        out.extend_from_slice(&seconds_to_ticks(self.forward_delay_seconds)?.to_be_bytes());
        debug_assert_eq!(out.len(), CONFIG_BPDU_LEN);

        if self.bpdu_type == BPDU_TYPE_RST {
            // 802.1w §9.3.3: "Version 1 Length", always zero, present only on RST BPDUs.
            out.push(0x00);
        }
        Ok(out)
    }

    /// Parse a configuration or RST BPDU body.
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.len() < CONFIG_BPDU_LEN {
            bail!(
                "configuration BPDU needs at least {} octets, got {}",
                CONFIG_BPDU_LEN,
                body.len()
            );
        }
        let protocol_id = u16::from_be_bytes([body[0], body[1]]);
        if protocol_id != PROTOCOL_ID {
            bail!("protocol identifier must be 0x0000, got 0x{protocol_id:04x}");
        }
        Ok(Self {
            version: body[2],
            bpdu_type: body[3],
            flags: BpduFlags::from_byte(body[4]),
            root: BridgeId::decode(&body[5..13])?,
            root_path_cost: u32::from_be_bytes([body[13], body[14], body[15], body[16]]),
            bridge: BridgeId::decode(&body[17..25])?,
            port: PortId::decode(&body[25..27])?,
            message_age_seconds: ticks_to_seconds(u16::from_be_bytes([body[27], body[28]])),
            max_age_seconds: ticks_to_seconds(u16::from_be_bytes([body[29], body[30]])),
            hello_time_seconds: ticks_to_seconds(u16::from_be_bytes([body[31], body[32]])),
            forward_delay_seconds: ticks_to_seconds(u16::from_be_bytes([body[33], body[34]])),
        })
    }
}

/// Any BPDU this codec understands.
#[derive(Debug, Clone, PartialEq)]
pub enum Bpdu {
    /// Configuration (0x00) or RST (0x02).
    Config(ConfigBpdu),
    /// Topology Change Notification (0x80). Carries no fields at all — the four octets are
    /// protocol id, version and type.
    TopologyChangeNotification,
}

impl Bpdu {
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.len() < TCN_BPDU_LEN {
            bail!(
                "BPDU needs at least {} octets, got {}",
                TCN_BPDU_LEN,
                body.len()
            );
        }
        let protocol_id = u16::from_be_bytes([body[0], body[1]]);
        if protocol_id != PROTOCOL_ID {
            bail!("protocol identifier must be 0x0000, got 0x{protocol_id:04x}");
        }
        match body[3] {
            BPDU_TYPE_TCN => Ok(Bpdu::TopologyChangeNotification),
            BPDU_TYPE_CONFIG | BPDU_TYPE_RST => Ok(Bpdu::Config(ConfigBpdu::decode(body)?)),
            other => bail!(
                "unknown BPDU type 0x{other:02x} (expected 0x00 configuration, 0x02 RST or \
                 0x80 topology change notification)"
            ),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Bpdu::Config(config) => config.encode(),
            Bpdu::TopologyChangeNotification => Ok(encode_tcn_bpdu()),
        }
    }
}

/// The complete four-octet Topology Change Notification BPDU.
pub fn encode_tcn_bpdu() -> Vec<u8> {
    vec![0x00, 0x00, VERSION_STP, BPDU_TYPE_TCN]
}

// ---------------------------------------------------------------------------
// 802.3 framing
// ---------------------------------------------------------------------------

/// A decoded 802.3 + LLC frame with the BPDU body extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame8023 {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    /// The BPDU body, with the LLC header removed and any Ethernet padding trimmed off using
    /// the 802.3 length field.
    pub payload: Vec<u8>,
}

/// Wrap a BPDU body in an 802.3 + LLC frame, padded to the 60-octet minimum.
///
/// The length field carries the MAC **client data** length — LLC header plus BPDU — and
/// deliberately does *not* include the padding. A configuration BPDU therefore always shows
/// `00 26` (38) and an RST BPDU `00 27` (39).
pub fn encode_frame(destination: [u8; 6], source: [u8; 6], bpdu: &[u8]) -> Vec<u8> {
    let client_len = LLC_HEADER_LEN + bpdu.len();
    let mut frame =
        Vec::with_capacity(MIN_ETHERNET_FRAME_LEN.max(ETHERNET_HEADER_LEN + client_len));
    frame.extend_from_slice(&destination);
    frame.extend_from_slice(&source);
    frame.extend_from_slice(&(client_len as u16).to_be_bytes());
    frame.push(LLC_DSAP);
    frame.push(LLC_SSAP);
    frame.push(LLC_CONTROL);
    frame.extend_from_slice(bpdu);
    while frame.len() < MIN_ETHERNET_FRAME_LEN {
        frame.push(0x00);
    }
    frame
}

/// Parse an 802.3 + LLC frame and hand back the BPDU body.
///
/// Rejects anything whose LLC header is not the STP one, which is what distinguishes a BPDU
/// from every other 802.3 frame on the segment.
pub fn decode_frame(frame: &[u8]) -> Result<Frame8023> {
    if frame.len() < ETHERNET_HEADER_LEN + LLC_HEADER_LEN {
        bail!(
            "802.3 frame needs at least {} octets to carry an LLC header, got {}",
            ETHERNET_HEADER_LEN + LLC_HEADER_LEN,
            frame.len()
        );
    }
    let mut destination = [0u8; 6];
    destination.copy_from_slice(&frame[0..6]);
    let mut source = [0u8; 6];
    source.copy_from_slice(&frame[6..12]);

    let declared_len = u16::from_be_bytes([frame[12], frame[13]]) as usize;
    if declared_len > 1500 {
        bail!(
            "not an 802.3 length-encapsulated frame: field at offset 12 is 0x{declared_len:04x}, \
             which is an EtherType, not a length. BPDUs are never carried in Ethernet II frames."
        );
    }

    let (dsap, ssap, control) = (frame[14], frame[15], frame[16]);
    if dsap != LLC_DSAP || ssap != LLC_SSAP || control != LLC_CONTROL {
        bail!(
            "not a BPDU: LLC header is {:02x}/{:02x}/{:02x}, expected {:02x}/{:02x}/{:02x}",
            dsap,
            ssap,
            control,
            LLC_DSAP,
            LLC_SSAP,
            LLC_CONTROL
        );
    }

    // Trust the length field over the frame length, so trailing padding is not handed on as
    // BPDU content — but never read past what actually arrived.
    let available = frame.len() - ETHERNET_HEADER_LEN - LLC_HEADER_LEN;
    let bpdu_len = declared_len.saturating_sub(LLC_HEADER_LEN).min(available);
    let start = ETHERNET_HEADER_LEN + LLC_HEADER_LEN;
    Ok(Frame8023 {
        destination,
        source,
        payload: frame[start..start + bpdu_len].to_vec(),
    })
}
