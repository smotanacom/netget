//! Pure ICMPv6 Neighbour Discovery (RFC 4861) codec.
//!
//! Everything in this file is a **pure function over plain values**: it opens no socket, reads no
//! configuration and touches no global state. That is the whole design. NDP's real transport is a
//! raw ICMPv6 socket, which needs root or `CAP_NET_RAW`, and nothing in this repository has that —
//! so the transport can never be executed here, but the packet format can be, and is, checked
//! against literal bytes in `tests/server/ndp/codec_test.rs`.
//!
//! This is the `bluetooth_ble_beacon` split the root `CLAUDE.md` describes: message construction
//! is pure and exhaustively tested; the platform transport is a thin layer over it that has never
//! run. `metadata().notes` says both.
//!
//! # Message layouts (RFC 4861 §4)
//!
//! Every one starts with the ICMPv6 header of RFC 4443 §2.1 — type, code, checksum — and every
//! one of the five NDP types uses code 0. Options, when present, always follow.
//!
//! ```text
//! Router Solicitation  (133)  type code cksum | reserved(4)                          | options
//! Router Advertisement (134)  type code cksum | hop(1) flags(1) lifetime(2)
//!                                             | reachable(4) | retrans(4)            | options
//! Neighbour Solicit.   (135)  type code cksum | reserved(4) | target(16)             | options
//! Neighbour Advert.    (136)  type code cksum | flags(4)    | target(16)             | options
//! Redirect             (137)  type code cksum | reserved(4) | target(16) | dest(16)  | options
//! ```
//!
//! # Two things implementations get wrong, both pinned by literal-byte tests
//!
//! 1. **Option length is in units of 8 octets, not octets** (RFC 4861 §4.6). A Prefix Information
//!    option is 32 octets on the wire and its length field reads `4`. Writing `32` there produces
//!    an option a real stack walks straight past the end of, and it is the classic NDP bug.
//! 2. **The ICMPv6 checksum covers an IPv6 pseudo-header** (RFC 4443 §2.3, RFC 8200 §8.1) — the
//!    source address, the destination address, the upper-layer length and next header 58. It is
//!    therefore *not computable from the ICMPv6 bytes alone*, which is why every encode entry
//!    point on this type demands both addresses. A codec that computes it over the message only
//!    produces packets every conforming receiver silently drops.
//!
//! # No bytes cross the LLM boundary
//!
//! Addresses go in and out as IPv6 strings, link-layer addresses as `"00:11:22:33:44:55"`,
//! flags as booleans, lifetimes as seconds. There is no way to hand this codec a hex blob and
//! there is deliberately no action parameter that would accept one.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::net::Ipv6Addr;

// ---------------------------------------------------------------------------------------------
// Wire constants
// ---------------------------------------------------------------------------------------------

/// IPv6 "Next Header" value for ICMPv6 (RFC 4443 §1). The pseudo-header carries this, not 0.
pub const NEXT_HEADER_ICMPV6: u8 = 58;

pub const TYPE_ROUTER_SOLICITATION: u8 = 133;
pub const TYPE_ROUTER_ADVERTISEMENT: u8 = 134;
pub const TYPE_NEIGHBOR_SOLICITATION: u8 = 135;
pub const TYPE_NEIGHBOR_ADVERTISEMENT: u8 = 136;
pub const TYPE_REDIRECT: u8 = 137;

/// RFC 4861 §4: all five NDP messages use ICMPv6 code 0, and a receiver discards anything else.
pub const NDP_CODE: u8 = 0;

/// Option types (RFC 4861 §4.6, RFC 8106 §5.1 for RDNSS).
pub const OPT_SOURCE_LINK_LAYER_ADDRESS: u8 = 1;
pub const OPT_TARGET_LINK_LAYER_ADDRESS: u8 = 2;
pub const OPT_PREFIX_INFORMATION: u8 = 3;
pub const OPT_MTU: u8 = 5;
pub const OPT_RDNSS: u8 = 25;

/// Prefix Information flags (RFC 4861 §4.6.2).
pub const PREFIX_FLAG_ON_LINK: u8 = 0x80;
pub const PREFIX_FLAG_AUTONOMOUS: u8 = 0x40;

/// Router Advertisement flags (RFC 4861 §4.2).
pub const RA_FLAG_MANAGED: u8 = 0x80;
pub const RA_FLAG_OTHER: u8 = 0x40;

/// Neighbour Advertisement flags (RFC 4861 §4.4) — the top three bits of a 32-bit field.
pub const NA_FLAG_ROUTER: u32 = 0x8000_0000;
pub const NA_FLAG_SOLICITED: u32 = 0x4000_0000;
pub const NA_FLAG_OVERRIDE: u32 = 0x2000_0000;

/// `ff02::1`, the all-nodes link-local multicast group: where an unsolicited Router
/// Advertisement goes.
pub const ALL_NODES_MULTICAST: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
/// `ff02::2`, the all-routers link-local multicast group: where a Router Solicitation goes.
pub const ALL_ROUTERS_MULTICAST: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 2);

/// RFC 4861 §11.2: every NDP message is sent with an IPv6 Hop Limit of 255 and a receiver
/// discards one that arrives with anything less. That is the protocol's entire defence against
/// an off-link attacker — it cannot be forged, because a router decrements.
pub const NDP_HOP_LIMIT: u32 = 255;

/// A lifetime of `0xffffffff` means "for ever" in a Prefix Information option and in RDNSS.
pub const LIFETIME_INFINITE: u32 = 0xffff_ffff;

// ---------------------------------------------------------------------------------------------
// Link-layer addresses
// ---------------------------------------------------------------------------------------------

/// Parse `aa:bb:cc:dd:ee:ff` (also accepting `-`, `.` and space separators) into six octets.
pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    if cleaned.len() != 12 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "'{text}' is not a link-layer address (expected 6 hex octets, e.g. 00:11:22:33:44:55)"
        );
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("'{text}' is not a link-layer address"))?;
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

