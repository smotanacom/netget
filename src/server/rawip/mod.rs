//! Generic raw IP protocol-N server.
//!
//! This is the **generic home for IP protocols that have no other** — GRE (47), ESP (50),
//! AH (51), SCTP (132), and anything else the operator names by number. It binds one
//! `SOCK_RAW` socket for one operator-chosen IP protocol number, decodes the **IP header**
//! (which is generic: the same twenty bytes whatever rides on top), surfaces the payload as
//! opaque bytes, and lets the model decide what to do.
//!
//! # The rule that defines this module
//!
//! **There is no per-protocol branching here, and there must never be.** No
//! `match protocol_number { 47 => parse_gre(), 50 => parse_esp(), .. }`. The moment this file
//! knows about GRE specifically it has become the wrong thing, and the next change adds ESP,
//! then L2TP, and it is the centralized per-protocol logic `CLAUDE.md`'s decentralization rule
//! forbids. A protocol that deserves real parsing deserves its own module under `src/server/`.
//!
//! The one table keyed by protocol number is [`ip_protocol_name`], and it maps a number to the
//! IANA *name* so the model is told "47 (GRE)" rather than "47". A name is not behaviour: no
//! code path branches on it, and adding an entry cannot change what the server does.
//!
//! # Layout
//!
//! The **decoder is deliberately split from the transport**: [`decode_ip_packet`] and its
//! helpers are pure functions over a byte slice with no I/O, no state and no privilege, so
//! they are tested against literal packet bytes (`tests/server/rawip/e2e_test.rs`). The
//! transport below is the part that has never been executed — a raw socket needs root.

pub mod actions;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde_json::json;
use socket2::{Domain, Protocol as SockProtocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, StartupParams};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::utils::truncate_for_log;
use actions::{RawIpProtocol, RAWIP_PACKET_RECEIVED_EVENT};

// ============================================================================
// Pure IP header decoding — no I/O, no privilege, no protocol-specific branching.
// ============================================================================

/// Minimum length of an IPv4 header, in bytes (RFC 791 §3.1).
pub const IPV4_MIN_HEADER_LEN: usize = 20;
/// Length of the fixed IPv6 header, in bytes (RFC 8200 §3).
pub const IPV6_HEADER_LEN: usize = 40;

/// How much of the payload is placed in the event handed to the model.
///
/// A 64 KB packet hex-encodes to 128 KB of prompt, which no model reads and every model pays
/// for. The full length is always reported in `payload_length`, and `payload_truncated` says
/// whether what the model sees is the whole thing.
pub const EVENT_PAYLOAD_LIMIT: usize = 2048;

/// Why a buffer could not be read as an IP packet.
///
/// Every variant is a **refusal**, never a panic: the bytes come off the wire from a stranger,
/// and this decoder is the only thing between them and the server loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpDecodeError {
    /// Fewer bytes than the version's fixed header needs.
    TooShort {
        got: usize,
        need: usize,
        version: u8,
    },
    /// The version nibble was neither 4 nor 6.
    UnsupportedVersion(u8),
    /// IPv4 IHL below the legal minimum of 5 words (20 bytes).
    IhlTooSmall(u8),
    /// IPv4 IHL claims a header longer than the bytes actually captured.
    HeaderTruncated { ihl_bytes: usize, got: usize },
}

impl std::fmt::Display for IpDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpDecodeError::TooShort {
                got,
                need,
                version,
            } => write!(
                f,
                "truncated IPv{version} packet: {got} bytes captured, {need} needed for the header"
            ),
            IpDecodeError::UnsupportedVersion(v) => {
                write!(f, "not an IP packet: version nibble is {v}, expected 4 or 6")
            }
            IpDecodeError::IhlTooSmall(ihl) => write!(
                f,
                "malformed IPv4 header: IHL is {ihl} words, the minimum is 5 (RFC 791 §3.1)"
            ),
            IpDecodeError::HeaderTruncated { ihl_bytes, got } => write!(
                f,
                "malformed IPv4 header: IHL claims {ihl_bytes} header bytes but only {got} were captured"
            ),
        }
    }
}

impl std::error::Error for IpDecodeError {}

/// The IPv4 header fields, decoded (RFC 791 §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipv4Header {
    pub ihl: u8,
    pub header_length: usize,
    pub dscp: u8,
    pub ecn: u8,
    pub total_length: u16,
    pub identification: u16,
    /// Bit 0 of the flags field. Must be zero; a peer setting it is worth surfacing.
    pub reserved_flag: bool,
    pub dont_fragment: bool,
    pub more_fragments: bool,
    /// In 8-byte units, exactly as it appears on the wire.
    pub fragment_offset: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub header_checksum: u16,
    pub source: Ipv4Addr,
    pub destination: Ipv4Addr,
    pub options_present: bool,
    pub options_length: usize,
}

