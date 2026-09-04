//! Pure packet decoding, building and filtering for the TUN/TAP endpoint.
//!
//! **Nothing in this file performs I/O.** That is deliberate: creating a TUN interface needs
//! root on every platform, so the transport can never run in the test suite, while everything
//! that decides *what NetGet says and to whom* lives here and is tested against literal packet
//! bytes. See `src/server/tuntap/CLAUDE.md` for the split and what it does and does not prove.
//!
//! Four things live here:
//!
//! * [`PacketInformation`] — the 4-byte header macOS `utun` prepends to every packet and Linux
//!   does not (unless `IFF_NO_PI` is left unset). This is the single most common source of
//!   "why is every packet off by four bytes", so it is an explicit, named, tested mode rather
//!   than an assumption.
//! * [`decode`] — a frame in, structured header fields out. Never raw bytes: the model is given
//!   `{"protocol": "tcp", "source_port": 443, "tcp_flags": ["syn"]}`, not base64.
//! * [`build_packet`] — the inverse, for the `send_packet` action. The model describes a packet
//!   by its fields and NetGet lays out the headers and computes every checksum.
//! * [`PacketFilter`] — the deterministic gate that decides which packets are even *allowed* to
//!   become an event. This is load-bearing: a per-packet LLM call is hopelessly slow, so the
//!   filter runs first, in native code, on every packet.

use crate::utils::truncate_for_log;
use serde_json::{json, Value};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Bytes of a payload surfaced to the model in `payload_preview`.
///
/// A preview, not the payload: the point of the event is the decoded header, and a model
/// cannot do anything useful with a kilobyte of TLS ciphertext.
pub const PAYLOAD_PREVIEW_BYTES: usize = 64;

/// Ethernet header length (TAP only).
pub const ETHERNET_HEADER_LEN: usize = 14;

/// Length of the platform packet-information header, where one exists.
pub const PACKET_INFORMATION_LEN: usize = 4;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a frame could not be decoded.
///
/// Decode failures are *normal* on a real interface — a truncated read, a protocol NetGet does
/// not model, a link-layer frame that is not IP. They are counted and dropped, never answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The frame was shorter than the header it claims to carry.
    TooShort {
        /// What was being read when the frame ran out.
        what: &'static str,
        /// Bytes needed.
        need: usize,
        /// Bytes available.
        have: usize,
    },
    /// The IP version nibble was neither 4 nor 6.
    UnsupportedIpVersion(u8),
    /// The platform packet-information header did not describe an IP packet.
    BadPacketInformation {
        /// The four bytes that were read.
        header: [u8; 4],
    },
    /// A TAP frame carried an EtherType this endpoint does not decode.
    UnsupportedEtherType(u16),
    /// An IPv4 header declared an IHL below the 20-byte minimum.
    BadIhl(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { what, need, have } => write!(
                f,
                "frame too short for {what}: need {need} bytes, have {have}"
            ),
            Self::UnsupportedIpVersion(v) => write!(f, "unsupported IP version {v}"),
            Self::BadPacketInformation { header } => write!(
                f,
                "packet-information header {:02x?} does not describe an IP packet",
                header
            ),
            Self::UnsupportedEtherType(t) => write!(f, "unsupported EtherType 0x{t:04x}"),
            Self::BadIhl(ihl) => write!(f, "IPv4 IHL {ihl} is below the 5-word minimum"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Why a `send_packet` action could not be turned into bytes.
///
/// Every one of these is a *refusal*, and the caller drops the packet. NetGet never guesses at
/// a field the model left out in a way that would put a different packet on the wire than the
/// model described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError(pub String);

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BuildError {}

fn bad(msg: impl Into<String>) -> BuildError {
    BuildError(msg.into())
}

// ---------------------------------------------------------------------------
// Link mode and packet information
// ---------------------------------------------------------------------------

/// Layer the interface operates at.
///
/// TUN is layer 3 — the frame *is* an IP packet. TAP is layer 2 — the frame is an Ethernet
/// frame with an IP packet inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    /// Layer 3: bare IP packets.
    Tun,
    /// Layer 2: Ethernet frames.
    Tap,
}

impl LinkMode {
    /// Parse the `mode` startup parameter.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tun" | "l3" | "layer3" => Ok(Self::Tun),
            "tap" | "l2" | "layer2" => Ok(Self::Tap),
            other => Err(format!(
                "mode must be \"tun\" (layer 3, bare IP) or \"tap\" (layer 2, Ethernet), got {other:?}"
            )),
        }
    }

    /// The name used in parameters, logs and event data.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tun => "tun",
            Self::Tap => "tap",
        }
    }
}

/// The platform's per-packet prefix.
///
/// # The off-by-four
///
/// macOS `utun` *always* prepends four bytes to every packet in both directions: the address
/// family as a big-endian `u32` (`AF_INET` = 2, `AF_INET6` = 30). Linux `/dev/net/tun` prepends
/// nothing when the device was opened with `IFF_NO_PI`, and a different four bytes when it was
/// not: two bytes of flags followed by the big-endian EtherType (`0x0800` / `0x86DD`).
///
/// Getting this wrong does not fail loudly. The IP version nibble lands in the middle of the
/// prefix, so the packet decodes as garbage rather than erroring, and the symptom is "every
/// field is wrong" rather than "the header is misaligned".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketInformation {
    /// No prefix — Linux with `IFF_NO_PI`, and what the `tun` crate hands us after it has
    /// normalised the platform difference itself.
    None,
    /// macOS `utun`: four bytes of big-endian address family.
    MacOsUtun,
    /// Linux without `IFF_NO_PI`: two bytes of flags, two bytes of big-endian EtherType.
    LinuxTunPi,
}

/// macOS `AF_INET`.
const AF_INET: u32 = 2;
/// macOS `AF_INET6`. (Linux's is 10; this constant is only used for `utun`.)
const AF_INET6_DARWIN: u32 = 30;
/// EtherType for IPv4.
pub const ETHERTYPE_IPV4: u16 = 0x0800;
/// EtherType for IPv6.
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