/// The solicited-node multicast address for a target (RFC 4291 §2.7.1):
/// `ff02::1:ff00:0/104` with the target's low 24 bits appended.
///
/// This is where a Neighbour Solicitation for an address one does not yet know is sent, and it
/// is the reason NDP does not need broadcast the way ARP does.
pub fn solicited_node_multicast(target: Ipv6Addr) -> Ipv6Addr {
    let o = target.octets();
    Ipv6Addr::from([
        0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xff, o[13], o[14], o[15],
    ])
}

// ---------------------------------------------------------------------------------------------
// The checksum, and its pseudo-header
// ---------------------------------------------------------------------------------------------

/// The ICMPv6 checksum over the IPv6 pseudo-header of RFC 8200 §8.1 plus `message`.
///
/// The pseudo-header is 40 octets and is **never transmitted**:
///
/// ```text
/// | source address (16) | destination address (16) | upper-layer length (4) | zero(3) | 58 |
/// ```
///
/// `message` must already carry a zeroed checksum field (octets 2..4); this function does not
/// mutate it. The result is the one's-complement of the one's-complement sum, in host order —
/// [`write_checksum`] puts it on the wire.
///
/// **This is the single thing most ICMPv6 implementations get wrong**, in one of two ways:
/// omitting the pseudo-header entirely (a checksum that is correct for the payload and wrong for
/// every packet), or using next header 0 instead of 58 because the IPv6 header the packet is
/// eventually wrapped in says something else. Both produce packets a conforming receiver drops
/// without a word.
pub fn icmpv6_checksum(source: Ipv6Addr, destination: Ipv6Addr, message: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    let mut add_pair = |hi: u8, lo: u8| {
        sum += ((hi as u32) << 8) | lo as u32;
    };

    for chunk in source.octets().chunks(2) {
        add_pair(chunk[0], chunk[1]);
    }
    for chunk in destination.octets().chunks(2) {
        add_pair(chunk[0], chunk[1]);
    }

    // Upper-layer packet length, 32 bits. NDP messages are far below 64KiB, but the field is
    // four octets and both halves are summed.
    let length = message.len() as u32;
    add_pair((length >> 24) as u8, (length >> 16) as u8);
    add_pair((length >> 8) as u8, length as u8);

    // Three octets of zero, then the next header. The zeros contribute nothing; the 58 lands in
    // the low half of the last 16-bit word.
    add_pair(0, NEXT_HEADER_ICMPV6);

    let mut chunks = message.chunks_exact(2);
    for chunk in chunks.by_ref() {
        add_pair(chunk[0], chunk[1]);
    }
    // An odd trailing octet is padded on the right with zero (RFC 1071 §1).
    if let [last] = chunks.remainder() {
        add_pair(*last, 0);
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Compute the checksum for `message` and write it into its header.
///
/// Zeroes the field first, so this is safe to call on a message that already carries one.
pub fn write_checksum(message: &mut [u8], source: Ipv6Addr, destination: Ipv6Addr) -> Result<u16> {
    if message.len() < 4 {
        bail!(
            "an ICMPv6 message is at least 4 octets (type, code, checksum); got {}",
            message.len()
        );
    }
    message[2] = 0;
    message[3] = 0;
    let checksum = icmpv6_checksum(source, destination, message);
    message[2..4].copy_from_slice(&checksum.to_be_bytes());
    Ok(checksum)
}

/// Verify the checksum a message carries against the addresses it was sent between.
///
/// Separate from [`decode`] deliberately: on a raw ICMPv6 socket the kernel has already verified
/// it and does not report the destination address without `IPV6_RECVPKTINFO`, so the transport
/// there cannot call this. The UDP test transport carries both addresses and does.
pub fn verify_checksum(message: &[u8], source: Ipv6Addr, destination: Ipv6Addr) -> Result<()> {
    if message.len() < 4 {
        bail!(
            "ICMPv6 message is {} octets, too short to check",
            message.len()
        );
    }
    let carried = u16::from_be_bytes([message[2], message[3]]);
    let mut zeroed = message.to_vec();
    zeroed[2] = 0;
    zeroed[3] = 0;
    let expected = icmpv6_checksum(source, destination, &zeroed);
    if carried != expected {
        bail!(
            "ICMPv6 checksum is 0x{carried:04x} but the pseudo-header over {source} -> \
             {destination} gives 0x{expected:04x}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

/// A Prefix Information option's contents (RFC 4861 §4.6.2).
///
/// This is the option that hands a host its address and its on-link determination, and the two
/// flags are the ones that decide how much: `on_link` says "you can reach this prefix without a
/// router", `autonomous` says "build yourself an address out of it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixInformation {
    pub prefix: Ipv6Addr,
    pub prefix_length: u8,
    pub on_link: bool,
    pub autonomous: bool,
    pub valid_lifetime: u32,
    pub preferred_lifetime: u32,
}

/// One Neighbour Discovery option.
///
/// [`NdpOption::Other`] deliberately keeps only the type and the declared length: an option this
/// codec does not model still has to be walked over correctly, but putting its octets into event
/// data would break the no-bytes-to-the-model rule for no benefit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NdpOption {
    SourceLinkLayerAddress([u8; 6]),
    TargetLinkLayerAddress([u8; 6]),
    PrefixInformation(PrefixInformation),
    Mtu(u32),
    /// RFC 8106 §5.1. One option carries a lifetime and one or more resolver addresses.
    Rdnss {
        lifetime: u32,
        servers: Vec<Ipv6Addr>,
    },
    Other {
        option_type: u8,
        length_units: u8,
    },
}

impl NdpOption {
    pub fn option_type(&self) -> u8 {
        match self {
            NdpOption::SourceLinkLayerAddress(_) => OPT_SOURCE_LINK_LAYER_ADDRESS,
            NdpOption::TargetLinkLayerAddress(_) => OPT_TARGET_LINK_LAYER_ADDRESS,
            NdpOption::PrefixInformation(_) => OPT_PREFIX_INFORMATION,
            NdpOption::Mtu(_) => OPT_MTU,
            NdpOption::Rdnss { .. } => OPT_RDNSS,
            NdpOption::Other { option_type, .. } => *option_type,
        }
    }

    /// Serialise the option, header included.
    ///
    /// The length octet is **in units of 8 octets** (RFC 4861 §4.6), so it is the total length
    /// divided by eight and never the octet count. Every option this codec produces is a whole
    /// number of 8-octet units by construction, and the assertion at the end of this function
    /// is what keeps that true if a variant is added later.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            NdpOption::SourceLinkLayerAddress(mac) | NdpOption::TargetLinkLayerAddress(mac) => {
                // type | length=1 | 6 octets of IEEE 802 address = 8 octets total.
                out.push(self.option_type());
                out.push(1);
                out.extend_from_slice(mac);
            }
            NdpOption::PrefixInformation(p) => {
                if p.prefix_length > 128 {
                    bail!(
                        "prefix length {} is impossible; an IPv6 prefix is 0-128 bits",
                        p.prefix_length
                    );
                }
                // RFC 4861 §4.6.2 warns about exactly this combination: a prefix that a host is
                // told to build an address from must leave 64 bits for the interface identifier.
                if p.autonomous && p.prefix_length != 64 {
                    bail!(
                        "an autonomous prefix must be a /64 — SLAAC appends a 64-bit interface \
                         identifier, so /{} leaves no room and every host would ignore it. Set \
                         autonomous to false for a non-/64 prefix.",
                        p.prefix_length
                    );
                }
                if p.preferred_lifetime > p.valid_lifetime {
                    bail!(
                        "preferred_lifetime ({}) must not exceed valid_lifetime ({}); RFC 4861 \
                         §4.6.2 requires a host to ignore the whole option when it does",
                        p.preferred_lifetime,
                        p.valid_lifetime
                    );
                }
                out.push(OPT_PREFIX_INFORMATION);
                out.push(4); // 32 octets / 8
                out.push(p.prefix_length);
                let mut flags = 0u8;
                if p.on_link {
                    flags |= PREFIX_FLAG_ON_LINK;
                }
                if p.autonomous {
                    flags |= PREFIX_FLAG_AUTONOMOUS;
                }
                out.push(flags);
                out.extend_from_slice(&p.valid_lifetime.to_be_bytes());
                out.extend_from_slice(&p.preferred_lifetime.to_be_bytes());
                out.extend_from_slice(&0u32.to_be_bytes()); // Reserved2
                out.extend_from_slice(&p.prefix.octets());
            }
            NdpOption::Mtu(mtu) => {
                out.push(OPT_MTU);
                out.push(1); // 8 octets / 8
                out.extend_from_slice(&0u16.to_be_bytes()); // Reserved
                out.extend_from_slice(&mtu.to_be_bytes());
            }
            NdpOption::Rdnss { lifetime, servers } => {
                if servers.is_empty() {
                    bail!("an RDNSS option with no servers says nothing; omit it instead");
                }
                // 8 octets of header plus 16 per address => 1 + 2*n units.
                let units = 1 + 2 * servers.len();
                if units > u8::MAX as usize {
                    bail!(
                        "{} RDNSS servers need {units} 8-octet units, past the 255 the length \
                         field can express",
                        servers.len()
                    );
                }
                out.push(OPT_RDNSS);
                out.push(units as u8);
                out.extend_from_slice(&0u16.to_be_bytes()); // Reserved
                out.extend_from_slice(&lifetime.to_be_bytes());
                for server in servers {
                    out.extend_from_slice(&server.octets());
                }
            }
            NdpOption::Other { option_type, .. } => {
                bail!(
                    "option type {option_type} was decoded but cannot be re-encoded: its contents \
                     were deliberately not kept"
                );
            }
        }

        // The invariant the length field encodes. An option that is not a multiple of eight
        // cannot be expressed at all, and shipping one would desynchronise every receiver's
        // option walk from this point to the end of the packet.
        if out.len() % 8 != 0 || out.is_empty() {
            bail!(
                "option type {} encoded to {} octets, which is not a whole number of 8-octet \
                 units",
                self.option_type(),
                out.len()
            );
        }
        Ok(out)
    }

    /// What the model is shown about one option: names, addresses and seconds. No octets.
    pub fn to_event_data(&self) -> Value {
        match self {
            NdpOption::SourceLinkLayerAddress(mac) => json!({
                "option": "source_link_layer_address",
                "link_layer_address": format_mac(mac),
            }),
            NdpOption::TargetLinkLayerAddress(mac) => json!({
                "option": "target_link_layer_address",
                "link_layer_address": format_mac(mac),
            }),
            NdpOption::PrefixInformation(p) => json!({
                "option": "prefix_information",
                "prefix": p.prefix.to_string(),
                "length": p.prefix_length,
                "on_link": p.on_link,
                "autonomous": p.autonomous,
                "valid_lifetime": p.valid_lifetime,
                "preferred_lifetime": p.preferred_lifetime,
            }),
            NdpOption::Mtu(mtu) => json!({"option": "mtu", "mtu": mtu}),
            NdpOption::Rdnss { lifetime, servers } => json!({
                "option": "rdnss",
                "lifetime": lifetime,
                "servers": servers.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            }),
            NdpOption::Other {
                option_type,
                length_units,
            } => json!({
                "option": "unrecognised",
                "option_type": option_type,
                "length_units": length_units,
            }),
        }
    }
}