/// The fixed IPv6 header fields, decoded (RFC 8200 §3).
///
/// Extension headers are **not** walked. `next_header` is reported as it appears; if it names
/// an extension header rather than an upper-layer protocol, the payload begins with that
/// extension header and the model is looking at it. Walking the chain would mean knowing which
/// numbers are extension headers and how each one is sized — per-protocol knowledge this
/// module deliberately does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipv6Header {
    pub traffic_class: u8,
    pub flow_label: u32,
    /// The header's own Payload Length field, in bytes. Not necessarily what was captured.
    pub payload_length: u16,
    pub next_header: u8,
    pub hop_limit: u8,
    pub source: Ipv6Addr,
    pub destination: Ipv6Addr,
}

/// A decoded IP header, either version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpHeader {
    V4(Ipv4Header),
    V6(Ipv6Header),
}

impl IpHeader {
    /// 4 or 6.
    pub fn version(&self) -> u8 {
        match self {
            IpHeader::V4(_) => 4,
            IpHeader::V6(_) => 6,
        }
    }

    /// The number identifying what rides on top: IPv4 `protocol`, IPv6 `next_header`.
    pub fn upper_protocol(&self) -> u8 {
        match self {
            IpHeader::V4(h) => h.protocol,
            IpHeader::V6(h) => h.next_header,
        }
    }

    pub fn source(&self) -> IpAddr {
        match self {
            IpHeader::V4(h) => IpAddr::V4(h.source),
            IpHeader::V6(h) => IpAddr::V6(h.source),
        }
    }

    pub fn destination(&self) -> IpAddr {
        match self {
            IpHeader::V4(h) => IpAddr::V4(h.destination),
            IpHeader::V6(h) => IpAddr::V6(h.destination),
        }
    }
}

/// A decoded packet: its header, and the payload that followed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPacket {
    pub header: IpHeader,
    pub payload: Vec<u8>,
    /// True when the header's length field promised more payload than was captured. A raw
    /// socket read that fills the buffer, or a datagram cut short, both land here — the model
    /// is told rather than being handed a short payload silently.
    pub payload_incomplete: bool,
}

/// Decode an IP header and split off its payload.
///
/// Version-generic: dispatches on the version nibble only, which is not per-protocol logic —
/// IPv4 and IPv6 are different header formats, not different protocols carried *by* IP.
///
/// Returns `Err` for anything that is not a well-formed header. It never panics and never
/// indexes past the slice: a hostile or truncated packet is refused, which is the whole point
/// of this function existing separately from the socket that produced the bytes.
pub fn decode_ip_packet(bytes: &[u8]) -> std::result::Result<DecodedPacket, IpDecodeError> {
    let version = match bytes.first() {
        Some(b) => b >> 4,
        None => {
            return Err(IpDecodeError::TooShort {
                got: 0,
                need: IPV4_MIN_HEADER_LEN,
                version: 0,
            })
        }
    };

    match version {
        4 => decode_ipv4(bytes),
        6 => decode_ipv6(bytes),
        other => Err(IpDecodeError::UnsupportedVersion(other)),
    }
}

fn decode_ipv4(bytes: &[u8]) -> std::result::Result<DecodedPacket, IpDecodeError> {
    if bytes.len() < IPV4_MIN_HEADER_LEN {
        return Err(IpDecodeError::TooShort {
            got: bytes.len(),
            need: IPV4_MIN_HEADER_LEN,
            version: 4,
        });
    }

    let ihl = bytes[0] & 0x0F;
    if ihl < 5 {
        return Err(IpDecodeError::IhlTooSmall(ihl));
    }
    let header_length = ihl as usize * 4;
    if bytes.len() < header_length {
        return Err(IpDecodeError::HeaderTruncated {
            ihl_bytes: header_length,
            got: bytes.len(),
        });
    }

    let total_length = u16::from_be_bytes([bytes[2], bytes[3]]);
    let flags_and_offset = u16::from_be_bytes([bytes[6], bytes[7]]);

    let header = Ipv4Header {
        ihl,
        header_length,
        dscp: bytes[1] >> 2,
        ecn: bytes[1] & 0x03,
        total_length,
        identification: u16::from_be_bytes([bytes[4], bytes[5]]),
        reserved_flag: flags_and_offset & 0x8000 != 0,
        dont_fragment: flags_and_offset & 0x4000 != 0,
        more_fragments: flags_and_offset & 0x2000 != 0,
        fragment_offset: flags_and_offset & 0x1FFF,
        ttl: bytes[8],
        protocol: bytes[9],
        header_checksum: u16::from_be_bytes([bytes[10], bytes[11]]),
        source: Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]),
        destination: Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]),
        options_present: ihl > 5,
        options_length: header_length - IPV4_MIN_HEADER_LEN,
    };

    // Trust `total_length` only as an upper bound, and only when it is self-consistent. A
    // peer that lies about it must not be able to make us read past the buffer, and BSD raw
    // sockets have historically delivered this field in host byte order, which would make a
    // strict reading wrong on macOS.
    let declared_end = total_length as usize;
    let available_end = bytes.len();
    let end = if declared_end >= header_length && declared_end <= available_end {
        declared_end
    } else {
        available_end
    };
    let payload_incomplete = declared_end > available_end;

    Ok(DecodedPacket {
        header: IpHeader::V4(header),
        payload: bytes[header_length..end].to_vec(),
        payload_incomplete,
    })
}