impl PacketInformation {
    /// Parse the `packet_information` startup parameter.
    ///
    /// `auto` maps to [`PacketInformation::None`]: the `tun` crate strips and re-adds the
    /// platform prefix itself (`posix::Tun::new` sets a read/write `offset` of
    /// [`PACKET_INFORMATION_LEN`] whenever packet information is enabled), so by the time a
    /// frame reaches NetGet it is already a bare packet. The other values exist for a raw fd
    /// handed in from outside that machinery, and for the tests that pin the layout.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "none" => Ok(Self::None),
            "macos_utun" | "macos" | "utun" => Ok(Self::MacOsUtun),
            "linux_pi" | "linux" => Ok(Self::LinuxTunPi),
            other => Err(format!(
                "packet_information must be one of \"auto\", \"none\", \"macos_utun\", \
                 \"linux_pi\", got {other:?}"
            )),
        }
    }

    /// Bytes this mode prepends.
    pub fn header_len(&self) -> usize {
        match self {
            Self::None => 0,
            Self::MacOsUtun | Self::LinuxTunPi => PACKET_INFORMATION_LEN,
        }
    }

    /// Remove the prefix, returning the packet itself.
    ///
    /// The prefix is *validated*, not skipped: a header that does not name IPv4 or IPv6 means
    /// the configured mode does not match the device, and silently advancing four bytes there
    /// is exactly the failure this type exists to prevent.
    pub fn strip<'a>(&self, frame: &'a [u8]) -> Result<&'a [u8], DecodeError> {
        match self {
            Self::None => Ok(frame),
            Self::MacOsUtun | Self::LinuxTunPi => {
                if frame.len() < PACKET_INFORMATION_LEN {
                    return Err(DecodeError::TooShort {
                        what: "packet-information header",
                        need: PACKET_INFORMATION_LEN,
                        have: frame.len(),
                    });
                }
                let mut header = [0u8; 4];
                header.copy_from_slice(&frame[..PACKET_INFORMATION_LEN]);
                let ok = match self {
                    Self::MacOsUtun => {
                        let af = u32::from_be_bytes(header);
                        af == AF_INET || af == AF_INET6_DARWIN
                    }
                    Self::LinuxTunPi => {
                        let proto = u16::from_be_bytes([header[2], header[3]]);
                        proto == ETHERTYPE_IPV4 || proto == ETHERTYPE_IPV6
                    }
                    Self::None => unreachable!(),
                };
                if !ok {
                    return Err(DecodeError::BadPacketInformation { header });
                }
                Ok(&frame[PACKET_INFORMATION_LEN..])
            }
        }
    }

    /// The prefix that must precede an outgoing packet, if any.
    pub fn prefix_for(&self, ipv6: bool) -> Option<[u8; 4]> {
        match self {
            Self::None => None,
            Self::MacOsUtun => Some(if ipv6 {
                AF_INET6_DARWIN.to_be_bytes()
            } else {
                AF_INET.to_be_bytes()
            }),
            Self::LinuxTunPi => {
                let proto = if ipv6 { ETHERTYPE_IPV6 } else { ETHERTYPE_IPV4 };
                let [a, b] = proto.to_be_bytes();
                Some([0, 0, a, b])
            }
        }
    }

    /// Prepend the platform prefix to a freshly built packet, in place.
    pub fn prepend(&self, packet: &mut Vec<u8>) -> Result<(), BuildError> {
        let ipv6 = match packet.first() {
            Some(b) => match b >> 4 {
                4 => false,
                6 => true,
                v => return Err(bad(format!("cannot prefix a packet of IP version {v}"))),
            },
            None => return Err(bad("cannot prefix an empty packet")),
        };
        if let Some(prefix) = self.prefix_for(ipv6) {
            packet.splice(0..0, prefix);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Protocol and ICMP naming
// ---------------------------------------------------------------------------

/// IANA protocol number → name, for the protocols this endpoint can talk about.
///
/// The event carries **both** the name and the number. A model reasons far better about
/// `"tcp"` than about `6`, and the number keeps the event lossless for everything unnamed.
pub fn ip_protocol_name(number: u8) -> &'static str {
    match number {
        0 => "hopopt",
        1 => "icmp",
        2 => "igmp",
        4 => "ipv4",
        6 => "tcp",
        17 => "udp",
        41 => "ipv6",
        43 => "ipv6-route",
        44 => "ipv6-frag",
        47 => "gre",
        50 => "esp",
        51 => "ah",
        58 => "icmpv6",
        59 => "ipv6-nonxt",
        60 => "ipv6-opts",
        89 => "ospf",
        112 => "vrrp",
        132 => "sctp",
        _ => "unknown",
    }
}

/// Name → IANA protocol number, the inverse of [`ip_protocol_name`] for the names a model is
/// likely to write. Returns `None` for a name with no number.
pub fn ip_protocol_number(name: &str) -> Option<u8> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "hopopt" => 0,
        "icmp" => 1,
        "igmp" => 2,
        "tcp" => 6,
        "udp" => 17,
        "gre" => 47,
        "esp" => 50,
        "ah" => 51,
        "icmpv6" | "ipv6-icmp" => 58,
        "ospf" => 89,
        "vrrp" => 112,
        "sctp" => 132,
        _ => return None,
    })
}

/// ICMPv4 type number → name.
pub fn icmpv4_type_name(t: u8) -> &'static str {
    match t {
        0 => "echo_reply",
        3 => "destination_unreachable",
        4 => "source_quench",
        5 => "redirect",
        8 => "echo_request",
        9 => "router_advertisement",
        10 => "router_solicitation",
        11 => "time_exceeded",
        12 => "parameter_problem",
        13 => "timestamp",
        14 => "timestamp_reply",
        _ => "unknown",
    }
}

/// ICMPv6 type number → name.
pub fn icmpv6_type_name(t: u8) -> &'static str {
    match t {
        1 => "destination_unreachable",
        2 => "packet_too_big",
        3 => "time_exceeded",
        4 => "parameter_problem",
        128 => "echo_request",
        129 => "echo_reply",
        133 => "router_solicitation",
        134 => "router_advertisement",
        135 => "neighbor_solicitation",
        136 => "neighbor_advertisement",
        _ => "unknown",
    }
}