/// Decode a run of options.
///
/// RFC 4861 §4.6: "Nodes MUST silently discard an ND packet that contains an option with length
/// zero." That is not pedantry — a zero length is an infinite loop in a naive walker, and it is
/// how a malicious neighbour wedges a stack.
pub fn decode_options(mut data: &[u8]) -> Result<Vec<NdpOption>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        if data.len() < 2 {
            bail!("option header is truncated: {} octet(s) remain", data.len());
        }
        let option_type = data[0];
        let units = data[1];
        if units == 0 {
            bail!("option type {option_type} declares length 0, which RFC 4861 §4.6 forbids");
        }
        let total = units as usize * 8;
        if total > data.len() {
            bail!(
                "option type {option_type} declares {units} 8-octet units ({total} octets) but \
                 only {} remain",
                data.len()
            );
        }
        let body = &data[2..total];
        data = &data[total..];

        let option = match (option_type, units) {
            (OPT_SOURCE_LINK_LAYER_ADDRESS, 1) => {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&body[..6]);
                NdpOption::SourceLinkLayerAddress(mac)
            }
            (OPT_TARGET_LINK_LAYER_ADDRESS, 1) => {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&body[..6]);
                NdpOption::TargetLinkLayerAddress(mac)
            }
            (OPT_PREFIX_INFORMATION, 4) => {
                let mut prefix = [0u8; 16];
                prefix.copy_from_slice(&body[14..30]);
                NdpOption::PrefixInformation(PrefixInformation {
                    prefix_length: body[0],
                    on_link: body[1] & PREFIX_FLAG_ON_LINK != 0,
                    autonomous: body[1] & PREFIX_FLAG_AUTONOMOUS != 0,
                    valid_lifetime: u32::from_be_bytes([body[2], body[3], body[4], body[5]]),
                    preferred_lifetime: u32::from_be_bytes([body[6], body[7], body[8], body[9]]),
                    prefix: Ipv6Addr::from(prefix),
                })
            }
            (OPT_MTU, 1) => {
                NdpOption::Mtu(u32::from_be_bytes([body[2], body[3], body[4], body[5]]))
            }
            // 1 header unit + 2 units per address, so an even number of units cannot be RDNSS.
            (OPT_RDNSS, units) if units >= 3 && units % 2 == 1 => {
                let lifetime = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                let mut servers = Vec::new();
                for chunk in body[6..].chunks_exact(16) {
                    let mut addr = [0u8; 16];
                    addr.copy_from_slice(chunk);
                    servers.push(Ipv6Addr::from(addr));
                }
                NdpOption::Rdnss { lifetime, servers }
            }
            // A known type with an impossible length, or a type this codec does not model. Both
            // are walked over rather than fatal: RFC 4861 §4.6 requires a receiver to ignore
            // options it does not recognise, and a single malformed option should not lose an
            // otherwise valid solicitation.
            (option_type, length_units) => NdpOption::Other {
                option_type,
                length_units,
            },
        };
        out.push(option);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