fn decode_ipv6(bytes: &[u8]) -> std::result::Result<DecodedPacket, IpDecodeError> {
    if bytes.len() < IPV6_HEADER_LEN {
        return Err(IpDecodeError::TooShort {
            got: bytes.len(),
            need: IPV6_HEADER_LEN,
            version: 6,
        });
    }

    let mut src = [0u8; 16];
    src.copy_from_slice(&bytes[8..24]);
    let mut dst = [0u8; 16];
    dst.copy_from_slice(&bytes[24..40]);

    let payload_length = u16::from_be_bytes([bytes[4], bytes[5]]);

    let header = Ipv6Header {
        traffic_class: ((bytes[0] & 0x0F) << 4) | (bytes[1] >> 4),
        flow_label: (((bytes[1] & 0x0F) as u32) << 16)
            | ((bytes[2] as u32) << 8)
            | (bytes[3] as u32),
        payload_length,
        next_header: bytes[6],
        hop_limit: bytes[7],
        source: Ipv6Addr::from(src),
        destination: Ipv6Addr::from(dst),
    };

    let declared_end = IPV6_HEADER_LEN + payload_length as usize;
    let available_end = bytes.len();
    let end = if declared_end <= available_end {
        declared_end
    } else {
        available_end
    };

    Ok(DecodedPacket {
        header: IpHeader::V6(header),
        payload: bytes[IPV6_HEADER_LEN..end].to_vec(),
        payload_incomplete: declared_end > available_end,
    })
}

/// The IANA name for an IP protocol number, where one is well known.
///
/// A **table of names, not a table of behaviour.** Nothing in this module branches on the
/// result: it is placed in the event so the model reads "47 (GRE)" instead of "47", and an
/// unknown number is reported as a number, which is a perfectly usable answer. Adding an
/// entry here can change no decoding, no framing and no reply — that is exactly why it does
/// not violate the no-per-protocol-logic rule.
///
/// Source: IANA "Assigned Internet Protocol Numbers".
pub fn ip_protocol_name(number: u8) -> Option<&'static str> {
    Some(match number {
        0 => "HOPOPT",
        1 => "ICMP",
        2 => "IGMP",
        3 => "GGP",
        4 => "IPv4-in-IPv4",
        5 => "ST",
        6 => "TCP",
        8 => "EGP",
        9 => "IGP",
        17 => "UDP",
        27 => "RDP",
        33 => "DCCP",
        41 => "IPv6-in-IPv4",
        43 => "IPv6-Route",
        44 => "IPv6-Frag",
        46 => "RSVP",
        47 => "GRE",
        50 => "ESP",
        51 => "AH",
        58 => "IPv6-ICMP",
        59 => "IPv6-NoNxt",
        60 => "IPv6-Opts",
        88 => "EIGRP",
        89 => "OSPFIGP",
        94 => "IPIP",
        97 => "ETHERIP",
        98 => "ENCAP",
        103 => "PIM",
        108 => "IPComp",
        112 => "VRRP",
        115 => "L2TP",
        124 => "ISIS-over-IPv4",
        132 => "SCTP",
        133 => "FC",
        135 => "Mobility-Header",
        136 => "UDPLite",
        137 => "MPLS-in-IP",
        139 => "HIP",
        140 => "Shim6",
        141 => "WESP",
        142 => "ROHC",
        143 => "Ethernet",
        253 | 254 => "Experimental",
        255 => "Reserved",
        _ => return None,
    })
}

