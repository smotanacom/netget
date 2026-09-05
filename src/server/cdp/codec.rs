//! Pure CDP frame codec — 802.3 + LLC/SNAP framing, the CDP header, and the TLV body.
//!
//! **Nothing in this file touches a socket, a capture handle, the LLM or `AppState`.** Every
//! function here is a pure transformation between bytes and structured values, which is the
//! whole point: the raw-Ethernet transport in [`super`] needs packet-capture privilege and
//! cannot run in the test environment, so the part of CDP that can actually be *wrong* — the
//! wire format — is isolated where it can be tested exhaustively against literal specification
//! bytes and against real captures. See `src/server/cdp/CLAUDE.md` for what is proven and what
//! is not.
//!
//! # Frame layout
//!
//! ```text
//! destination MAC (6)  01:00:0C:CC:CC:CC
//! source MAC      (6)
//! 802.3 length    (2)  = 8 + len(CDP payload)   -- a LENGTH, not an EtherType
//! LLC  DSAP       (1)  0xAA
//! LLC  SSAP       (1)  0xAA
//! LLC  control    (1)  0x03  (unnumbered information)
//! SNAP OUI        (3)  00:00:0C  (Cisco)
//! SNAP protocol   (2)  0x2000    (CDP)
//! CDP  version    (1)  1 or 2
//! CDP  TTL        (1)  seconds the neighbour should keep the entry
//! CDP  checksum   (2)
//! CDP  TLVs       (n)  type(2) length(2) value(length-4)
//! ```
//!
//! Note the TLV `length` **includes its own 4-byte header**, which is the single most common
//! mistake when hand-writing CDP: a Device ID of `"myswitch"` (8 bytes) is `00 01 00 0C`, not
//! `00 01 00 08`.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::net::IpAddr;

// ---------------------------------------------------------------------------------------------
// Framing constants
// ---------------------------------------------------------------------------------------------

/// The CDP multicast destination. Every CDP advertisement goes here; there is no unicast CDP.
pub const CDP_MULTICAST_MAC: [u8; 6] = [0x01, 0x00, 0x0c, 0xcc, 0xcc, 0xcc];

/// LLC DSAP/SSAP for SNAP-encapsulated traffic.
pub const LLC_SAP_SNAP: u8 = 0xAA;
/// LLC control byte: unnumbered information.
pub const LLC_CONTROL_UI: u8 = 0x03;
/// SNAP OUI for Cisco.
pub const SNAP_OUI_CISCO: [u8; 3] = [0x00, 0x00, 0x0c];
/// SNAP protocol id for CDP.
pub const SNAP_PROTOCOL_CDP: u16 = 0x2000;

/// The complete LLC + SNAP header that precedes every CDP payload.
pub const LLC_SNAP_HEADER: [u8; 8] = [
    LLC_SAP_SNAP,
    LLC_SAP_SNAP,
    LLC_CONTROL_UI,
    SNAP_OUI_CISCO[0],
    SNAP_OUI_CISCO[1],
    SNAP_OUI_CISCO[2],
    (SNAP_PROTOCOL_CDP >> 8) as u8,
    (SNAP_PROTOCOL_CDP & 0xff) as u8,
];

/// Ethernet header length: destination + source + length field.
pub const ETHERNET_HEADER_LEN: usize = 14;
/// LLC + SNAP header length.
pub const LLC_SNAP_HEADER_LEN: usize = 8;
/// Offset of the CDP payload inside a complete frame.
pub const CDP_PAYLOAD_OFFSET: usize = ETHERNET_HEADER_LEN + LLC_SNAP_HEADER_LEN;
/// version + ttl + checksum.
pub const CDP_HEADER_LEN: usize = 4;

// ---------------------------------------------------------------------------------------------
// TLV type codes
// ---------------------------------------------------------------------------------------------