/// Name → ICMP type number, per IP version. `None` when the name is not known.
pub fn icmp_type_number(name: &str, ipv6: bool) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase();
    let n = if ipv6 {
        match name.as_str() {
            "destination_unreachable" => 1,
            "packet_too_big" => 2,
            "time_exceeded" => 3,
            "parameter_problem" => 4,
            "echo_request" => 128,
            "echo_reply" => 129,
            "router_solicitation" => 133,
            "router_advertisement" => 134,
            "neighbor_solicitation" => 135,
            "neighbor_advertisement" => 136,
            _ => return None,
        }
    } else {
        match name.as_str() {
            "echo_reply" => 0,
            "destination_unreachable" => 3,
            "source_quench" => 4,
            "redirect" => 5,
            "echo_request" => 8,
            "router_advertisement" => 9,
            "router_solicitation" => 10,
            "time_exceeded" => 11,
            "parameter_problem" => 12,
            "timestamp" => 13,
            "timestamp_reply" => 14,
            _ => return None,
        }
    };
    Some(n)
}

/// The TCP flag bits, low to high, with the names the event and `send_packet` both use.
pub const TCP_FLAG_NAMES: [(u8, &str); 8] = [
    (0x01, "fin"),
    (0x02, "syn"),
    (0x04, "rst"),
    (0x08, "psh"),
    (0x10, "ack"),
    (0x20, "urg"),
    (0x40, "ece"),
    (0x80, "cwr"),
];

/// Expand a TCP flags byte into the names set in it.
pub fn tcp_flag_names(bits: u8) -> Vec<&'static str> {
    TCP_FLAG_NAMES
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// Collapse flag names back into the byte. Unknown names are refused rather than ignored —
/// a typo that silently sends a packet with no flags is worse than a rejection.
pub fn tcp_flag_bits(names: &[String]) -> Result<u8, BuildError> {
    let mut bits = 0u8;
    for name in names {
        let lower = name.trim().to_ascii_lowercase();
        match TCP_FLAG_NAMES.iter().find(|(_, n)| *n == lower) {
            Some((bit, _)) => bits |= bit,
            None => {
                return Err(bad(format!(
                    "unknown TCP flag {name:?}; valid flags are {}",
                    TCP_FLAG_NAMES
                        .iter()
                        .map(|(_, n)| *n)
                        .collect::<Vec<_>>()
                        .join(", ")
                )))
            }
        }
    }
    Ok(bits)
}

// ---------------------------------------------------------------------------
// Decoded representation
// ---------------------------------------------------------------------------

/// The Ethernet header of a TAP frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthernetHeader {
    /// Destination MAC, colon-separated lowercase hex.
    pub destination_mac: String,
    /// Source MAC, colon-separated lowercase hex.
    pub source_mac: String,
    /// EtherType.
    pub ethertype: u16,
}

/// Format six bytes as `aa:bb:cc:dd:ee:ff`.
pub fn format_mac(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Parse `aa:bb:cc:dd:ee:ff` (or the `-` separated form) into six bytes.
pub fn parse_mac(s: &str) -> Result<[u8; 6], BuildError> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(bad(format!(
            "MAC address {s:?} must have six colon-separated octets"
        )));
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16)
            .map_err(|_| bad(format!("MAC address {s:?} has a non-hex octet {p:?}")))?;
    }
    Ok(out)
}

/// Transport-layer detail, as far as this endpoint decodes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// TCP, with the fields a policy decision is actually made on.
    Tcp {
        /// Source port.
        source_port: u16,
        /// Destination port.
        destination_port: u16,
        /// Sequence number.
        seq: u32,
        /// Acknowledgement number.
        ack: u32,
        /// Raw flag bits.
        flags: u8,
        /// Advertised window.
        window: u16,
    },
    /// UDP.
    Udp {
        /// Source port.
        source_port: u16,
        /// Destination port.
        destination_port: u16,
    },
    /// ICMP or ICMPv6.
    Icmp {
        /// Type number.
        icmp_type: u8,
        /// Code.
        code: u8,
        /// Echo identifier, where the type carries one.
        id: Option<u16>,
        /// Echo sequence, where the type carries one.
        sequence: Option<u16>,
    },
    /// Anything else — the IP protocol number is still reported.
    Other,
}

/// A decoded packet: every field the event surfaces, and nothing raw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPacket {
    /// Present only in TAP mode.
    pub ethernet: Option<EthernetHeader>,
    /// 4 or 6.
    pub ip_version: u8,
    /// Source address.
    pub source: IpAddr,
    /// Destination address.
    pub destination: IpAddr,
    /// IANA protocol number of the payload.
    pub protocol: u8,
    /// IPv4 TTL / IPv6 hop limit.
    pub ttl: u8,
    /// Total IP packet length in bytes, as the header declares it.
    pub total_length: usize,
    /// Decoded transport detail.
    pub transport: Transport,
    /// Bytes of transport payload actually present in the frame.
    pub payload_len: usize,
    /// A short preview of the payload, in the encoding named by [`Self::payload_encoding`].
    pub payload_preview: String,
    /// `"utf8"` or `"hex"` — chosen by NetGet and always stated, never left for the reader to
    /// guess. This is the producer declaring what it did, which is the opposite of an executor
    /// sniffing what it was given.
    pub payload_encoding: &'static str,
}

