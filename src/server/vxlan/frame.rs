//! VXLAN (RFC 7348) and Geneve (RFC 8926) encapsulation, and just enough of Ethernet, ARP,
//! IPv4, ICMP echo and UDP to be a host on the overlay. Shared by the server and the client
//! (`src/client/vxlan/`). Pure: no sockets here.
use anyhow::{bail, ensure, Context, Result};
use std::net::Ipv4Addr;

pub const VXLAN_PORT: u16 = 4789;
pub const GENEVE_PORT: u16 = 6081;
/// Geneve's protocol type for an Ethernet payload (Transparent Ethernet Bridging).
const ETH_BRIDGING: u16 = 0x6558;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
/// The largest UDP payload: one IPv4 datagram's worth.
pub const MAX_DATAGRAM: usize = 65_507;
/// A UDP payload NetGet will build into one inner frame; bigger would need IP fragmentation,
/// which the overlay host does not do.
pub const MAX_UDP_PAYLOAD: usize = 1400;
/// A locally administered MAC for NetGet's overlay hosts when none is given.
pub const DEFAULT_MAC: &str = "02:4e:47:00:00:01";

pub type Mac = [u8; 6];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encap {
    Vxlan,
    Geneve,
}

impl Encap {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "vxlan" => Ok(Encap::Vxlan),
            "geneve" => Ok(Encap::Geneve),
            other => bail!("encapsulation {other:?} is not vxlan or geneve"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Encap::Vxlan => "vxlan",
            Encap::Geneve => "geneve",
        }
    }
    pub fn port(self) -> u16 {
        match self {
            Encap::Vxlan => VXLAN_PORT,
            Encap::Geneve => GENEVE_PORT,
        }
    }
}

pub const MAX_VNI: u32 = 0xff_ffff;

/// The VNI and the inner Ethernet frame of one encapsulated datagram.
pub fn decap(encap: Encap, b: &[u8]) -> Result<(u32, &[u8])> {
    ensure!(
        b.len() >= 8,
        "a {}-byte datagram has no {} header",
        b.len(),
        encap.name()
    );
    let vni = u32::from_be_bytes([0, b[4], b[5], b[6]]);
    match encap {
        Encap::Vxlan => {
            ensure!(b[0] & 0x08 != 0, "the VXLAN I flag is clear: no valid VNI");
            Ok((vni, &b[8..]))
        }
        Encap::Geneve => {
            let version = b[0] >> 6;
            ensure!(version == 0, "Geneve version {version}");
            ensure!(b[1] & 0x80 == 0, "a Geneve OAM packet");
            let options = (b[0] & 0x3f) as usize * 4;
            let protocol = u16::from_be_bytes([b[2], b[3]]);
            ensure!(
                protocol == ETH_BRIDGING,
                "Geneve protocol type {protocol:#06x}, not Ethernet"
            );
            ensure!(
                b.len() >= 8 + options,
                "Geneve options run past the datagram"
            );
            Ok((vni, &b[8 + options..]))
        }
    }
}

pub fn encap(encap: Encap, vni: u32, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + inner.len());
    let v = vni.to_be_bytes();
    match encap {
        Encap::Vxlan => out.extend_from_slice(&[0x08, 0, 0, 0, v[1], v[2], v[3], 0]),
        Encap::Geneve => {
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(&ETH_BRIDGING.to_be_bytes());
            out.extend_from_slice(&[v[1], v[2], v[3], 0]);
        }
    }
    out.extend_from_slice(inner);
    out
}

pub fn parse_mac(s: &str) -> Result<Mac> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    ensure!(
        parts.len() == 6,
        "{s:?} is not a MAC address like 02:00:00:00:00:01"
    );
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] =
            u8::from_str_radix(p, 16).with_context(|| format!("{s:?} is not a MAC address"))?;
    }
    ensure!(mac[0] & 1 == 0, "{s} is a multicast address, not a host's");
    Ok(mac)
}