/// The body of a Router Advertisement (RFC 4861 §4.2).
///
/// This is the highest-impact message in IPv6. One of these hands a whole link its prefix, its
/// default route and — through RDNSS — its DNS resolvers, and a host accepts it from anybody on
/// the segment. See this module's `CLAUDE.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterAdvertisement {
    /// Hop Limit hosts should use. 0 means "unspecified, keep your own".
    pub cur_hop_limit: u8,
    /// "Managed address configuration": get your address from DHCPv6, not from a prefix.
    pub managed: bool,
    /// "Other configuration": get everything except the address from DHCPv6.
    pub other: bool,
    /// Seconds this router should be used as a default router. **0 means "I am not a router"**
    /// and is how a router withdraws itself.
    pub router_lifetime: u16,
    /// Milliseconds a neighbour is assumed reachable after a reachability confirmation.
    pub reachable_time: u32,
    /// Milliseconds between retransmitted Neighbour Solicitations.
    pub retrans_timer: u32,
    pub options: Vec<NdpOption>,
}

/// One of the five Neighbour Discovery messages, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NdpMessage {
    RouterSolicitation {
        options: Vec<NdpOption>,
    },
    RouterAdvertisement(RouterAdvertisement),
    NeighborSolicitation {
        target: Ipv6Addr,
        options: Vec<NdpOption>,
    },
    NeighborAdvertisement {
        router: bool,
        solicited: bool,
        override_flag: bool,
        target: Ipv6Addr,
        options: Vec<NdpOption>,
    },
    Redirect {
        /// A better first hop for `destination`, or `destination` itself when it is on-link.
        target: Ipv6Addr,
        destination: Ipv6Addr,
        options: Vec<NdpOption>,
    },
}

impl NdpMessage {
    pub fn message_type(&self) -> u8 {
        match self {
            NdpMessage::RouterSolicitation { .. } => TYPE_ROUTER_SOLICITATION,
            NdpMessage::RouterAdvertisement(_) => TYPE_ROUTER_ADVERTISEMENT,
            NdpMessage::NeighborSolicitation { .. } => TYPE_NEIGHBOR_SOLICITATION,
            NdpMessage::NeighborAdvertisement { .. } => TYPE_NEIGHBOR_ADVERTISEMENT,
            NdpMessage::Redirect { .. } => TYPE_REDIRECT,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            NdpMessage::RouterSolicitation { .. } => "router_solicitation",
            NdpMessage::RouterAdvertisement(_) => "router_advertisement",
            NdpMessage::NeighborSolicitation { .. } => "neighbor_solicitation",
            NdpMessage::NeighborAdvertisement { .. } => "neighbor_advertisement",
            NdpMessage::Redirect { .. } => "redirect",
        }
    }