impl DecodedPacket {
    /// The protocol's name, e.g. `"tcp"`.
    pub fn protocol_name(&self) -> &'static str {
        ip_protocol_name(self.protocol)
    }

    /// True when this is IPv6.
    pub fn is_ipv6(&self) -> bool {
        self.ip_version == 6
    }

    /// The ports, when the transport has any.
    pub fn ports(&self) -> Option<(u16, u16)> {
        match self.transport {
            Transport::Tcp {
                source_port,
                destination_port,
                ..
            }
            | Transport::Udp {
                source_port,
                destination_port,
            } => Some((source_port, destination_port)),
            _ => None,
        }
    }

    /// One line a human (or a model) can read at a glance.
    pub fn summary(&self) -> String {
        let base = match &self.transport {
            Transport::Tcp {
                source_port,
                destination_port,
                flags,
                seq,
                ..
            } => format!(
                "{}:{} > {}:{} TCP [{}] seq={}",
                self.source,
                source_port,
                self.destination,
                destination_port,
                tcp_flag_names(*flags).join(","),
                seq
            ),
            Transport::Udp {
                source_port,
                destination_port,
            } => format!(
                "{}:{} > {}:{} UDP",
                self.source, source_port, self.destination, destination_port
            ),
            Transport::Icmp {
                icmp_type,
                code,
                id,
                sequence,
            } => {
                let name = if self.is_ipv6() {
                    icmpv6_type_name(*icmp_type)
                } else {
                    icmpv4_type_name(*icmp_type)
                };
                let mut s = format!(
                    "{} > {} {} {} code={}",
                    self.source,
                    self.destination,
                    if self.is_ipv6() { "ICMPv6" } else { "ICMP" },
                    name,
                    code
                );
                if let (Some(id), Some(seq)) = (id, sequence) {
                    s.push_str(&format!(" id={id} seq={seq}"));
                }
                s
            }
            Transport::Other => format!(
                "{} > {} {}",
                self.source,
                self.destination,
                self.protocol_name()
            ),
        };
        format!("{base} len={}", self.total_length)
    }

    /// The `tuntap_packet_received` event payload.
    ///
    /// Structured header fields only. The payload appears as a short, explicitly-encoded
    /// preview beside its true length — never as the packet's bytes.
    pub fn to_event_data(&self) -> Value {
        let mut data = json!({
            "direction": "inbound",
            "ip_version": self.ip_version,
            "source": self.source.to_string(),
            "destination": self.destination.to_string(),
            "protocol": self.protocol_name(),
            "protocol_number": self.protocol,
            "total_length": self.total_length,
            "payload_length": self.payload_len,
            "payload_preview": self.payload_preview,
            "payload_encoding": self.payload_encoding,
            "summary": self.summary(),
        });
        let obj = data.as_object_mut().expect("json! built an object");

        if self.is_ipv6() {
            obj.insert("hop_limit".into(), json!(self.ttl));
        } else {
            obj.insert("ttl".into(), json!(self.ttl));
        }

        if let Some(eth) = &self.ethernet {
            obj.insert(
                "ethernet".into(),
                json!({
                    "source_mac": eth.source_mac,
                    "destination_mac": eth.destination_mac,
                    "ethertype": format!("0x{:04x}", eth.ethertype),
                }),
            );
        }

        match &self.transport {
            Transport::Tcp {
                source_port,
                destination_port,
                seq,
                ack,
                flags,
                window,
            } => {
                obj.insert("source_port".into(), json!(source_port));
                obj.insert("destination_port".into(), json!(destination_port));
                obj.insert("seq".into(), json!(seq));
                obj.insert("ack".into(), json!(ack));
                obj.insert("window".into(), json!(window));
                obj.insert("tcp_flags".into(), json!(tcp_flag_names(*flags)));
            }
            Transport::Udp {
                source_port,
                destination_port,
            } => {
                obj.insert("source_port".into(), json!(source_port));
                obj.insert("destination_port".into(), json!(destination_port));
            }
            Transport::Icmp {
                icmp_type,
                code,
                id,
                sequence,
            } => {
                let name = if self.is_ipv6() {
                    icmpv6_type_name(*icmp_type)
                } else {
                    icmpv4_type_name(*icmp_type)
                };
                obj.insert("icmp_type".into(), json!(name));
                obj.insert("icmp_type_number".into(), json!(icmp_type));
                obj.insert("icmp_code".into(), json!(code));
                if let Some(id) = id {
                    obj.insert("icmp_id".into(), json!(id));
                }
                if let Some(seq) = sequence {
                    obj.insert("icmp_sequence".into(), json!(seq));
                }
            }
            Transport::Other => {}
        }

        data
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

fn need(what: &'static str, need: usize, have: usize) -> DecodeError {
    DecodeError::TooShort { what, need, have }
}

/// Decode one frame read from the interface.
///
/// `pi` strips (and validates) the platform prefix; `mode` decides whether an Ethernet header
/// is expected in front of the IP packet.
pub fn decode(
    frame: &[u8],
    pi: PacketInformation,
    mode: LinkMode,
) -> Result<DecodedPacket, DecodeError> {
    let frame = pi.strip(frame)?;

    let (ethernet, ip) = match mode {
        LinkMode::Tun => (None, frame),
        LinkMode::Tap => {
            if frame.len() < ETHERNET_HEADER_LEN {
                return Err(need("Ethernet header", ETHERNET_HEADER_LEN, frame.len()));
            }
            let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
            if ethertype != ETHERTYPE_IPV4 && ethertype != ETHERTYPE_IPV6 {
                return Err(DecodeError::UnsupportedEtherType(ethertype));
            }
            (
                Some(EthernetHeader {
                    destination_mac: format_mac(&frame[0..6]),
                    source_mac: format_mac(&frame[6..12]),
                    ethertype,
                }),
                &frame[ETHERNET_HEADER_LEN..],
            )
        }
    };

    if ip.is_empty() {
        return Err(need("IP header", 1, 0));
    }

    match ip[0] >> 4 {
        4 => decode_ipv4(ethernet, ip),
        6 => decode_ipv6(ethernet, ip),
        v => Err(DecodeError::UnsupportedIpVersion(v)),
    }
}

fn decode_ipv4(ethernet: Option<EthernetHeader>, ip: &[u8]) -> Result<DecodedPacket, DecodeError> {
    if ip.len() < 20 {
        return Err(need("IPv4 header", 20, ip.len()));
    }
    let ihl = ip[0] & 0x0f;
    if ihl < 5 {
        return Err(DecodeError::BadIhl(ihl));
    }
    let header_len = ihl as usize * 4;
    if ip.len() < header_len {
        return Err(need("IPv4 options", header_len, ip.len()));
    }
    let total_length = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    let ttl = ip[8];
    let protocol = ip[9];
    let source = IpAddr::V4(ipv4_at(ip, 12));
    let destination = IpAddr::V4(ipv4_at(ip, 16));

    let rest = &ip[header_len..];
    let (transport, payload) = decode_transport(protocol, false, rest)?;

    let (payload_preview, payload_encoding) = preview(payload);

    Ok(DecodedPacket {
        ethernet,
        ip_version: 4,
        source,
        destination,
        protocol,
        ttl,
        total_length,
        transport,
        payload_len: payload.len(),
        payload_preview,
        payload_encoding,
    })
}

fn ipv4_at(buf: &[u8], at: usize) -> Ipv4Addr {
    Ipv4Addr::new(buf[at], buf[at + 1], buf[at + 2], buf[at + 3])
}

fn decode_ipv6(ethernet: Option<EthernetHeader>, ip: &[u8]) -> Result<DecodedPacket, DecodeError> {
    if ip.len() < 40 {
        return Err(need("IPv6 header", 40, ip.len()));
    }
    let payload_length = u16::from_be_bytes([ip[4], ip[5]]) as usize;
    let protocol = ip[6];
    let ttl = ip[7];
    let mut src = [0u8; 16];
    src.copy_from_slice(&ip[8..24]);
    let mut dst = [0u8; 16];
    dst.copy_from_slice(&ip[24..40]);

    let rest = &ip[40..];
    // Extension headers are deliberately not walked: this endpoint reports the *next header* it
    // finds, and a packet carrying extension headers decodes as that protocol with no ports.
    // Pretending to have parsed a chain that was never implemented would be worse than saying so.
    let (transport, payload) = decode_transport(protocol, true, rest)?;
    let (payload_preview, payload_encoding) = preview(payload);

    Ok(DecodedPacket {
        ethernet,
        ip_version: 6,
        source: IpAddr::V6(Ipv6Addr::from(src)),
        destination: IpAddr::V6(Ipv6Addr::from(dst)),
        protocol,
        ttl,
        total_length: 40 + payload_length,
        transport,
        payload_len: payload.len(),
        payload_preview,
        payload_encoding,
    })
}

fn decode_transport(
    protocol: u8,
    ipv6: bool,
    rest: &[u8],
) -> Result<(Transport, &[u8]), DecodeError> {
    match protocol {
        6 => {
            if rest.len() < 20 {
                return Err(need("TCP header", 20, rest.len()));
            }
            let data_offset = (rest[12] >> 4) as usize * 4;
            let header_len = data_offset.max(20).min(rest.len());
            Ok((
                Transport::Tcp {
                    source_port: u16::from_be_bytes([rest[0], rest[1]]),
                    destination_port: u16::from_be_bytes([rest[2], rest[3]]),
                    seq: u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]),
                    ack: u32::from_be_bytes([rest[8], rest[9], rest[10], rest[11]]),
                    flags: rest[13],
                    window: u16::from_be_bytes([rest[14], rest[15]]),
                },
                &rest[header_len..],
            ))
        }
        17 => {
            if rest.len() < 8 {
                return Err(need("UDP header", 8, rest.len()));
            }
            Ok((
                Transport::Udp {
                    source_port: u16::from_be_bytes([rest[0], rest[1]]),
                    destination_port: u16::from_be_bytes([rest[2], rest[3]]),
                },
                &rest[8..],
            ))
        }
        1 | 58 => {
            if rest.len() < 4 {
                return Err(need("ICMP header", 4, rest.len()));
            }
            let icmp_type = rest[0];
            let code = rest[1];
            let echo = if ipv6 {
                matches!(icmp_type, 128 | 129)
            } else {
                matches!(icmp_type, 0 | 8 | 13 | 14)
            };
            let (id, sequence, body_at) = if echo && rest.len() >= 8 {
                (
                    Some(u16::from_be_bytes([rest[4], rest[5]])),
                    Some(u16::from_be_bytes([rest[6], rest[7]])),
                    8,
                )
            } else {
                (None, None, 4.min(rest.len()))
            };
            Ok((
                Transport::Icmp {
                    icmp_type,
                    code,
                    id,
                    sequence,
                },
                &rest[body_at..],
            ))
        }
        _ => Ok((Transport::Other, rest)),
    }
}