pub const TLV_DEVICE_ID: u16 = 0x0001;
pub const TLV_ADDRESSES: u16 = 0x0002;
pub const TLV_PORT_ID: u16 = 0x0003;
pub const TLV_CAPABILITIES: u16 = 0x0004;
pub const TLV_SOFTWARE_VERSION: u16 = 0x0005;
pub const TLV_PLATFORM: u16 = 0x0006;
pub const TLV_NATIVE_VLAN: u16 = 0x000A;
pub const TLV_DUPLEX: u16 = 0x000B;
pub const TLV_MANAGEMENT_ADDRESS: u16 = 0x0016;

/// Names for the TLV types this codec parses but does not model, so a received advertisement
/// that carries them can still be described to the model as *what it is* rather than as an
/// opaque number. Only the type and the length are ever surfaced — never the bytes.
const KNOWN_OTHER_TLVS: &[(u16, &str)] = &[
    (0x0007, "ip_prefix"),
    (0x0008, "protocol_hello"),
    (0x0009, "vtp_management_domain"),
    (0x000C, "trust_bitmap_legacy"),
    (0x000E, "voip_vlan_reply"),
    (0x000F, "voip_vlan_query"),
    (0x0010, "power_consumption"),
    (0x0011, "mtu"),
    (0x0012, "trust_bitmap"),
    (0x0013, "untrusted_port_cos"),
    (0x0014, "system_name"),
    (0x0015, "system_object_id"),
    (0x0017, "location"),
    (0x0018, "external_port_id"),
    (0x0019, "power_requested"),
    (0x001A, "power_available"),
    (0x001B, "port_unidirectional"),
];

/// Device capability bits, as advertised in the Capabilities TLV.
///
/// Named rather than numeric because the model authors these: `["switch", "igmp"]` is something
/// a language model can produce correctly and `0x28` is not.
pub const CAPABILITY_FLAGS: &[(u32, &str)] = &[
    (0x0001, "router"),
    (0x0002, "transparent_bridge"),
    (0x0004, "source_route_bridge"),
    (0x0008, "switch"),
    (0x0010, "host"),
    (0x0020, "igmp"),
    (0x0040, "repeater"),
    (0x0080, "voip_phone"),
];

/// Decode a capability bitmask into the names this codec knows.
///
/// Bits with no name are reported as `bit_<n>` rather than dropped: a neighbour that sets an
/// undocumented bit is telling us something, and silently discarding it would make the event
/// disagree with the wire.
pub fn capability_names(bits: u32) -> Vec<String> {
    let mut out = Vec::new();
    for (mask, name) in CAPABILITY_FLAGS {
        if bits & mask != 0 {
            out.push((*name).to_string());
        }
    }
    let known: u32 = CAPABILITY_FLAGS.iter().map(|(m, _)| *m).sum();
    let mut leftover = bits & !known;
    let mut bit = 0u32;
    while leftover != 0 {
        if leftover & 1 != 0 {
            out.push(format!("bit_{}", bit));
        }
        leftover >>= 1;
        bit += 1;
    }
    out
}

/// Encode capability names into a bitmask, rejecting anything unrecognised.
///
/// Rejecting is deliberate. A silently-ignored capability name would put an advertisement on
/// the wire that claims less than the operator's prompt asked for, and nothing would say so.
pub fn capability_bits(names: &[String]) -> Result<u32> {
    let mut bits = 0u32;
    for name in names {
        let lower = name.trim().to_ascii_lowercase();
        match CAPABILITY_FLAGS.iter().find(|(_, n)| *n == lower) {
            Some((mask, _)) => bits |= mask,
            None => {
                let allowed: Vec<&str> = CAPABILITY_FLAGS.iter().map(|(_, n)| *n).collect();
                bail!(
                    "unknown CDP capability '{}'; allowed values are {:?}",
                    name,
                    allowed
                );
            }
        }
    }
    Ok(bits)
}

// ---------------------------------------------------------------------------------------------
// Checksum
// ---------------------------------------------------------------------------------------------