    pub fn options(&self) -> &[NdpOption] {
        match self {
            NdpMessage::RouterSolicitation { options }
            | NdpMessage::NeighborSolicitation { options, .. }
            | NdpMessage::NeighborAdvertisement { options, .. }
            | NdpMessage::Redirect { options, .. } => options,
            NdpMessage::RouterAdvertisement(ra) => &ra.options,
        }
    }

    fn options_mut(&mut self) -> &mut Vec<NdpOption> {
        match self {
            NdpMessage::RouterSolicitation { options }
            | NdpMessage::NeighborSolicitation { options, .. }
            | NdpMessage::NeighborAdvertisement { options, .. }
            | NdpMessage::Redirect { options, .. } => options,
            NdpMessage::RouterAdvertisement(ra) => &mut ra.options,
        }
    }

    /// The Source Link-Layer Address option, if the sender included one.
    pub fn source_link_layer(&self) -> Option<[u8; 6]> {
        self.options().iter().find_map(|o| match o {
            NdpOption::SourceLinkLayerAddress(mac) => Some(*mac),
            _ => None,
        })
    }

    /// The Target Link-Layer Address option, if the sender included one.
    pub fn target_link_layer(&self) -> Option<[u8; 6]> {
        self.options().iter().find_map(|o| match o {
            NdpOption::TargetLinkLayerAddress(mac) => Some(*mac),
            _ => None,
        })
    }

    /// Serialise, with a correct checksum over the pseudo-header for `source` -> `destination`.
    ///
    /// Both addresses are required arguments rather than optional, because the checksum genuinely
    /// cannot be computed without them and an API that let a caller forget would produce packets
    /// every receiver drops.
    pub fn encode(&self, source: Ipv6Addr, destination: Ipv6Addr) -> Result<Vec<u8>> {
        let mut out = self.encode_without_checksum()?;
        write_checksum(&mut out, source, destination)?;
        Ok(out)
    }

    /// Serialise with the checksum field left as zero.
    ///
    /// Public because it is the honest thing to hand a raw ICMPv6 socket: RFC 3542 §3.1 makes the
    /// kernel compute and insert the checksum for `IPPROTO_ICMPV6`, so anything written there is
    /// overwritten. Also what the checksum tests build on.
    pub fn encode_without_checksum(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(64);
        out.push(self.message_type());
        out.push(NDP_CODE);
        out.extend_from_slice(&[0, 0]); // Checksum, filled in later.

        match self {
            NdpMessage::RouterSolicitation { .. } => {
                out.extend_from_slice(&0u32.to_be_bytes()); // Reserved
            }
            NdpMessage::RouterAdvertisement(ra) => {
                out.push(ra.cur_hop_limit);
                let mut flags = 0u8;
                if ra.managed {
                    flags |= RA_FLAG_MANAGED;
                }
                if ra.other {
                    flags |= RA_FLAG_OTHER;
                }
                out.push(flags);
                out.extend_from_slice(&ra.router_lifetime.to_be_bytes());
                out.extend_from_slice(&ra.reachable_time.to_be_bytes());
                out.extend_from_slice(&ra.retrans_timer.to_be_bytes());
            }
            NdpMessage::NeighborSolicitation { target, .. } => {
                out.extend_from_slice(&0u32.to_be_bytes()); // Reserved
                out.extend_from_slice(&target.octets());
            }
            NdpMessage::NeighborAdvertisement {
                router,
                solicited,
                override_flag,
                target,
                ..
            } => {
                let mut flags = 0u32;
                if *router {
                    flags |= NA_FLAG_ROUTER;
                }
                if *solicited {
                    flags |= NA_FLAG_SOLICITED;
                }
                if *override_flag {
                    flags |= NA_FLAG_OVERRIDE;
                }
                out.extend_from_slice(&flags.to_be_bytes());
                out.extend_from_slice(&target.octets());
            }
            NdpMessage::Redirect {
                target,
                destination,
                ..
            } => {
                out.extend_from_slice(&0u32.to_be_bytes()); // Reserved
                out.extend_from_slice(&target.octets());
                out.extend_from_slice(&destination.octets());
            }
        }

        for option in self.options() {
            out.extend_from_slice(&option.encode()?);
        }
        Ok(out)
    }