/// Build the payload preview and name the encoding used.
///
/// A payload that is entirely printable ASCII is shown as text, because that is what makes an
/// HTTP request or a DNS-over-TCP name legible to a model; anything else is hex. **NetGet
/// decides and then states the answer** in `payload_encoding`, so no reader has to guess and
/// no executor has to sniff.
fn preview(payload: &[u8]) -> (String, &'static str) {
    let head = &payload[..payload.len().min(PAYLOAD_PREVIEW_BYTES)];
    let printable = !head.is_empty()
        && head
            .iter()
            .all(|b| matches!(b, 0x20..=0x7e | b'\t' | b'\r' | b'\n'));
    if printable {
        let text: String = head.iter().map(|b| *b as char).collect();
        // truncate_for_log rather than slicing: byte-index truncation on a multi-byte boundary
        // has panicked this codebase before, and the helper is the project-wide answer.
        (truncate_for_log(&text, PAYLOAD_PREVIEW_BYTES), "utf8")
    } else {
        (
            truncate_for_log(&hex::encode(head), PAYLOAD_PREVIEW_BYTES * 2),
            "hex",
        )
    }
}

// ---------------------------------------------------------------------------
// The escalation filter
// ---------------------------------------------------------------------------

/// One condition a packet must satisfy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Term {
    V4,
    V6,
    Protocol(u8),
    PortAny(u16),
    ProtocolPort(u8, u16),
    TcpSyn,
    From(IpAddr),
    To(IpAddr),
    Host(IpAddr),
}

impl Term {
    fn matches(&self, p: &DecodedPacket) -> bool {
        match self {
            Self::V4 => p.ip_version == 4,
            Self::V6 => p.ip_version == 6,
            Self::Protocol(n) => p.protocol == *n,
            Self::PortAny(port) => p.ports().is_some_and(|(s, d)| s == *port || d == *port),
            Self::ProtocolPort(n, port) => {
                p.protocol == *n && p.ports().is_some_and(|(s, d)| s == *port || d == *port)
            }
            Self::TcpSyn => matches!(
                p.transport,
                Transport::Tcp { flags, .. } if flags & 0x02 != 0 && flags & 0x10 == 0
            ),
            Self::From(addr) => p.source == *addr,
            Self::To(addr) => p.destination == *addr,
            Self::Host(addr) => p.source == *addr || p.destination == *addr,
        }
    }
}

/// The deterministic gate in front of everything else.
///
/// **This is the design's load-bearing part.** Every packet routed to the interface is decoded
/// in native code and tested against this filter. Only a packet that matches is allowed to
/// become an event at all — and only then can a handler, or in the last resort the model, be
/// consulted. Without it, one `ping` is one LLM call per second and a TCP handshake is three in
/// a few milliseconds.
///
/// # Grammar
///
/// * `all` — every decodable packet.
/// * `none` — nothing. The interface still runs; every packet is counted and dropped.
/// * otherwise: comma-separated alternatives, ORed. Each alternative is one or more terms
///   joined by `+`, ANDed:
///   - `v4`, `v6`
///   - a protocol name (`icmp`, `icmpv6`, `tcp`, `udp`, `gre`, `esp`, `ospf`, …) or
///     `ip-proto-<n>`
///   - `tcp:<port>` / `udp:<port>` — either endpoint carries that port
///   - `port:<n>` — either endpoint, either transport
///   - `tcp-syn` — a TCP segment with SYN set and ACK clear. **The most useful term in the
///     language**: it fires once per connection attempt instead of once per packet, so a whole
///     TCP flow costs one event.
///   - `from:<addr>`, `to:<addr>`, `host:<addr>`
///
/// `tcp-syn+to:10.7.0.2, icmp` reads as "a new connection to 10.7.0.2, or any ping".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketFilter {
    /// The original expression, kept for logs and for `metadata()`-style reporting.
    source: String,
    /// `None` means "match everything"; an empty Vec means "match nothing".
    alternatives: Option<Vec<Vec<Term>>>,
}