/// The CDP checksum: a 16-bit one's-complement sum (RFC 1071) over the CDP payload with the
/// checksum field zeroed — **with Cisco's non-standard treatment of an odd-length payload.**
///
/// For an even-length payload this is the plain IP checksum. For an odd-length payload RFC 1071
/// says to pad with a trailing zero byte, which makes the final word `last << 8`. Cisco's
/// implementation instead puts the last octet in the *low* half of the final big-endian word and
/// then compensates for an off-by-one in its own sign handling:
///
/// | last byte `L`   | final 16-bit word |
/// |---|---|
/// | `L < 0x80`      | `0x00 << 8 \| L`   |
/// | `L >= 0x80`     | `0xFF << 8 \| (L - 1)` |
///
/// This is not a guess. It is transcribed from Wireshark's `packet-cdp.c`, which builds exactly
/// that padded buffer ("Swap bytes in last word" / "Compensate off-by-one error") before calling
/// `in_cksum`, and it is corroborated by scapy's independent `_CDPChecksum._check_len`. The two
/// disagree on one value: scapy tests `last <= 0x80` where Wireshark tests `last & 0x80`, so for
/// a payload ending in exactly `0x80` scapy produces `0x0080` and Wireshark `0xFF7F`. This
/// implementation follows **Wireshark**, because Wireshark's is the reading that treats the byte
/// as signed (`0x80` is negative, so it takes the compensated branch) and because Wireshark is
/// what an operator will use to check our frames.
///
/// Emitting the RFC-1071 form instead is not a cosmetic difference: Wireshark flags the packet
/// `[incorrect]` and a real Cisco device discards it, which is precisely the failure mode this
/// project's OSPF checksum bug had (`src/server/ospf/CLAUDE.md`).
pub fn checksum(data: &[u8]) -> u16 {
    let mut buf: Vec<u8> = data.to_vec();
    if buf.len() % 2 == 1 {
        let n = buf.len();
        let last = buf[n - 1];
        buf[n - 1] = 0x00;
        buf.push(last);
        if last & 0x80 != 0 {
            buf[n] = last.wrapping_sub(1);
            buf[n - 1] = 0xFF;
        }
    }
    // `buf` is even-length by construction above, so every pair is a complete big-endian word.
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < buf.len() {
        sum += u16::from_be_bytes([buf[i], buf[i + 1]]) as u32;
        i += 2;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// The checksum a CDP payload *should* carry: [`checksum`] over the payload with bytes 2..4
/// forced to zero.
pub fn payload_checksum(payload: &[u8]) -> Result<u16> {
    if payload.len() < CDP_HEADER_LEN {
        bail!(
            "CDP payload is {} bytes, shorter than the 4-byte header",
            payload.len()
        );
    }
    let mut copy = payload.to_vec();
    copy[2] = 0;
    copy[3] = 0;
    Ok(checksum(&copy))
}

// ---------------------------------------------------------------------------------------------
// MAC helpers
// ---------------------------------------------------------------------------------------------

/// Render a MAC as the canonical lowercase colon form.
pub fn mac_to_string(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Parse `aa:bb:cc:dd:ee:ff` or `aa-bb-cc-dd-ee-ff` into six bytes.
pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = text.split(['-', ':']).collect();
    if parts.len() != 6 {
        bail!(
            "'{}' is not a MAC address: expected six colon- or dash-separated octets",
            text
        );
    }
    let mut mac = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(part, 16)
            .map_err(|_| anyhow!("'{}' is not a MAC address: '{}' is not hex", text, part))?;
    }
    Ok(mac)
}

// ---------------------------------------------------------------------------------------------
// Model types
// ---------------------------------------------------------------------------------------------

/// One entry of an Addresses or Management Address TLV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpAddress {
    /// `"ipv4"` or `"ipv6"`.
    pub protocol: String,
    /// Textual address.
    pub address: String,
}

impl CdpAddress {
    pub fn new(addr: IpAddr) -> Self {
        Self {
            protocol: match addr {
                IpAddr::V4(_) => "ipv4".to_string(),
                IpAddr::V6(_) => "ipv6".to_string(),
            },
            address: addr.to_string(),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "protocol": self.protocol, "address": self.address })
    }
}