pub fn mac_str(m: &Mac) -> String {
    m.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arp {
    pub request: bool,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4Addr,
    pub target_mac: Mac,
    pub target_ip: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Arp(Arp),
    Echo {
        request: bool,
        identifier: u16,
        sequence: u16,
        data: Vec<u8>,
    },
    /// ICMP destination unreachable / time exceeded, with what it is about.
    IcmpError {
        kind: u8,
        code: u8,
        original_protocol: u8,
        original_dst: Ipv4Addr,
        original_dst_port: Option<u16>,
    },
    Udp {
        src_port: u16,
        dst_port: u16,
        data: Vec<u8>,
    },
    /// Anything else, described but not answered.
    Other {
        what: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub dst_mac: Mac,
    pub src_mac: Mac,
    /// IPv4 only; ARP carries its addresses in `Payload::Arp`.
    pub src_ip: Option<Ipv4Addr>,
    pub dst_ip: Option<Ipv4Addr>,
    pub payload: Payload,
}

/// RFC 1071.
pub fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    let mut odd: Option<u8> = None;
    for part in parts {
        for &b in *part {
            match odd.take() {
                Some(hi) => sum += u32::from(u16::from_be_bytes([hi, b])),
                None => odd = Some(b),
            }
        }
    }
    if let Some(hi) = odd {
        sum += u32::from(u16::from_be_bytes([hi, 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn ip(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

fn mac(b: &[u8]) -> Mac {
    let mut m = [0u8; 6];
    m.copy_from_slice(&b[..6]);
    m
}

/// Parse an inner Ethernet frame. Malformed frames are errors; well-formed frames NetGet
/// does not answer (IPv6, TCP, fragments, other ICMP) are `Payload::Other`.
pub fn parse_frame(b: &[u8]) -> Result<Frame> {
    ensure!(
        b.len() >= 14,
        "a {}-byte inner frame is shorter than Ethernet",
        b.len()
    );
    let (dst_mac, src_mac) = (mac(&b[0..6]), mac(&b[6..12]));
    let ethertype = u16::from_be_bytes([b[12], b[13]]);
    let body = &b[14..];
    let other = |what: String| Frame {
        dst_mac,
        src_mac,
        src_ip: None,
        dst_ip: None,
        payload: Payload::Other { what },
    };
    match ethertype {
        ETHERTYPE_ARP => {
            ensure!(body.len() >= 28, "a truncated ARP packet");
            ensure!(
                body[0..6] == [0, 1, 8, 0, 6, 4],
                "ARP for something other than Ethernet/IPv4"
            );
            let op = u16::from_be_bytes([body[6], body[7]]);
            ensure!(op == 1 || op == 2, "ARP operation {op}");
            Ok(Frame {
                dst_mac,
                src_mac,
                src_ip: None,
                dst_ip: None,
                payload: Payload::Arp(Arp {
                    request: op == 1,
                    sender_mac: mac(&body[8..14]),
                    sender_ip: ip(&body[14..18]),
                    target_mac: mac(&body[18..24]),
                    target_ip: ip(&body[24..28]),
                }),
            })
        }
        ETHERTYPE_IPV4 => parse_ipv4(dst_mac, src_mac, body),
        other_type => Ok(other(format!("ethertype {other_type:#06x}"))),
    }
}

fn parse_ipv4(dst_mac: Mac, src_mac: Mac, b: &[u8]) -> Result<Frame> {
    ensure!(b.len() >= 20, "a truncated IPv4 header");
    ensure!(b[0] >> 4 == 4, "IP version {}", b[0] >> 4);
    let ihl = (b[0] & 0x0f) as usize * 4;
    ensure!(ihl >= 20 && b.len() >= ihl, "IPv4 header length {ihl}");
    let total = u16::from_be_bytes([b[2], b[3]]) as usize;
    ensure!(
        total >= ihl && total <= b.len(),
        "IPv4 total length {total} against a {}-byte payload",
        b.len()
    );
    ensure!(checksum(&[&b[..ihl]]) == 0, "bad IPv4 header checksum");
    let (src, dst) = (ip(&b[12..16]), ip(&b[16..20]));
    let protocol = b[9];
    let flags_frag = u16::from_be_bytes([b[6], b[7]]);
    let body = &b[ihl..total];
    let frame = |payload| Frame {
        dst_mac,
        src_mac,
        src_ip: Some(src),
        dst_ip: Some(dst),
        payload,
    };
    if flags_frag & 0x3fff != 0 {
        return Ok(frame(Payload::Other {
            what: "an IPv4 fragment".into(),
        }));
    }
    match protocol {
        1 => {
            ensure!(body.len() >= 8, "a truncated ICMP message");
            ensure!(checksum(&[body]) == 0, "bad ICMP checksum");
            let (kind, code) = (body[0], body[1]);
            match kind {
                0 | 8 => Ok(frame(Payload::Echo {
                    request: kind == 8,
                    identifier: u16::from_be_bytes([body[4], body[5]]),
                    sequence: u16::from_be_bytes([body[6], body[7]]),
                    data: body[8..].to_vec(),
                })),
                3 | 11 if body.len() >= 8 + 20 => {
                    let inner = &body[8..];
                    let inner_ihl = (inner[0] & 0x0f) as usize * 4;
                    let original_protocol = inner[9];
                    let original_dst_port = (original_protocol == 17
                        && inner.len() >= inner_ihl + 4)
                        .then(|| u16::from_be_bytes([inner[inner_ihl + 2], inner[inner_ihl + 3]]));
                    Ok(frame(Payload::IcmpError {
                        kind,
                        code,
                        original_protocol,
                        original_dst: ip(&inner[16..20]),
                        original_dst_port,
                    }))
                }
                _ => Ok(frame(Payload::Other {
                    what: format!("ICMP type {kind} code {code}"),
                })),
            }
        }
        17 => {
            ensure!(body.len() >= 8, "a truncated UDP header");
            let len = u16::from_be_bytes([body[4], body[5]]) as usize;
            ensure!(len >= 8 && len <= body.len(), "UDP length {len}");
            let sum = u16::from_be_bytes([body[6], body[7]]);
            if sum != 0 {
                ensure!(
                    checksum(&[&pseudo_header(src, dst, 17, len), &body[..len]]) == 0,
                    "bad UDP checksum"
                );
            }
            Ok(frame(Payload::Udp {
                src_port: u16::from_be_bytes([body[0], body[1]]),
                dst_port: u16::from_be_bytes([body[2], body[3]]),
                data: body[8..len].to_vec(),
            }))
        }
        6 => Ok(frame(Payload::Other { what: "TCP".into() })),
        p => Ok(frame(Payload::Other {
            what: format!("IP protocol {p}"),
        })),
    }
}

fn pseudo_header(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, len: usize) -> Vec<u8> {
    let mut p = Vec::with_capacity(12);
    p.extend_from_slice(&src.octets());
    p.extend_from_slice(&dst.octets());
    p.extend_from_slice(&[0, protocol]);
    p.extend_from_slice(&(len as u16).to_be_bytes());
    p
}

pub fn ethernet(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(14 + payload.len());
    out.extend_from_slice(&dst);
    out.extend_from_slice(&src);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A whole ARP frame. A request is broadcast with a zero target MAC.
pub fn arp_frame(
    request: bool,
    sender_mac: Mac,
    sender_ip: Ipv4Addr,
    target_mac: Mac,
    target_ip: Ipv4Addr,
) -> Vec<u8> {
    let mut a = vec![0, 1, 8, 0, 6, 4, 0, if request { 1 } else { 2 }];
    a.extend_from_slice(&sender_mac);
    a.extend_from_slice(&sender_ip.octets());
    a.extend_from_slice(&if request { [0; 6] } else { target_mac });
    a.extend_from_slice(&target_ip.octets());
    let dst = if request { [0xff; 6] } else { target_mac };
    ethernet(dst, sender_mac, ETHERTYPE_ARP, &a)
}

/// An IPv4 packet with TTL 64 and Don't Fragment.
pub fn ipv4(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, id: u16, payload: &[u8]) -> Vec<u8> {
    let total = (20 + payload.len()) as u16;
    let mut h = vec![0x45, 0];
    h.extend_from_slice(&total.to_be_bytes());
    h.extend_from_slice(&id.to_be_bytes());
    h.extend_from_slice(&[0x40, 0, 64, protocol, 0, 0]);
    h.extend_from_slice(&src.octets());
    h.extend_from_slice(&dst.octets());
    let sum = checksum(&[&h]);
    h[10..12].copy_from_slice(&sum.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

pub fn icmp_echo(request: bool, identifier: u16, sequence: u16, data: &[u8]) -> Vec<u8> {
    let mut m = vec![if request { 8 } else { 0 }, 0, 0, 0];
    m.extend_from_slice(&identifier.to_be_bytes());
    m.extend_from_slice(&sequence.to_be_bytes());
    m.extend_from_slice(data);
    let sum = checksum(&[&m]);
    m[2..4].copy_from_slice(&sum.to_be_bytes());
    m
}

pub fn udp(src: Ipv4Addr, dst: Ipv4Addr, src_port: u16, dst_port: u16, data: &[u8]) -> Vec<u8> {
    let len = 8 + data.len();
    let mut u = Vec::with_capacity(len);
    u.extend_from_slice(&src_port.to_be_bytes());
    u.extend_from_slice(&dst_port.to_be_bytes());
    u.extend_from_slice(&(len as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(data);
    let mut sum = checksum(&[&pseudo_header(src, dst, 17, len), &u]);
    if sum == 0 {
        sum = 0xffff;
    }
    u[6..8].copy_from_slice(&sum.to_be_bytes());
    u
}

/// Text when the bytes are printable UTF-8, hex otherwise; with the encoding named, so the
/// model can answer in kind (the TCP convention).
pub fn data_json(data: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(data) {
        Ok(s)
            if s.chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')) =>
        {
            (s.to_string(), "utf8")
        }
        _ => (hex::encode(data), "hex"),
    }
}

/// The bytes of an action's `data`, decoded by its `encoding` (`utf8` default, or `hex`).
pub fn data_bytes(v: &serde_json::Value) -> Result<Vec<u8>> {
    let data = v["data"].as_str().unwrap_or_default();
    let bytes = match v["encoding"].as_str().unwrap_or("utf8") {
        "utf8" => data.as_bytes().to_vec(),
        "hex" => hex::decode(data).context("data is not hex")?,
        other => bail!("encoding {other:?} is not utf8 or hex"),
    };
    ensure!(
        bytes.len() <= MAX_UDP_PAYLOAD,
        "{} bytes is more than one frame carries ({MAX_UDP_PAYLOAD})",
        bytes.len()
    );
    Ok(bytes)
}