    /// Parse an ICMPv6 message that is expected to be one of the five NDP types.
    ///
    /// The checksum is **not** checked here — see [`verify_checksum`] for why it cannot be, from
    /// the ICMPv6 bytes alone.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < 4 {
            bail!(
                "ICMPv6 message is {} octet(s); the header alone is 4",
                data.len()
            );
        }
        let message_type = data[0];
        let code = data[1];
        if code != NDP_CODE {
            bail!(
                "ICMPv6 type {message_type} carries code {code}; RFC 4861 requires code 0 for \
                 every Neighbour Discovery message and a receiver discards anything else"
            );
        }
        let body = &data[4..];

        let need = |want: usize, what: &str| -> Result<()> {
            if body.len() < want {
                bail!(
                    "{what} needs {} octets after the ICMPv6 header, got {}",
                    want,
                    body.len()
                );
            }
            Ok(())
        };

        let addr_at = |offset: usize| -> Ipv6Addr {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&body[offset..offset + 16]);
            Ipv6Addr::from(octets)
        };

        match message_type {
            TYPE_ROUTER_SOLICITATION => {
                need(4, "a Router Solicitation")?;
                Ok(NdpMessage::RouterSolicitation {
                    options: decode_options(&body[4..])?,
                })
            }
            TYPE_ROUTER_ADVERTISEMENT => {
                need(12, "a Router Advertisement")?;
                Ok(NdpMessage::RouterAdvertisement(RouterAdvertisement {
                    cur_hop_limit: body[0],
                    managed: body[1] & RA_FLAG_MANAGED != 0,
                    other: body[1] & RA_FLAG_OTHER != 0,
                    router_lifetime: u16::from_be_bytes([body[2], body[3]]),
                    reachable_time: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                    retrans_timer: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
                    options: decode_options(&body[12..])?,
                }))
            }
            TYPE_NEIGHBOR_SOLICITATION => {
                need(20, "a Neighbour Solicitation")?;
                Ok(NdpMessage::NeighborSolicitation {
                    target: addr_at(4),
                    options: decode_options(&body[20..])?,
                })
            }
            TYPE_NEIGHBOR_ADVERTISEMENT => {
                need(20, "a Neighbour Advertisement")?;
                let flags = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                Ok(NdpMessage::NeighborAdvertisement {
                    router: flags & NA_FLAG_ROUTER != 0,
                    solicited: flags & NA_FLAG_SOLICITED != 0,
                    override_flag: flags & NA_FLAG_OVERRIDE != 0,
                    target: addr_at(4),
                    options: decode_options(&body[20..])?,
                })
            }
            TYPE_REDIRECT => {
                need(36, "a Redirect")?;
                Ok(NdpMessage::Redirect {
                    target: addr_at(4),
                    destination: addr_at(20),
                    options: decode_options(&body[36..])?,
                })
            }
            other => bail!(
                "ICMPv6 type {other} is not a Neighbour Discovery message (133 Router \
                 Solicitation, 134 Router Advertisement, 135 Neighbour Solicitation, 136 \
                 Neighbour Advertisement, 137 Redirect)"
            ),
        }
    }

    /// Everything the model is shown about a received message. Addresses as strings, flags as
    /// booleans, lifetimes as numbers of seconds — never octets.
    ///
    /// Fields the message did not carry are **omitted rather than null**, so a script can test
    /// presence with a plain `in`.
    pub fn to_event_data(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("message_type".into(), json!(self.type_name()));
        map.insert("icmpv6_type".into(), json!(self.message_type()));

        match self {
            NdpMessage::RouterSolicitation { .. } => {}
            NdpMessage::RouterAdvertisement(ra) => {
                map.insert("cur_hop_limit".into(), json!(ra.cur_hop_limit));
                map.insert("managed".into(), json!(ra.managed));
                map.insert("other".into(), json!(ra.other));
                map.insert("router_lifetime".into(), json!(ra.router_lifetime));
                map.insert("reachable_time".into(), json!(ra.reachable_time));
                map.insert("retrans_timer".into(), json!(ra.retrans_timer));

                let prefixes: Vec<Value> = ra
                    .options
                    .iter()
                    .filter_map(|o| match o {
                        NdpOption::PrefixInformation(p) => Some(json!({
                            "prefix": p.prefix.to_string(),
                            "length": p.prefix_length,
                            "on_link": p.on_link,
                            "autonomous": p.autonomous,
                            "valid_lifetime": p.valid_lifetime,
                            "preferred_lifetime": p.preferred_lifetime,
                        })),
                        _ => None,
                    })
                    .collect();
                if !prefixes.is_empty() {
                    map.insert("prefixes".into(), json!(prefixes));
                }
                let rdnss: Vec<String> = ra
                    .options
                    .iter()
                    .flat_map(|o| match o {
                        NdpOption::Rdnss { servers, .. } => {
                            servers.iter().map(|s| s.to_string()).collect()
                        }
                        _ => Vec::new(),
                    })
                    .collect();
                if !rdnss.is_empty() {
                    map.insert("rdnss".into(), json!(rdnss));
                }
                if let Some(mtu) = ra.options.iter().find_map(|o| match o {
                    NdpOption::Mtu(m) => Some(*m),
                    _ => None,
                }) {
                    map.insert("mtu".into(), json!(mtu));
                }
            }
            NdpMessage::NeighborSolicitation { target, .. } => {
                map.insert("target_address".into(), json!(target.to_string()));
                map.insert(
                    "solicited_node_multicast".into(),
                    json!(solicited_node_multicast(*target).to_string()),
                );
            }
            NdpMessage::NeighborAdvertisement {
                router,
                solicited,
                override_flag,
                target,
                ..
            } => {
                map.insert("target_address".into(), json!(target.to_string()));
                map.insert("router".into(), json!(router));
                map.insert("solicited".into(), json!(solicited));
                map.insert("override".into(), json!(override_flag));
            }
            NdpMessage::Redirect {
                target,
                destination,
                ..
            } => {
                map.insert("target_address".into(), json!(target.to_string()));
                map.insert("destination_address".into(), json!(destination.to_string()));
            }
        }

        if let Some(mac) = self.source_link_layer() {
            map.insert("source_link_layer".into(), json!(format_mac(&mac)));
        }
        if let Some(mac) = self.target_link_layer() {
            map.insert("target_link_layer".into(), json!(format_mac(&mac)));
        }
        let options: Vec<Value> = self.options().iter().map(|o| o.to_event_data()).collect();
        if !options.is_empty() {
            map.insert("options".into(), json!(options));
        }
        map
    }
}

// ---------------------------------------------------------------------------------------------
// Action -> message
// ---------------------------------------------------------------------------------------------

/// A validated request to transmit one NDP message.
///
/// `source` and `destination` are optional because the *server* owns both: it knows its own
/// link-local address and it knows who it is answering. A model naming them is impersonating a
/// specific node deliberately, which is allowed and is exactly what makes this protocol
/// interesting, but it must not be forced to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    pub message: NdpMessage,
    pub source: Option<Ipv6Addr>,
    pub destination: Option<Ipv6Addr>,
}

impl SendRequest {
    /// Interpret one `send_*` action.
    ///
    /// Deliberately strict. Every message this builds writes something into the peer's stack —
    /// a neighbour cache entry, a default route, a resolver list — so a field the model got wrong
    /// must fail here, where the model can be told, rather than reaching a host half-formed.
    pub fn from_action(action: &Value) -> Result<Self> {
        let action_type = action
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("action has no 'type'"))?;