impl PacketFilter {
    /// Parse a filter expression. See the type documentation for the grammar.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let source = expr.trim().to_string();
        let lower = source.to_ascii_lowercase();
        if lower.is_empty() {
            return Err(
                "packet_filter must not be empty; use \"none\" to escalate nothing, \
                        or \"all\" to escalate everything"
                    .to_string(),
            );
        }
        if lower == "all" {
            return Ok(Self {
                source,
                alternatives: None,
            });
        }
        if lower == "none" {
            return Ok(Self {
                source,
                alternatives: Some(Vec::new()),
            });
        }

        let mut alternatives = Vec::new();
        for group in lower.split(',') {
            let group = group.trim();
            if group.is_empty() {
                return Err(format!(
                    "packet_filter {expr:?} has an empty alternative between commas"
                ));
            }
            let mut terms = Vec::new();
            for term in group.split('+') {
                terms.push(parse_term(term.trim(), expr)?);
            }
            alternatives.push(terms);
        }
        Ok(Self {
            source,
            alternatives: Some(alternatives),
        })
    }

    /// The expression as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// True when this filter can never match, so nothing will ever escalate.
    pub fn matches_nothing(&self) -> bool {
        matches!(&self.alternatives, Some(v) if v.is_empty())
    }

    /// Does this packet get to become an event?
    pub fn matches(&self, packet: &DecodedPacket) -> bool {
        match &self.alternatives {
            None => true,
            Some(alts) => alts
                .iter()
                .any(|terms| terms.iter().all(|t| t.matches(packet))),
        }
    }
}