/// How a payload is represented in an event or an action.
///
/// **Explicit on both directions, never sniffed on the way out.** `"48656c6c6f"` is
/// simultaneously valid text and valid hex and only the sender knows which it meant — the
/// `send_tcp_data` defect `CLAUDE.md` records was exactly this, documented as accepting hex
/// and then calling `as_bytes()` on it. Inbound, the server states which encoding it chose;
/// outbound, [`decode_payload`] honours what the action says and rejects anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadEncoding {
    Utf8,
    Hex,
}

impl PayloadEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            PayloadEncoding::Utf8 => "utf8",
            PayloadEncoding::Hex => "hex",
        }
    }

    /// Parse the `encoding` field of an action. Unknown values are an error, not a fallback:
    /// silently treating `"base64"` as utf8 would put the base64 text itself on the wire.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "utf8" | "utf-8" => Ok(PayloadEncoding::Utf8),
            "hex" => Ok(PayloadEncoding::Hex),
            other => Err(anyhow!(
                "unknown payload encoding '{other}': use \"utf8\" (default) or \"hex\""
            )),
        }
    }
}

/// Turn action payload text into the exact bytes to put on the wire.
///
/// This is the half that must actually exist. An action documented as accepting hex whose
/// executor calls `as_bytes()` puts the ASCII of the hex digits on the wire, which is the
/// reference defect in `CLAUDE.md`'s action design rules.
pub fn decode_payload(text: &str, encoding: PayloadEncoding) -> Result<Vec<u8>> {
    match encoding {
        PayloadEncoding::Utf8 => Ok(text.as_bytes().to_vec()),
        PayloadEncoding::Hex => {
            let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            hex::decode(&cleaned).map_err(|e| {
                anyhow!(
                    "payload was declared encoding=\"hex\" but is not valid hex: {e}. \
                     Send the bytes as an even-length string of hex digits, or set \
                     encoding to \"utf8\" to send the text itself."
                )
            })
        }
    }
}

/// Choose how to show an opaque payload to the model, and say which was chosen.
///
/// Text that is genuinely readable is shown as text; anything else is hex. The choice is
/// reported in the event as `payload_encoding`, so the model can echo the same value back in
/// its action and the round trip is symmetric.
pub fn encode_payload_for_event(payload: &[u8]) -> (PayloadEncoding, String) {
    match std::str::from_utf8(payload) {
        Ok(text)
            if text
                .chars()
                .all(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t') =>
        {
            (PayloadEncoding::Utf8, text.to_string())
        }
        _ => (PayloadEncoding::Hex, hex::encode(payload)),
    }
}

/// Build the `rawip_packet_received` event body for a decoded packet.
///
/// Kept pure and separate from the socket so it can be asserted directly against literal
/// packet bytes.
pub fn packet_event_data(
    packet: &DecodedPacket,
    listening_protocol_number: u8,
    connection_id: ConnectionId,
) -> serde_json::Value {
    let full_len = packet.payload.len();
    let shown = &packet.payload[..full_len.min(EVENT_PAYLOAD_LIMIT)];
    let (encoding, payload) = encode_payload_for_event(shown);

    let mut data = json!({
        "connection_id": connection_id.to_string(),
        "listening_protocol_number": listening_protocol_number,
        "ip_version": packet.header.version(),
        "source": packet.header.source().to_string(),
        "destination": packet.header.destination().to_string(),
        "payload_length": full_len,
        "payload": payload,
        "payload_encoding": encoding.as_str(),
        "payload_truncated": full_len > EVENT_PAYLOAD_LIMIT,
        "payload_incomplete": packet.payload_incomplete,
    });

    let obj = data.as_object_mut().expect("json! built an object");

    match &packet.header {
        IpHeader::V4(h) => {
            obj.insert("ihl".into(), json!(h.ihl));
            obj.insert("header_length".into(), json!(h.header_length));
            obj.insert("dscp".into(), json!(h.dscp));
            obj.insert("ecn".into(), json!(h.ecn));
            obj.insert("total_length".into(), json!(h.total_length));
            obj.insert("identification".into(), json!(h.identification));
            obj.insert(
                "flags".into(),
                json!({
                    "reserved": h.reserved_flag,
                    "dont_fragment": h.dont_fragment,
                    "more_fragments": h.more_fragments,
                }),
            );
            obj.insert("fragment_offset".into(), json!(h.fragment_offset));
            obj.insert(
                "fragmented".into(),
                json!(h.more_fragments || h.fragment_offset != 0),
            );
            obj.insert("ttl".into(), json!(h.ttl));
            obj.insert("protocol".into(), json!(h.protocol));
            obj.insert("protocol_name".into(), json!(ip_protocol_name(h.protocol)));
            obj.insert("header_checksum".into(), json!(h.header_checksum));
            obj.insert("options_present".into(), json!(h.options_present));
            obj.insert("options_length".into(), json!(h.options_length));
        }
        IpHeader::V6(h) => {
            obj.insert("traffic_class".into(), json!(h.traffic_class));
            obj.insert("flow_label".into(), json!(h.flow_label));
            // The header's own field, distinct from `payload_length` above, which is how
            // many payload bytes were actually captured.
            obj.insert("ipv6_payload_length".into(), json!(h.payload_length));
            obj.insert("next_header".into(), json!(h.next_header));
            obj.insert(
                "next_header_name".into(),
                json!(ip_protocol_name(h.next_header)),
            );
            obj.insert("protocol".into(), json!(h.next_header));
            obj.insert(
                "protocol_name".into(),
                json!(ip_protocol_name(h.next_header)),
            );
            obj.insert("hop_limit".into(), json!(h.hop_limit));
        }
    }

    data
}

// ============================================================================
// Configuration — every declared startup parameter is parsed here, and nowhere else.
// ============================================================================

/// Which IP version the socket speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpVersion {
    V4,
    V6,
}