/// Interface duplex, as advertised in the Duplex TLV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Duplex {
    Half,
    Full,
}

impl Duplex {
    pub fn as_str(self) -> &'static str {
        match self {
            Duplex::Half => "half",
            Duplex::Full => "full",
        }
    }

    fn from_byte(b: u8) -> Self {
        if b == 0 {
            Duplex::Half
        } else {
            Duplex::Full
        }
    }

    fn to_byte(self) -> u8 {
        match self {
            Duplex::Half => 0x00,
            Duplex::Full => 0x01,
        }
    }
}

/// A TLV this codec recognises the existence of but does not model.
///
/// Only the type code, its name where known, and its length are kept. The bytes are deliberately
/// dropped: the project rule is that no event carries raw bytes or base64 to the model, and a
/// length is the honest amount of information available without modelling the TLV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtherTlv {
    pub type_code: u16,
    pub name: &'static str,
    pub length: usize,
}

/// A CDP advertisement, in the terms the LLM reasons about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpAdvertisement {
    pub version: u8,
    pub ttl: u8,
    pub device_id: Option<String>,
    pub port_id: Option<String>,
    pub platform: Option<String>,
    pub software_version: Option<String>,
    pub capabilities: Option<u32>,
    pub native_vlan: Option<u16>,
    pub duplex: Option<Duplex>,
    pub addresses: Vec<CdpAddress>,
    pub management_addresses: Vec<CdpAddress>,
    pub other_tlvs: Vec<OtherTlv>,
}

impl Default for CdpAdvertisement {
    fn default() -> Self {
        // CDPv2 with the Cisco default hold time of 180 seconds.
        Self {
            version: 2,
            ttl: 180,
            device_id: None,
            port_id: None,
            platform: None,
            software_version: None,
            capabilities: None,
            native_vlan: None,
            duplex: None,
            addresses: Vec::new(),
            management_addresses: Vec::new(),
            other_tlvs: Vec::new(),
        }
    }
}

/// What [`decode_payload`] found, including the checksum verdict.
///
/// The verdict is carried alongside rather than inside [`CdpAdvertisement`] so that the
/// advertisement type stays a pure description of *what to say*, usable unchanged as the input
/// to [`encode_payload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedCdp {
    pub advertisement: CdpAdvertisement,
    pub declared_checksum: u16,
    pub computed_checksum: u16,
}

impl DecodedCdp {
    pub fn checksum_valid(&self) -> bool {
        self.declared_checksum == self.computed_checksum
    }
}

// ---------------------------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------------------------

fn push_tlv(out: &mut Vec<u8>, type_code: u16, value: &[u8]) -> Result<()> {
    let len = value
        .len()
        .checked_add(4)
        .and_then(|l| u16::try_from(l).ok())
        .ok_or_else(|| {
            anyhow!(
                "CDP TLV 0x{:04x} value is {} bytes, too long for a 16-bit length",
                type_code,
                value.len()
            )
        })?;
    out.extend_from_slice(&type_code.to_be_bytes());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    Ok(())
}

fn encode_address_list(addresses: &[CdpAddress]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&(addresses.len() as u32).to_be_bytes());
    for entry in addresses {
        let parsed: IpAddr = entry
            .address
            .parse()
            .map_err(|_| anyhow!("'{}' is not an IP address", entry.address))?;
        match parsed {
            IpAddr::V4(v4) => {
                // Protocol type 1 (NLPID), 1-byte protocol 0xCC (IP).
                body.push(0x01);
                body.push(0x01);
                body.push(0xCC);
                body.extend_from_slice(&4u16.to_be_bytes());
                body.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                // Protocol type 2 (802.2), 8-byte SNAP protocol AA AA 03 00 00 00 86 DD.
                body.push(0x02);
                body.push(0x08);
                body.extend_from_slice(&[0xAA, 0xAA, 0x03, 0x00, 0x00, 0x00, 0x86, 0xDD]);
                body.extend_from_slice(&16u16.to_be_bytes());
                body.extend_from_slice(&v6.octets());
            }
        }
    }
    Ok(body)
}