fn parse_term(term: &str, whole: &str) -> Result<Term, String> {
    let err = |detail: String| format!("packet_filter {whole:?}: {detail}");

    if term.is_empty() {
        return Err(err("empty term between '+' separators".into()));
    }
    match term {
        "v4" | "ipv4" => return Ok(Term::V4),
        "v6" | "ipv6" => return Ok(Term::V6),
        "tcp-syn" | "tcp_syn" => return Ok(Term::TcpSyn),
        _ => {}
    }
    if let Some(rest) = term.strip_prefix("ip-proto-") {
        let n: u8 = rest
            .parse()
            .map_err(|_| err(format!("{rest:?} is not an IP protocol number (0-255)")))?;
        return Ok(Term::Protocol(n));
    }
    for (prefix, ctor) in [
        ("from:", Term::From as fn(IpAddr) -> Term),
        ("to:", Term::To as fn(IpAddr) -> Term),
        ("host:", Term::Host as fn(IpAddr) -> Term),
    ] {
        if let Some(rest) = term.strip_prefix(prefix) {
            let addr: IpAddr = rest
                .parse()
                .map_err(|_| err(format!("{rest:?} is not an IP address")))?;
            return Ok(ctor(addr));
        }
    }
    if let Some(rest) = term.strip_prefix("port:") {
        let port: u16 = rest
            .parse()
            .map_err(|_| err(format!("{rest:?} is not a port number (0-65535)")))?;
        return Ok(Term::PortAny(port));
    }
    if let Some((name, port)) = term.split_once(':') {
        let number = ip_protocol_number(name)
            .ok_or_else(|| err(format!("{name:?} is not a known protocol name")))?;
        if number != 6 && number != 17 {
            return Err(err(format!("{name:?} has no ports to match on")));
        }
        let port: u16 = port
            .parse()
            .map_err(|_| err(format!("{port:?} is not a port number (0-65535)")))?;
        return Ok(Term::ProtocolPort(number, port));
    }
    match ip_protocol_number(term) {
        Some(n) => Ok(Term::Protocol(n)),
        None => Err(err(format!(
            "{term:?} is not a filter term; expected all, none, v4, v6, tcp-syn, a protocol \
             name, ip-proto-<n>, tcp:<port>, udp:<port>, port:<n>, from:<addr>, to:<addr> or \
             host:<addr>"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

/// The one's-complement sum used by IPv4, ICMP, TCP and UDP.
pub fn checksum16(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = bytes.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn pseudo_header(source: IpAddr, destination: IpAddr, protocol: u8, length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    match (source, destination) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            out.extend_from_slice(&s.octets());
            out.extend_from_slice(&d.octets());
            out.push(0);
            out.push(protocol);
            out.extend_from_slice(&(length as u16).to_be_bytes());
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            out.extend_from_slice(&s.octets());
            out.extend_from_slice(&d.octets());
            out.extend_from_slice(&(length as u32).to_be_bytes());
            out.extend_from_slice(&[0, 0, 0, protocol]);
        }
        _ => unreachable!("caller checked the address families match"),
    }
    out
}

fn get_str<'a>(action: &'a Value, key: &str) -> Option<&'a str> {
    action.get(key).and_then(|v| v.as_str())
}

fn get_u64(action: &Value, key: &str) -> Option<u64> {
    action.get(key).and_then(|v| v.as_u64())
}

fn required_addr(action: &Value, key: &str) -> Result<IpAddr, BuildError> {
    let raw = get_str(action, key).ok_or_else(|| bad(format!("send_packet requires \"{key}\"")))?;
    raw.parse()
        .map_err(|_| bad(format!("\"{key}\" is not an IP address: {raw:?}")))
}

/// Decode the `payload` field according to the explicit `payload_encoding` field.
///
/// **The encoding is never sniffed.** `"48656c6c6f"` is simultaneously valid text and valid
/// hex, and only the sender knows which it meant — the exact bug `CLAUDE.md` records against
/// `send_tcp_data`. The field defaults to `utf8`, and hex is decoded strictly (even length,
/// hex digits only) so a malformed value is a refusal rather than a different packet.
pub fn decode_payload(action: &Value) -> Result<Vec<u8>, BuildError> {
    let Some(payload) = action.get("payload") else {
        return Ok(Vec::new());
    };
    if payload.is_null() {
        return Ok(Vec::new());
    }
    let text = payload
        .as_str()
        .ok_or_else(|| bad("\"payload\" must be a string"))?;
    let encoding = get_str(action, "payload_encoding").unwrap_or("utf8");
    match encoding.trim().to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" | "text" => Ok(text.as_bytes().to_vec()),
        "hex" => hex::decode(text.trim()).map_err(|e| {
            bad(format!(
                "\"payload\" is not valid hex ({e}); payload_encoding said \"hex\""
            ))
        }),
        other => Err(bad(format!(
            "payload_encoding must be \"utf8\" or \"hex\", got {other:?}"
        ))),
    }
}

/// Build the bytes for a `send_packet` action.
///
/// The model describes the packet by its fields; NetGet lays out the headers and computes the
/// IPv4 header checksum, the ICMP/ICMPv6 checksum and the TCP/UDP checksum over the correct
/// pseudo-header. When `source_mac` and `destination_mac` are both present the result is an
/// Ethernet frame (TAP); otherwise it is a bare IP packet (TUN).
pub fn build_packet(action: &Value) -> Result<Vec<u8>, BuildError> {
    let source = required_addr(action, "source")?;
    let destination = required_addr(action, "destination")?;

    let declared_version = get_u64(action, "ip_version");
    let ipv6 = match (source, destination) {
        (IpAddr::V4(_), IpAddr::V4(_)) => false,
        (IpAddr::V6(_), IpAddr::V6(_)) => true,
        _ => {
            return Err(bad(
                "\"source\" and \"destination\" must be the same IP version",
            ))
        }
    };
    if let Some(v) = declared_version {
        let expected = if ipv6 { 6 } else { 4 };
        if v != expected {
            return Err(bad(format!(
                "ip_version says {v} but the addresses are IPv{expected}"
            )));
        }
    }

    let protocol = match action.get("protocol") {
        None => return Err(bad("send_packet requires \"protocol\"")),
        Some(Value::String(s)) => ip_protocol_number(s).ok_or_else(|| {
            bad(format!(
                "unknown protocol {s:?}; use a name such as \"icmp\", \"icmpv6\", \"tcp\", \
                 \"udp\", or a number"
            ))
        })?,
        Some(Value::Number(n)) => {
            let v = n
                .as_u64()
                .ok_or_else(|| bad("\"protocol\" must be a non-negative number"))?;
            u8::try_from(v).map_err(|_| bad("\"protocol\" must be 0-255"))?
        }
        Some(other) => {
            return Err(bad(format!(
                "\"protocol\" must be a name or a number, got {other}"
            )))
        }
    };

    if ipv6 && protocol == 1 {
        return Err(bad(
            "protocol \"icmp\" is IPv4-only; use \"icmpv6\" with IPv6 addresses",
        ));
    }
    if !ipv6 && protocol == 58 {
        return Err(bad(
            "protocol \"icmpv6\" is IPv6-only; use \"icmp\" with IPv4 addresses",
        ));
    }

    let payload = decode_payload(action)?;

    let transport = match protocol {
        1 | 58 => build_icmp(action, ipv6, &payload)?,
        6 => build_tcp(action, &payload)?,
        17 => build_udp(action, &payload)?,
        _ => payload.clone(),
    };

    let mut transport = transport;
    // Checksums that need the addresses live here, where both are known.
    match protocol {
        58 => {
            let ph = pseudo_header(source, destination, 58, transport.len());
            let mut all = ph;
            all.extend_from_slice(&transport);
            let sum = checksum16(&all);
            transport[2..4].copy_from_slice(&sum.to_be_bytes());
        }
        6 | 17 => {
            let ph = pseudo_header(source, destination, protocol, transport.len());
            let mut all = ph;
            all.extend_from_slice(&transport);
            let sum = checksum16(&all);
            let at = if protocol == 6 { 16 } else { 6 };
            transport[at..at + 2].copy_from_slice(&sum.to_be_bytes());
        }
        _ => {}
    }

    let ttl = match (get_u64(action, "ttl"), get_u64(action, "hop_limit")) {
        (Some(v), _) | (None, Some(v)) => {
            u8::try_from(v).map_err(|_| bad("\"ttl\"/\"hop_limit\" must be 0-255"))?
        }
        (None, None) => 64,
    };

    let mut packet = if ipv6 {
        build_ipv6_header(source, destination, protocol, ttl, transport.len())
    } else {
        build_ipv4_header(action, source, destination, protocol, ttl, transport.len())?
    };
    packet.extend_from_slice(&transport);

    // TAP: wrap in an Ethernet header when the model supplied both MACs. The engine, which is
    // the only thing that knows the interface's layer, refuses the mismatched combinations —
    // this function is deliberately mode-agnostic so its examples are executable anywhere.
    match (
        get_str(action, "source_mac"),
        get_str(action, "destination_mac"),
    ) {
        (Some(src), Some(dst)) => {
            let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + packet.len());
            frame.extend_from_slice(&parse_mac(dst)?);
            frame.extend_from_slice(&parse_mac(src)?);
            frame.extend_from_slice(
                &(if ipv6 { ETHERTYPE_IPV6 } else { ETHERTYPE_IPV4 }).to_be_bytes(),
            );
            frame.extend_from_slice(&packet);
            Ok(frame)
        }
        (None, None) => Ok(packet),
        _ => Err(bad(
            "an Ethernet frame needs both \"source_mac\" and \"destination_mac\"; supply \
             neither for a layer-3 (TUN) packet",
        )),
    }
}

fn build_ipv4_header(
    action: &Value,
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    ttl: u8,
    payload_len: usize,
) -> Result<Vec<u8>, BuildError> {
    let (IpAddr::V4(s), IpAddr::V4(d)) = (source, destination) else {
        unreachable!("caller checked the family");
    };
    let total = 20 + payload_len;
    if total > u16::MAX as usize {
        return Err(bad(format!(
            "packet is {total} bytes, larger than IPv4 allows"
        )));
    }
    let identification = get_u64(action, "identification")
        .map(|v| u16::try_from(v).map_err(|_| bad("\"identification\" must be 0-65535")))
        .transpose()?
        .unwrap_or(0);

    let mut h = Vec::with_capacity(20);
    h.push(0x45); // version 4, IHL 5 — no options are ever emitted
    h.push(0); // DSCP/ECN
    h.extend_from_slice(&(total as u16).to_be_bytes());
    h.extend_from_slice(&identification.to_be_bytes());
    h.extend_from_slice(&0x4000u16.to_be_bytes()); // Don't Fragment, offset 0
    h.push(ttl);
    h.push(protocol);
    h.extend_from_slice(&[0, 0]); // checksum placeholder
    h.extend_from_slice(&s.octets());
    h.extend_from_slice(&d.octets());
    let sum = checksum16(&h);
    h[10..12].copy_from_slice(&sum.to_be_bytes());
    Ok(h)
}