/// Which transport carries packets to this server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawIpTransport {
    /// The real thing: `SOCK_RAW` on the configured protocol number. Needs root/`CAP_NET_RAW`.
    Raw,
    /// A UDP socket that carries **whole IP packets as datagrams**, so the decode → event →
    /// LLM → action path can be exercised without privilege. Never a production transport.
    Udp,
}

/// The operator's configuration, parsed from the declared startup parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawIpConfig {
    pub protocol_number: u8,
    pub ip_version: IpVersion,
    pub transport: RawIpTransport,
}

impl RawIpConfig {
    /// Parse and validate the three declared startup parameters.
    ///
    /// Errors are propagated with `?`, never unwrapped: these values come from the model or
    /// an MCP client, and a panic here kills the per-request task before it can reply.
    pub fn from_startup_params(params: Option<&StartupParams>) -> Result<Self> {
        let params = params.ok_or_else(|| {
            anyhow!(
                "rawip requires a 'protocol_number' startup parameter naming the IP protocol \
                 to listen for (for example 47 for GRE, 50 for ESP, 132 for SCTP). It is \
                 generic by design and has no default number to fall back on."
            )
        })?;

        let raw = params.get_i64("protocol_number")?;
        if !(0..=255).contains(&raw) {
            return Err(anyhow!(
                "protocol_number must be an IP protocol number in 0..=255, got {raw}"
            ));
        }
        let protocol_number = raw as u8;

        if protocol_number == 6 || protocol_number == 17 {
            let (name, feature) = if protocol_number == 6 {
                ("TCP", "tcp")
            } else {
                ("UDP", "udp")
            };
            return Err(anyhow!(
                "protocol_number {protocol_number} is {name}, which netget implements properly \
                 as its own protocol — start the '{feature}' server instead. A SOCK_RAW listener \
                 on {protocol_number} would compete with the kernel's own {name} stack for the \
                 same packets, and it cannot complete a {name} session in any case."
            ));
        }

        let ip_version = match params.get_optional_string("ip_version")? {
            None => IpVersion::V4,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "ipv4" | "v4" | "4" => IpVersion::V4,
                "ipv6" | "v6" | "6" => IpVersion::V6,
                other => {
                    return Err(anyhow!(
                        "ip_version must be \"ipv4\" or \"ipv6\", got \"{other}\""
                    ))
                }
            },
        };

        let transport = match params.get_optional_string("transport")? {
            None => RawIpTransport::Raw,
            Some(v) => match v.to_ascii_lowercase().as_str() {
                "raw" => RawIpTransport::Raw,
                "udp" => RawIpTransport::Udp,
                other => {
                    return Err(anyhow!(
                        "transport must be \"raw\" (default) or \"udp\" (the unprivileged test \
                         transport), got \"{other}\""
                    ))
                }
            },
        };

        Ok(Self {
            protocol_number,
            ip_version,
            transport,
        })
    }
}

// ============================================================================
// Transport
// ============================================================================

/// Whatever this server writes packets with.
enum Emitter {
    /// A second raw socket, as ICMP does, so a reply never races the receive socket's state.
    /// The `Mutex` is `std`'s and is never held across an `.await`: `send_raw` is synchronous.
    Raw {
        socket: std::sync::Mutex<Socket>,
        version: IpVersion,
    },
    /// The test transport writes the payload back to the peer that sent the datagram.
    Udp { socket: Arc<UdpSocket> },
}

struct RawIpState {
    config: RawIpConfig,
    emitter: Emitter,
    local_addr: SocketAddr,
}