/// Serialise an advertisement to a CDP payload, checksum included.
///
/// TLVs are emitted in ascending type order, which is what real Cisco devices do and what makes
/// the output diffable against a capture.
pub fn encode_payload(ad: &CdpAdvertisement) -> Result<Vec<u8>> {
    if ad.version != 1 && ad.version != 2 {
        bail!("CDP version must be 1 or 2, got {}", ad.version);
    }

    let mut out = Vec::with_capacity(128);
    out.push(ad.version);
    out.push(ad.ttl);
    out.extend_from_slice(&[0, 0]); // checksum placeholder

    if let Some(id) = &ad.device_id {
        push_tlv(&mut out, TLV_DEVICE_ID, id.as_bytes())?;
    }
    if !ad.addresses.is_empty() {
        let body = encode_address_list(&ad.addresses)?;
        push_tlv(&mut out, TLV_ADDRESSES, &body)?;
    }
    if let Some(port) = &ad.port_id {
        push_tlv(&mut out, TLV_PORT_ID, port.as_bytes())?;
    }
    if let Some(caps) = ad.capabilities {
        push_tlv(&mut out, TLV_CAPABILITIES, &caps.to_be_bytes())?;
    }
    if let Some(version) = &ad.software_version {
        push_tlv(&mut out, TLV_SOFTWARE_VERSION, version.as_bytes())?;
    }
    if let Some(platform) = &ad.platform {
        push_tlv(&mut out, TLV_PLATFORM, platform.as_bytes())?;
    }
    if let Some(vlan) = ad.native_vlan {
        push_tlv(&mut out, TLV_NATIVE_VLAN, &vlan.to_be_bytes())?;
    }
    if let Some(duplex) = ad.duplex {
        push_tlv(&mut out, TLV_DUPLEX, &[duplex.to_byte()])?;
    }
    if !ad.management_addresses.is_empty() {
        let body = encode_address_list(&ad.management_addresses)?;
        push_tlv(&mut out, TLV_MANAGEMENT_ADDRESS, &body)?;
    }

    let cksum = payload_checksum(&out)?;
    out[2..4].copy_from_slice(&cksum.to_be_bytes());
    Ok(out)
}

/// Wrap a CDP payload in the 802.3 + LLC/SNAP header addressed to the CDP multicast group.
///
/// The 802.3 length field carries the true length of everything after it (LLC + SNAP + payload).
/// No padding to the 60-byte Ethernet minimum is added: on both transports something else is
/// responsible for it — a NIC pads on transmit, and the UDP test transport has no minimum at
/// all — and padding here would make the emitted bytes differ from the bytes the codec tests
/// assert.
pub fn encode_frame(source_mac: [u8; 6], payload: &[u8]) -> Result<Vec<u8>> {
    let body_len = LLC_SNAP_HEADER_LEN + payload.len();
    let length_field = u16::try_from(body_len)
        .map_err(|_| anyhow!("CDP frame body is {} bytes, too long for 802.3", body_len))?;

    let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + body_len);
    frame.extend_from_slice(&CDP_MULTICAST_MAC);
    frame.extend_from_slice(&source_mac);
    frame.extend_from_slice(&length_field.to_be_bytes());
    frame.extend_from_slice(&LLC_SNAP_HEADER);
    frame.extend_from_slice(payload);
    Ok(frame)
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

/// The addressing of a received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub destination_mac: [u8; 6],
    pub source_mac: [u8; 6],
    /// The 802.3 length field as it appeared on the wire.
    pub declared_length: u16,
}