        let message = match action_type {
            "send_router_advertisement" => Self::router_advertisement(action)?,
            "send_neighbor_advertisement" => Self::neighbor_advertisement(action)?,
            "send_neighbor_solicitation" => Self::neighbor_solicitation(action)?,
            other => bail!("'{other}' does not describe an NDP message this server can transmit"),
        };

        // Encoding here is what makes this a validation rather than a hope: an autonomous prefix
        // that is not a /64, a preferred lifetime past its valid lifetime, an RDNSS list too long
        // for the length field — all fail now.
        message.encode_without_checksum()?;

        Ok(Self {
            message,
            source: optional_addr(action, "source_address")?,
            destination: optional_addr(action, "destination_address")?,
        })
    }

    /// Add a link-layer address option if the action named none.
    ///
    /// The server, not the model, is the authority on its own hardware address; RFC 4861 §4.4
    /// requires the Target Link-Layer Address option on a solicited Neighbour Advertisement, and
    /// §4.2 recommends the Source Link-Layer Address option on a Router Advertisement. Omitting
    /// either yields a message a peer records incompletely and then has to solicit again for.
    pub fn with_default_link_layer(mut self, mac: [u8; 6]) -> Self {
        let wanted = match &self.message {
            NdpMessage::NeighborAdvertisement { .. } => OPT_TARGET_LINK_LAYER_ADDRESS,
            NdpMessage::RouterAdvertisement(_) | NdpMessage::NeighborSolicitation { .. } => {
                OPT_SOURCE_LINK_LAYER_ADDRESS
            }
            _ => return self,
        };
        if self
            .message
            .options()
            .iter()
            .any(|o| o.option_type() == wanted)
        {
            return self;
        }
        let option = if wanted == OPT_TARGET_LINK_LAYER_ADDRESS {
            NdpOption::TargetLinkLayerAddress(mac)
        } else {
            NdpOption::SourceLinkLayerAddress(mac)
        };
        self.message.options_mut().push(option);
        self
    }

    fn router_advertisement(action: &Value) -> Result<NdpMessage> {
        let mut options = Vec::new();

        for (index, entry) in array_field(action, "prefixes")?.iter().enumerate() {
            let prefix_text = entry
                .get("prefix")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("prefixes[{index}] has no 'prefix'"))?;
            // A model will reach for CIDR notation, and refusing it over a slash would be
            // pointless pedantry — but the explicit `length` field wins when both are given.
            let (base, slash_length) = match prefix_text.split_once('/') {
                Some((addr, len)) => (
                    addr,
                    Some(len.trim().parse::<u8>().with_context(|| {
                        format!(
                            "prefixes[{index}] has an unreadable prefix length in '{prefix_text}'"
                        )
                    })?),
                ),
                None => (prefix_text, None),
            };
            let prefix = base.trim().parse::<Ipv6Addr>().with_context(|| {
                format!("prefixes[{index}] prefix '{base}' is not an IPv6 address")
            })?;
            let prefix_length = match entry.get("length") {
                Some(v) => u8::try_from(
                    v.as_u64()
                        .ok_or_else(|| anyhow!("prefixes[{index}].length must be a number"))?,
                )
                .map_err(|_| anyhow!("prefixes[{index}].length must be 0-128"))?,
                None => slash_length.unwrap_or(64),
            };

            options.push(NdpOption::PrefixInformation(PrefixInformation {
                prefix,
                prefix_length,
                on_link: bool_field(entry, "on_link")?.unwrap_or(true),
                autonomous: bool_field(entry, "autonomous")?.unwrap_or(true),
                valid_lifetime: u32_field(entry, "valid_lifetime")?.unwrap_or(2_592_000),
                preferred_lifetime: u32_field(entry, "preferred_lifetime")?.unwrap_or(604_800),
            }));
        }

        if let Some(mtu) = u32_field(action, "mtu")? {
            if mtu < 1280 {
                bail!("an MTU of {mtu} is below IPv6's minimum link MTU of 1280 (RFC 8200 §5)");
            }
            options.push(NdpOption::Mtu(mtu));
        }

        let servers = array_field(action, "rdnss")?;
        if !servers.is_empty() {
            let mut resolved = Vec::with_capacity(servers.len());
            for (index, entry) in servers.iter().enumerate() {
                let text = entry
                    .as_str()
                    .ok_or_else(|| anyhow!("rdnss[{index}] must be an IPv6 address string"))?;
                resolved.push(
                    text.trim().parse::<Ipv6Addr>().with_context(|| {
                        format!("rdnss[{index}] '{text}' is not an IPv6 address")
                    })?,
                );
            }
            options.push(NdpOption::Rdnss {
                lifetime: u32_field(action, "rdnss_lifetime")?.unwrap_or(600),
                servers: resolved,
            });
        }

        if let Some(mac) = mac_field(action, "source_link_layer")? {
            options.push(NdpOption::SourceLinkLayerAddress(mac));
        }

        let router_lifetime = match u32_field(action, "router_lifetime")? {
            None => 1800,
            Some(v) => u16::try_from(v)
                .map_err(|_| anyhow!("router_lifetime is 16 bits: 0-65535 seconds, got {v}"))?,
        };

        Ok(NdpMessage::RouterAdvertisement(RouterAdvertisement {
            cur_hop_limit: match u32_field(action, "hop_limit")? {
                None => 64,
                Some(v) => u8::try_from(v)
                    .map_err(|_| anyhow!("hop_limit is one octet: 0-255, got {v}"))?,
            },
            managed: bool_field(action, "managed")?.unwrap_or(false),
            other: bool_field(action, "other")?.unwrap_or(false),
            router_lifetime,
            reachable_time: u32_field(action, "reachable_time")?.unwrap_or(0),
            retrans_timer: u32_field(action, "retrans_timer")?.unwrap_or(0),
            options,
        }))
    }

    fn neighbor_advertisement(action: &Value) -> Result<NdpMessage> {
        let target = required_addr(action, "target")?;
        let mut options = Vec::new();
        if let Some(mac) = mac_field(action, "target_link_layer")? {
            options.push(NdpOption::TargetLinkLayerAddress(mac));
        }
        Ok(NdpMessage::NeighborAdvertisement {
            router: bool_field(action, "router")?.unwrap_or(false),
            // Defaults chosen for the overwhelmingly common case: an advertisement sent because
            // somebody solicited it, carrying the authoritative answer.
            solicited: bool_field(action, "solicited")?.unwrap_or(true),
            override_flag: bool_field(action, "override")?.unwrap_or(true),
            target,
            options,
        })
    }

    fn neighbor_solicitation(action: &Value) -> Result<NdpMessage> {
        let target = required_addr(action, "target")?;
        let mut options = Vec::new();
        if let Some(mac) = mac_field(action, "source_link_layer")? {
            options.push(NdpOption::SourceLinkLayerAddress(mac));
        }
        Ok(NdpMessage::NeighborSolicitation { target, options })
    }

    /// Where this message goes when the action named no destination.
    ///
    /// `peer` is the source address of whatever provoked it, when there was one.
    pub fn default_destination(&self, peer: Option<Ipv6Addr>) -> Ipv6Addr {
        // An unspecified source (::) means the peer has no address yet — a node doing Duplicate
        // Address Detection. It cannot receive a unicast reply, so the answer is multicast.
        let peer = peer.filter(|p| !p.is_unspecified());
        match &self.message {
            NdpMessage::RouterAdvertisement(_) => peer.unwrap_or(ALL_NODES_MULTICAST),
            NdpMessage::NeighborAdvertisement { .. } => peer.unwrap_or(ALL_NODES_MULTICAST),
            // A solicitation is for an address we do not know the link-layer address of, so it
            // goes to the target's solicited-node group rather than to the peer.
            NdpMessage::NeighborSolicitation { target, .. } => solicited_node_multicast(*target),
            NdpMessage::RouterSolicitation { .. } => ALL_ROUTERS_MULTICAST,
            NdpMessage::Redirect { .. } => peer.unwrap_or(ALL_NODES_MULTICAST),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Small typed field readers
// ---------------------------------------------------------------------------------------------

fn required_addr(action: &Value, key: &str) -> Result<Ipv6Addr> {
    let text = action
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'{key}' is required and must be an IPv6 address string"))?;
    text.trim()
        .parse::<Ipv6Addr>()
        .with_context(|| format!("'{key}' value '{text}' is not an IPv6 address"))
}

fn optional_addr(action: &Value, key: &str) -> Result<Option<Ipv6Addr>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            Ok(Some(s.trim().parse::<Ipv6Addr>().with_context(|| {
                format!("'{key}' value '{s}' is not an IPv6 address")
            })?))
        }
        Some(other) => bail!("'{key}' must be an IPv6 address string, got {other}"),
    }
}