impl RawIpState {
    /// Write `payload` and report how many bytes went out.
    ///
    /// `peer` is the datagram source on the UDP test transport and `None` on the raw one.
    async fn emit(
        &self,
        destination: Option<IpAddr>,
        ttl: Option<u8>,
        payload: &[u8],
        peer: Option<SocketAddr>,
    ) -> Result<(usize, String)> {
        match &self.emitter {
            Emitter::Raw { socket, version } => {
                let destination = destination.ok_or_else(|| {
                    anyhow!(
                        "send_rawip_packet needs a 'destination' IP address: a raw socket has \
                         no connection to reply on"
                    )
                })?;
                let n = send_raw(socket, *version, destination, ttl, payload)?;
                Ok((n, destination.to_string()))
            }
            Emitter::Udp { socket } => {
                let peer = peer.ok_or_else(|| {
                    anyhow!("the UDP test transport can only answer the peer that sent a datagram")
                })?;
                let n = socket.send_to(payload, peer).await?;
                Ok((n, peer.to_string()))
            }
        }
    }
}

/// Synchronous by construction, so no lock guard can be alive across an `.await`.
fn send_raw(
    socket: &std::sync::Mutex<Socket>,
    version: IpVersion,
    destination: IpAddr,
    ttl: Option<u8>,
    payload: &[u8],
) -> Result<usize> {
    let guard = socket
        .lock()
        .map_err(|_| anyhow!("rawip send socket mutex was poisoned by a previous panic"))?;
    if let Some(ttl) = ttl {
        match version {
            IpVersion::V4 => guard.set_ttl(ttl as u32)?,
            IpVersion::V6 => guard.set_unicast_hops_v6(ttl as u32)?,
        }
    }
    let addr = SockAddr::from(SocketAddr::new(destination, 0));
    Ok(guard.send_to(payload, &addr)?)
}

/// Generic raw IP protocol-N server.
pub struct RawIpServer;

impl RawIpServer {
    /// Bind the socket and start receiving.
    ///
    /// **Readiness is awaited and failure is returned.** A raw socket cannot be opened without
    /// privilege, and a server sitting in `Running` having captured nothing is the exact
    /// ARP/DataLink/ICMP defect the root `CLAUDE.md` records — so the socket is created here,
    /// before any task is spawned, and its error propagates out of `spawn()` so
    /// `server_startup` records `ServerStatus::Error`.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        let config = RawIpConfig::from_startup_params(startup_params.as_ref())?;

        let protocol_label = match ip_protocol_name(config.protocol_number) {
            Some(name) => format!("{} ({})", config.protocol_number, name),
            None => format!("{} (no IANA name)", config.protocol_number),
        };