/// Split a complete Ethernet frame into its header and the CDP payload.
///
/// The LLC/SNAP header is checked byte for byte — DSAP, SSAP, control, OUI and protocol id — so
/// anything that is not CDP is rejected here rather than being handed to the TLV parser. The
/// 802.3 length field is *not* enforced against the captured length: a captured frame is padded
/// to the Ethernet minimum and may be truncated by the snaplen, so requiring agreement would
/// reject real traffic. It is reported instead, and the TLV walk is bounded by the bytes
/// actually present.
pub fn decode_frame(frame: &[u8]) -> Result<(FrameHeader, &[u8])> {
    if frame.len() < CDP_PAYLOAD_OFFSET {
        bail!(
            "frame is {} bytes, too short for an 802.3 + LLC/SNAP CDP header ({} needed)",
            frame.len(),
            CDP_PAYLOAD_OFFSET
        );
    }
    let mut destination_mac = [0u8; 6];
    destination_mac.copy_from_slice(&frame[0..6]);
    let mut source_mac = [0u8; 6];
    source_mac.copy_from_slice(&frame[6..12]);
    let declared_length = u16::from_be_bytes([frame[12], frame[13]]);

    let llc_snap = &frame[ETHERNET_HEADER_LEN..CDP_PAYLOAD_OFFSET];
    if llc_snap != LLC_SNAP_HEADER {
        bail!(
            "not a CDP frame: LLC/SNAP header is {:02x?}, expected {:02x?} \
             (DSAP/SSAP 0xAA, control 0x03, OUI 00:00:0c, protocol 0x2000)",
            llc_snap,
            LLC_SNAP_HEADER
        );
    }

    Ok((
        FrameHeader {
            destination_mac,
            source_mac,
            declared_length,
        },
        &frame[CDP_PAYLOAD_OFFSET..],
    ))
}

fn decode_address_list(body: &[u8]) -> Vec<CdpAddress> {
    let mut out = Vec::new();
    if body.len() < 4 {
        return out;
    }
    let count = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let mut offset = 4usize;
    // Bounded by the declared count *and* by the bytes present, so a hostile count cannot make
    // this spin or over-read.
    while out.len() < count.min(64) && offset + 2 <= body.len() {
        let _protocol_type = body[offset];
        let protocol_len = body[offset + 1] as usize;
        offset += 2;
        if offset + protocol_len + 2 > body.len() {
            break;
        }
        let protocol = &body[offset..offset + protocol_len];
        offset += protocol_len;
        let addr_len = u16::from_be_bytes([body[offset], body[offset + 1]]) as usize;
        offset += 2;
        if offset + addr_len > body.len() {
            break;
        }
        let addr = &body[offset..offset + addr_len];
        offset += addr_len;

        // Identify by address length plus the NLPID/SNAP protocol selector, which is how the
        // two families are actually distinguished on the wire.
        if addr_len == 4 && protocol == [0xCC] {
            out.push(CdpAddress {
                protocol: "ipv4".to_string(),
                address: std::net::Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).to_string(),
            });
        } else if addr_len == 16 {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(addr);
            out.push(CdpAddress {
                protocol: "ipv6".to_string(),
                address: std::net::Ipv6Addr::from(octets).to_string(),
            });
        }
        // Anything else (CLNS, DECnet, AppleTalk...) is skipped rather than guessed at.
    }
    out
}