fn build_ipv6_header(
    source: IpAddr,
    destination: IpAddr,
    protocol: u8,
    hop_limit: u8,
    payload_len: usize,
) -> Vec<u8> {
    let (IpAddr::V6(s), IpAddr::V6(d)) = (source, destination) else {
        unreachable!("caller checked the family");
    };
    let mut h = Vec::with_capacity(40);
    h.extend_from_slice(&0x6000_0000u32.to_be_bytes()); // version 6, no traffic class or label
    h.extend_from_slice(&(payload_len as u16).to_be_bytes());
    h.push(protocol);
    h.push(hop_limit);
    h.extend_from_slice(&s.octets());
    h.extend_from_slice(&d.octets());
    h
}

fn icmp_type_of(action: &Value, ipv6: bool) -> Result<u8, BuildError> {
    match action.get("icmp_type") {
        None => Err(bad(
            "an ICMP packet requires \"icmp_type\" (a name such as \"echo_reply\", or a number)",
        )),
        Some(Value::String(s)) => icmp_type_number(s, ipv6).ok_or_else(|| {
            bad(format!(
                "unknown ICMP{} type {s:?}",
                if ipv6 { "v6" } else { "" }
            ))
        }),
        Some(Value::Number(n)) => {
            let v = n
                .as_u64()
                .ok_or_else(|| bad("\"icmp_type\" must be a non-negative number"))?;
            u8::try_from(v).map_err(|_| bad("\"icmp_type\" must be 0-255"))
        }
        Some(other) => Err(bad(format!(
            "\"icmp_type\" must be a name or a number, got {other}"
        ))),
    }
}

fn build_icmp(action: &Value, ipv6: bool, payload: &[u8]) -> Result<Vec<u8>, BuildError> {
    let icmp_type = icmp_type_of(action, ipv6)?;
    let code = get_u64(action, "icmp_code")
        .map(|v| u8::try_from(v).map_err(|_| bad("\"icmp_code\" must be 0-255")))
        .transpose()?
        .unwrap_or(0);

    let echo = if ipv6 {
        matches!(icmp_type, 128 | 129)
    } else {
        matches!(icmp_type, 0 | 8)
    };

    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.push(icmp_type);
    msg.push(code);
    msg.extend_from_slice(&[0, 0]); // checksum placeholder

    if echo {
        let id = get_u64(action, "icmp_id")
            .map(|v| u16::try_from(v).map_err(|_| bad("\"icmp_id\" must be 0-65535")))
            .transpose()?
            .unwrap_or(0);
        let seq = get_u64(action, "icmp_sequence")
            .map(|v| u16::try_from(v).map_err(|_| bad("\"icmp_sequence\" must be 0-65535")))
            .transpose()?
            .unwrap_or(0);
        msg.extend_from_slice(&id.to_be_bytes());
        msg.extend_from_slice(&seq.to_be_bytes());
    } else {
        msg.extend_from_slice(&[0, 0, 0, 0]); // unused / MTU / pointer, per type
    }
    msg.extend_from_slice(payload);

    if !ipv6 {
        // ICMPv4 has no pseudo-header, so the checksum is final here. ICMPv6 needs the
        // addresses and is finished by the caller.
        let sum = checksum16(&msg);
        msg[2..4].copy_from_slice(&sum.to_be_bytes());
    }
    Ok(msg)
}

fn port(action: &Value, key: &str) -> Result<u16, BuildError> {
    let v = get_u64(action, key)
        .ok_or_else(|| bad(format!("this packet requires \"{key}\" (0-65535)")))?;
    u16::try_from(v).map_err(|_| bad(format!("\"{key}\" must be 0-65535")))
}

fn build_udp(action: &Value, payload: &[u8]) -> Result<Vec<u8>, BuildError> {
    let mut msg = Vec::with_capacity(8 + payload.len());
    msg.extend_from_slice(&port(action, "source_port")?.to_be_bytes());
    msg.extend_from_slice(&port(action, "destination_port")?.to_be_bytes());
    msg.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    msg.extend_from_slice(&[0, 0]); // checksum placeholder — filled with the pseudo-header
    msg.extend_from_slice(payload);
    Ok(msg)
}

fn build_tcp(action: &Value, payload: &[u8]) -> Result<Vec<u8>, BuildError> {
    let flags = match action.get("flags") {
        None => 0x10, // a bare ACK is the only defensible default
        Some(Value::Array(items)) => {
            let names: Vec<String> = items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| bad("\"flags\" entries must be strings"))
                })
                .collect::<Result<_, _>>()?;
            tcp_flag_bits(&names)?
        }
        Some(other) => {
            return Err(bad(format!(
                "\"flags\" must be an array of flag names, got {other}"
            )))
        }
    };
    let seq = get_u64(action, "seq").unwrap_or(0);
    let ack = get_u64(action, "ack").unwrap_or(0);
    let window = get_u64(action, "window")
        .map(|v| u16::try_from(v).map_err(|_| bad("\"window\" must be 0-65535")))
        .transpose()?
        .unwrap_or(65535);

    let mut msg = Vec::with_capacity(20 + payload.len());
    msg.extend_from_slice(&port(action, "source_port")?.to_be_bytes());
    msg.extend_from_slice(&port(action, "destination_port")?.to_be_bytes());
    msg.extend_from_slice(&(seq as u32).to_be_bytes());
    msg.extend_from_slice(&(ack as u32).to_be_bytes());
    msg.push(5 << 4); // data offset 5 words, no options
    msg.push(flags);
    msg.extend_from_slice(&window.to_be_bytes());
    msg.extend_from_slice(&[0, 0]); // checksum placeholder
    msg.extend_from_slice(&[0, 0]); // urgent pointer
    msg.extend_from_slice(payload);
    Ok(msg)
}