fn mac_field(action: &Value, key: &str) -> Result<Option<[u8; 6]>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(parse_mac(s)?)),
        Some(other) => bail!("'{key}' must be a link-layer address string, got {other}"),
    }
}

fn bool_field(action: &Value, key: &str) -> Result<Option<bool>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => bail!("'{key}' must be true or false, got {other}"),
    }
}

fn u32_field(action: &Value, key: &str) -> Result<Option<u32>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| anyhow!("'{key}' must be a whole non-negative number, got {v}"))?;
            Ok(Some(u32::try_from(n).map_err(|_| {
                anyhow!("'{key}' must fit in 32 bits, got {n}")
            })?))
        }
    }
}

fn array_field(action: &Value, key: &str) -> Result<Vec<Value>> {
    match action.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(other) => bail!("'{key}' must be an array, got {other}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The UDP test transport's framing
// ---------------------------------------------------------------------------------------------

/// How many octets of addressing precede the ICMPv6 message on the UDP test transport.
pub const ADDRESSED_PREFIX_LEN: usize = 32;

/// Wrap an ICMPv6 message with the two addresses its checksum was computed over.
///
/// The UDP test transport exists because a raw ICMPv6 socket needs privileges nothing here has.
/// It carries `source(16) || destination(16) || icmpv6 message`, which is exactly the part of
/// the IPv6 header the ICMPv6 checksum depends on — so the receiving side can verify the
/// checksum, and the test therefore proves the pseudo-header calculation end to end rather than
/// only in the codec's own unit tests.
///
/// **No real NDP peer speaks this.** It is not a tunnel or an encapsulation anyone has defined.
pub fn encode_addressed(source: Ipv6Addr, destination: Ipv6Addr, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ADDRESSED_PREFIX_LEN + message.len());
    out.extend_from_slice(&source.octets());
    out.extend_from_slice(&destination.octets());
    out.extend_from_slice(message);
    out
}

/// The inverse of [`encode_addressed`].
pub fn decode_addressed(datagram: &[u8]) -> Result<(Ipv6Addr, Ipv6Addr, &[u8])> {
    if datagram.len() < ADDRESSED_PREFIX_LEN + 4 {
        bail!(
            "datagram is {} octets; the UDP test transport carries 16 octets of source address, \
             16 of destination and then at least a 4-octet ICMPv6 header",
            datagram.len()
        );
    }
    let mut source = [0u8; 16];
    source.copy_from_slice(&datagram[0..16]);
    let mut destination = [0u8; 16];
    destination.copy_from_slice(&datagram[16..32]);
    Ok((
        Ipv6Addr::from(source),
        Ipv6Addr::from(destination),
        &datagram[ADDRESSED_PREFIX_LEN..],
    ))
}