/// Parse a CDP payload into an advertisement plus the checksum verdict.
pub fn decode_payload(payload: &[u8]) -> Result<DecodedCdp> {
    if payload.len() < CDP_HEADER_LEN {
        bail!(
            "CDP payload is {} bytes, shorter than the 4-byte header",
            payload.len()
        );
    }
    let version = payload[0];
    let ttl = payload[1];
    let declared_checksum = u16::from_be_bytes([payload[2], payload[3]]);
    let computed_checksum = payload_checksum(payload)?;

    let mut ad = CdpAdvertisement {
        version,
        ttl,
        ..CdpAdvertisement::default()
    };

    let mut offset = CDP_HEADER_LEN;
    while offset + 4 <= payload.len() {
        let type_code = u16::from_be_bytes([payload[offset], payload[offset + 1]]);
        let declared = u16::from_be_bytes([payload[offset + 2], payload[offset + 3]]) as usize;
        // A TLV length counts its own 4-byte header. Anything below that would not advance the
        // cursor, so stop rather than loop forever on a malformed advertisement.
        if declared < 4 {
            break;
        }
        let end = offset + declared;
        if end > payload.len() {
            break;
        }
        let value = &payload[offset + 4..end];

        match type_code {
            TLV_DEVICE_ID => ad.device_id = Some(String::from_utf8_lossy(value).into_owned()),
            TLV_PORT_ID => ad.port_id = Some(String::from_utf8_lossy(value).into_owned()),
            TLV_PLATFORM => ad.platform = Some(String::from_utf8_lossy(value).into_owned()),
            TLV_SOFTWARE_VERSION => {
                ad.software_version = Some(String::from_utf8_lossy(value).into_owned())
            }
            TLV_CAPABILITIES if value.len() >= 4 => {
                ad.capabilities = Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
            }
            TLV_NATIVE_VLAN if value.len() >= 2 => {
                ad.native_vlan = Some(u16::from_be_bytes([value[0], value[1]]))
            }
            TLV_DUPLEX if !value.is_empty() => ad.duplex = Some(Duplex::from_byte(value[0])),
            TLV_ADDRESSES => ad.addresses = decode_address_list(value),
            TLV_MANAGEMENT_ADDRESS => ad.management_addresses = decode_address_list(value),
            _ => ad.other_tlvs.push(OtherTlv {
                type_code,
                name: KNOWN_OTHER_TLVS
                    .iter()
                    .find(|(c, _)| *c == type_code)
                    .map(|(_, n)| *n)
                    .unwrap_or("unknown"),
                length: value.len(),
            }),
        }

        offset = end;
    }

    Ok(DecodedCdp {
        advertisement: ad,
        declared_checksum,
        computed_checksum,
    })
}

// ---------------------------------------------------------------------------------------------
// JSON bridges
// ---------------------------------------------------------------------------------------------

fn addresses_json(list: &[CdpAddress]) -> Value {
    Value::Array(list.iter().map(CdpAddress::to_json).collect())
}

impl DecodedCdp {
    /// The event body for `cdp_neighbor_advertisement`.
    ///
    /// Every field is structured — strings, numbers, named capability flags. Nothing here is a
    /// byte array, a hex string or base64, which is the project rule for anything the model
    /// reads (root `CLAUDE.md`, "Action & event design rules").
    pub fn to_event_data(&self, header: &FrameHeader, connection_id: &str) -> Value {
        let ad = &self.advertisement;
        json!({
            "connection_id": connection_id,
            "source_mac": mac_to_string(&header.source_mac),
            "destination_mac": mac_to_string(&header.destination_mac),
            "version": ad.version,
            "ttl": ad.ttl,
            "device_id": ad.device_id,
            "port_id": ad.port_id,
            "platform": ad.platform,
            "software_version": ad.software_version,
            "capabilities": ad.capabilities.map(capability_names).unwrap_or_default(),
            "capabilities_value": ad.capabilities,
            "native_vlan": ad.native_vlan,
            "duplex": ad.duplex.map(|d| d.as_str()),
            "addresses": addresses_json(&ad.addresses),
            "management_addresses": addresses_json(&ad.management_addresses),
            "checksum_valid": self.checksum_valid(),
            "other_tlvs": Value::Array(
                ad.other_tlvs
                    .iter()
                    .map(|t| json!({ "type": t.type_code, "name": t.name, "length": t.length }))
                    .collect(),
            ),
        })
    }
}

fn parse_address_field(field: &str, value: &Value) -> Result<Vec<CdpAddress>> {
    let Some(items) = value.as_array() else {
        bail!("'{}' must be an array of IP addresses", field);
    };
    let mut out = Vec::new();
    for item in items {
        let text = match item {
            Value::String(s) => s.clone(),
            Value::Object(obj) => obj
                .get("address")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("each '{}' entry needs an 'address' string", field))?,
            other => bail!(
                "each '{}' entry must be an IP address string, got {}",
                field,
                other
            ),
        };
        let parsed: IpAddr = text
            .parse()
            .map_err(|_| anyhow!("'{}' in '{}' is not an IP address", text, field))?;
        out.push(CdpAddress::new(parsed));
    }
    Ok(out)
}