        match config.transport {
            RawIpTransport::Raw => {
                Self::spawn_raw(
                    listen_addr,
                    config,
                    protocol_label,
                    llm_client,
                    app_state,
                    status_tx,
                    server_id,
                )
                .await
            }
            RawIpTransport::Udp => {
                Self::spawn_udp(
                    listen_addr,
                    config,
                    protocol_label,
                    llm_client,
                    app_state,
                    status_tx,
                    server_id,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_raw(
        listen_addr: SocketAddr,
        config: RawIpConfig,
        protocol_label: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let domain = match config.ip_version {
            IpVersion::V4 => Domain::IPV4,
            IpVersion::V6 => Domain::IPV6,
        };
        let sock_protocol = SockProtocol::from(config.protocol_number as i32);

        let recv_socket = Socket::new(domain, Type::RAW, Some(sock_protocol)).map_err(|e| {
            anyhow!(
                "failed to create the raw IP protocol-{} receive socket (needs root, or \
                 CAP_NET_RAW on Linux): {e}",
                config.protocol_number
            )
        })?;
        recv_socket.set_nonblocking(true).map_err(|e| {
            anyhow!("failed to put the rawip receive socket in non-blocking mode: {e}")
        })?;

        let send_socket = Socket::new(domain, Type::RAW, Some(sock_protocol))
            .map_err(|e| anyhow!("failed to create the rawip send socket: {e}"))?;

        let version = config.ip_version;
        let state = Arc::new(RawIpState {
            config,
            emitter: Emitter::Raw {
                socket: std::sync::Mutex::new(send_socket),
                version,
            },
            local_addr: listen_addr,
        });

        Log::new(Some(&status_tx)).info(format!(
            "Raw IP server listening for IP protocol {protocol_label} (requires root)"
        ));

        let async_socket = AsyncFd::new(recv_socket)
            .map_err(|e| anyhow!("failed to register the rawip socket with the reactor: {e}"))?;

        let task_registrar = app_state.clone();
        let handle = tokio::spawn(async move {
            let mut buffer = vec![std::mem::MaybeUninit::<u8>::uninit(); 65535];
            loop {
                let mut guard = match async_socket.readable().await {
                    Ok(g) => g,
                    Err(e) => {
                        error!("rawip socket error: {e}");
                        break;
                    }
                };

                match guard.try_io(|inner| inner.get_ref().recv(&mut buffer)) {
                    Ok(Ok(0)) => continue,
                    Ok(Ok(n)) => {
                        // SAFETY: `recv` reported `n` bytes initialised at the front.
                        let bytes = unsafe {
                            std::slice::from_raw_parts(buffer.as_ptr() as *const u8, n).to_vec()
                        };
                        Self::spawn_packet_task(
                            bytes,
                            None,
                            llm_client.clone(),
                            app_state.clone(),
                            status_tx.clone(),
                            state.clone(),
                            server_id,
                        );
                    }
                    Ok(Err(e)) => error!("rawip recv error: {e}"),
                    Err(_would_block) => continue,
                }
            }
            warn!("rawip receive loop terminated");
        });

        task_registrar.register_server_task(server_id, handle).await;

        Ok(listen_addr)
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_udp(
        listen_addr: SocketAddr,
        config: RawIpConfig,
        protocol_label: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket =
            Arc::new(UdpSocket::bind(listen_addr).await.map_err(|e| {
                anyhow!("rawip UDP test transport could not bind {listen_addr}: {e}")
            })?);
        let local_addr = socket.local_addr()?;

        let state = Arc::new(RawIpState {
            config,
            emitter: Emitter::Udp {
                socket: socket.clone(),
            },
            local_addr,
        });

        Log::new(Some(&status_tx)).info(format!(
            "Raw IP server on the UDP test transport at {local_addr}, decoding whole IP packets \
             for protocol {protocol_label} (no raw socket, no privilege — not a production \
             transport)"
        ));

        let task_registrar = app_state.clone();
        let handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; 65535];
            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer)) => {
                        Self::spawn_packet_task(
                            buffer[..n].to_vec(),
                            Some(peer),
                            llm_client.clone(),
                            app_state.clone(),
                            status_tx.clone(),
                            state.clone(),
                            server_id,
                        );
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("rawip UDP test transport receive error: {e}"));
                        break;
                    }
                }
            }
        });

        task_registrar.register_server_task(server_id, handle).await;

        Ok(local_addr)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_packet_task(
        bytes: Vec<u8>,
        peer: Option<SocketAddr>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        state: Arc<RawIpState>,
        server_id: crate::state::ServerId,
    ) {
        tokio::spawn(async move {
            if let Err(e) = Self::handle_packet(
                bytes, peer, llm_client, app_state, status_tx, state, server_id,
            )
            .await
            {
                error!("rawip packet handling failed: {e}");
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_packet(
        bytes: Vec<u8>,
        peer: Option<SocketAddr>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        state: Arc<RawIpState>,
        server_id: crate::state::ServerId,
    ) -> Result<()> {
        let packet = match decode_ip_packet(&bytes) {
            Ok(p) => p,
            Err(e) => {
                // A packet we cannot read is dropped and logged. It is not handed to the
                // model as "here are some bytes": the event's whole contract is that its
                // header fields are decoded.
                Log::new(Some(&status_tx)).debug(format!(
                    "rawip dropped {} bytes: {e} (first bytes: {})",
                    bytes.len(),
                    truncate_for_log(&hex::encode(&bytes), 64)
                ));
                return Ok(());
            }
        };

        let source = packet.header.source();
        let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);

        Self::record_connection(
            &app_state,
            server_id,
            connection_id,
            SocketAddr::new(source, 0),
            state.local_addr,
            bytes.len(),
        )
        .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        let (shown_encoding, shown_payload) = encode_payload_for_event(&packet.payload);
        Log::new(Some(&status_tx)).debug(format!(
            "rawip IPv{} {} -> {} proto={} payload={} bytes",
            packet.header.version(),
            source,
            packet.header.destination(),
            packet.header.upper_protocol(),
            packet.payload.len()
        ));
        Log::new(Some(&status_tx)).trace(format!(
            "rawip payload ({}): {}",
            shown_encoding.as_str(),
            truncate_for_log(&shown_payload, 512)
        ));

        let event = Event::new(
            &RAWIP_PACKET_RECEIVED_EVENT,
            packet_event_data(&packet, state.config.protocol_number, connection_id),
        );

        let protocol = RawIpProtocol::new();
        let result = call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await;

        match result {
            Ok(execution) => {
                for message in &execution.messages {
                    Log::new(Some(&status_tx)).info(message.to_string());
                }

                let answered_nothing = execution.raw_actions.is_empty();
                let explicit_silence = !answered_nothing
                    && execution.raw_actions.iter().all(|a| {
                        a.get("type").and_then(|v| v.as_str()) == Some(actions::NO_RESPONSE)
                    });
                let action_failures = execution.failures.len();

                let mut packets_sent = 0usize;
                for protocol_result in execution.protocol_results {
                    let ActionResult::Custom { name, data } = &protocol_result else {
                        continue;
                    };
                    if name != actions::RAWIP_PACKET_RESULT {
                        continue;
                    }

                    let destination = data
                        .get("destination")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<IpAddr>().ok());
                    let ttl = data
                        .get("ttl")
                        .and_then(|v| v.as_u64())
                        .map(|v| v.min(255) as u8);
                    let payload = match data.get("payload_hex").and_then(|v| v.as_str()) {
                        Some(h) => match hex::decode(h) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                Log::new(Some(&status_tx)).error(format!(
                                    "rawip could not re-read its own executor output: {e}"
                                ));
                                continue;
                            }
                        },
                        None => continue,
                    };

                    match state.emit(destination, ttl, &payload, peer).await {
                        Ok((n, where_to)) => {
                            packets_sent += 1;
                            // `record_connection` seeds `bytes_received` at creation and
                            // nothing updated `bytes_sent`, so the rail's `up` counter stayed
                            // at zero for the life of the server however much it emitted.
                            app_state
                                .update_connection_stats(
                                    server_id,
                                    connection_id,
                                    None,
                                    Some(n as u64),
                                    None,
                                    Some(1),
                                )
                                .await;
                            Log::new(Some(&status_tx))
                                .debug(format!("rawip sent {n} payload bytes to {where_to}"));
                            Log::new(Some(&status_tx)).trace(format!(
                                "rawip sent (hex): {}",
                                truncate_for_log(&hex::encode(&payload), 512)
                            ));
                        }
                        Err(e) => {
                            Log::new(Some(&status_tx)).error(format!("rawip send failed: {e}"));
                        }
                    }
                }

                // The wire cannot carry the distinction — every one of these outcomes is
                // silence to the peer — so the log has to. The tokens are stable so an
                // operator can grep `decision=fail_closed_` for every packet netget failed
                // to answer, as distinct from one the model deliberately did not answer.
                let decision = if packets_sent > 0 {
                    "model_reply"
                } else if explicit_silence {
                    "model_reject"
                } else if answered_nothing {
                    "model_silent"
                } else if action_failures > 0 {
                    "fail_closed_action_error"
                } else {
                    "fail_closed_no_reply"
                };

                let summary = format!(
                    "rawip proto={} from {} decision={} sent={}",
                    state.config.protocol_number, source, decision, packets_sent
                );
                if decision.starts_with("fail_closed_") {
                    Log::new(Some(&status_tx)).error(format!("{summary} (nothing on the wire)"));
                } else {
                    Log::new(Some(&status_tx)).info(summary);
                }
            }
            Err(e) => {
                // **Silence is the correct answer here, and it is not a compromise.**
                // NetGet does not know what a peer speaking an arbitrary IP protocol
                // expects: this server is generic precisely because the protocol above IP is
                // one it deliberately does not understand. There is no error frame to send,
                // because there is no protocol to send one in — any bytes emitted would be a
                // guess, and a guess a peer parses is worse than a packet it never receives.
                // So nothing derived from `e` reaches the socket; the failure is reported
                // to the operator here and nowhere else. No `WireFailure` text is written to
                // the wire, deliberately: there is no wire format to write it in.
                let category = crate::utils::WireFailure::classify(&e);
                Log::new(Some(&status_tx)).error(format!(
                    "rawip proto={} from {} decision=fail_closed_llm_error category={} \
                     (no packet sent: a generic IP protocol has no error frame, and inventing \
                     one would be a guess at a protocol netget does not implement): {e}",
                    state.config.protocol_number,
                    source,
                    if category.is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    },
                ));
            }
        }

        Ok(())
    }

    async fn record_connection(
        app_state: &AppState,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        bytes_received: usize,
    ) {
        use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};

        let now = std::time::Instant::now();
        app_state
            .add_connection_to_server(
                server_id,
                ConnectionState {
                    id: connection_id,
                    remote_addr,
                    local_addr,
                    bytes_sent: 0,
                    bytes_received: bytes_received as u64,
                    packets_sent: 0,
                    packets_received: 1,
                    last_activity: now,
                    status: ConnectionStatus::Active,
                    status_changed_at: now,
                    protocol_info: ProtocolConnectionInfo::empty(),
                },
            )
            .await;
        debug!("rawip tracked packet from {remote_addr} as {connection_id}");
    }
}