fn optional_string(action: &Value, key: &str) -> Result<Option<String>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => bail!("'{}' must be a string, got {}", key, other),
    }
}

impl CdpAdvertisement {
    /// Build an advertisement from a `send_cdp_advertisement` action.
    ///
    /// Pure and total: every rejection is an `Err` naming the offending field, so the action
    /// executor can validate the model's answer without touching a socket. This is also what
    /// makes the declared `example` executable, which `tests/executable_examples_test.rs`
    /// checks across the whole tree.
    pub fn from_action(action: &Value) -> Result<Self> {
        let mut ad = CdpAdvertisement::default();

        if let Some(v) = action.get("version") {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("'version' must be 1 or 2, got {}", v))?;
            if n != 1 && n != 2 {
                bail!("'version' must be 1 or 2, got {}", n);
            }
            ad.version = n as u8;
        }

        if let Some(v) = action.get("ttl") {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("'ttl' must be a number of seconds, got {}", v))?;
            if n > 255 {
                bail!("'ttl' must be 0-255 seconds (CDP carries it in one byte), got {n}");
            }
            ad.ttl = n as u8;
        }

        ad.device_id = optional_string(action, "device_id")?;
        ad.port_id = optional_string(action, "port_id")?;
        ad.platform = optional_string(action, "platform")?;
        ad.software_version = optional_string(action, "software_version")?;

        // A CDP advertisement with no Device ID is not an advertisement: the neighbour keys its
        // table on it. Refuse rather than emit a nameless frame.
        if ad
            .device_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            bail!("send_cdp_advertisement requires a non-empty 'device_id'");
        }

        match action.get("capabilities") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                let names: Vec<String> = items
                    .iter()
                    .map(|i| {
                        i.as_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| anyhow!("'capabilities' entries must be strings"))
                    })
                    .collect::<Result<_>>()?;
                ad.capabilities = Some(capability_bits(&names)?);
            }
            Some(Value::Number(n)) => {
                let raw = n
                    .as_u64()
                    .ok_or_else(|| anyhow!("'capabilities' must be a non-negative number"))?;
                ad.capabilities = Some(u32::try_from(raw).map_err(|_| {
                    anyhow!("'capabilities' bitmask must fit in 32 bits, got {}", raw)
                })?);
            }
            Some(other) => bail!(
                "'capabilities' must be an array of names (e.g. [\"switch\",\"igmp\"]) \
                 or a numeric bitmask, got {}",
                other
            ),
        }

        if let Some(v) = action.get("native_vlan") {
            if !v.is_null() {
                let n = v
                    .as_u64()
                    .ok_or_else(|| anyhow!("'native_vlan' must be a number, got {}", v))?;
                if n > 4094 {
                    bail!("'native_vlan' must be 0-4094, got {}", n);
                }
                ad.native_vlan = Some(n as u16);
            }
        }

        match action.get("duplex") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => {
                ad.duplex = Some(match s.trim().to_ascii_lowercase().as_str() {
                    "full" => Duplex::Full,
                    "half" => Duplex::Half,
                    other => bail!("'duplex' must be \"full\" or \"half\", got \"{}\"", other),
                })
            }
            Some(other) => bail!("'duplex' must be \"full\" or \"half\", got {}", other),
        }

        if let Some(v) = action.get("addresses") {
            if !v.is_null() {
                ad.addresses = parse_address_field("addresses", v)?;
            }
        }
        if let Some(v) = action.get("management_addresses") {
            if !v.is_null() {
                ad.management_addresses = parse_address_field("management_addresses", v)?;
            }
        }

        Ok(ad)
    }

    /// A one-line summary for the status stream and the access log.
    pub fn summary(&self) -> String {
        format!(
            "device_id={} port_id={} platform={} native_vlan={} ttl={}",
            self.device_id.as_deref().unwrap_or("-"),
            self.port_id.as_deref().unwrap_or("-"),
            self.platform.as_deref().unwrap_or("-"),
            self.native_vlan
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".to_string()),
            self.ttl
        )
    }
}
