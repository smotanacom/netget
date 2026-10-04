//! BMP version 3 (RFC 7854, with the RFC 8671 Adj-RIB-Out flag and RFC 9069 Loc-RIB peers):
//! message framing, the per-peer header, TLVs, and the JSON the handler sees. Embedded BGP PDUs
//! are framed by their own length and decoded by `crate::server::bgp::wire` (netgauze).
use crate::server::bgp::wire;
use anyhow::{bail, ensure, Context, Result};
use netgauze_bgp_pkt::BgpMessage;
use serde_json::{json, Value as Json};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const VERSION: u8 = 3;
pub const HEADER_LEN: usize = 6;
pub const PEER_HEADER_LEN: usize = 42;
/// A BMP message carries at most two BGP PDUs (Peer Up) plus TLVs; BGP PDUs are 4096 bytes
/// unless extended messages were negotiated (65535).
pub const MAX_MESSAGE: usize = 256 * 1024;
/// Statistics entries and TLVs read from one message.
pub const MAX_ITEMS: usize = 1024;

pub const ROUTE_MONITORING: u8 = 0;
pub const STATISTICS: u8 = 1;
pub const PEER_DOWN: u8 = 2;
pub const PEER_UP: u8 = 3;
pub const INITIATION: u8 = 4;
pub const TERMINATION: u8 = 5;
pub const ROUTE_MIRRORING: u8 = 6;

pub const FLAG_V6: u8 = 0x80;
pub const FLAG_POST_POLICY: u8 = 0x40;
pub const FLAG_LEGACY_AS: u8 = 0x20;
pub const FLAG_ADJ_RIB_OUT: u8 = 0x10;

/// The length of the message at the front of `buf`, once its common header is there.
pub fn message_len(buf: &[u8]) -> Result<Option<usize>> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    ensure!(
        buf[0] == VERSION,
        "BMP version {} is not supported (only 3)",
        buf[0]
    );
    let len = u32::from_be_bytes(buf[1..5].try_into()?) as usize;
    ensure!(
        len >= HEADER_LEN,
        "BMP message length {len} is below the header"
    );
    ensure!(
        len <= MAX_MESSAGE,
        "BMP message of {len} bytes exceeds the {MAX_MESSAGE}-byte bound"
    );
    Ok(Some(len))
}

/// Frame a message body with the common header.
pub fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.push(VERSION);
    out.extend(((HEADER_LEN + body.len()) as u32).to_be_bytes());
    out.push(kind);
    out.extend(body);
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeerHeader {
    pub peer_type: u8,
    pub flags: u8,
    pub distinguisher: [u8; 8],
    pub address: IpAddr,
    pub asn: u32,
    pub bgp_id: Ipv4Addr,
    pub seconds: u32,
    pub micros: u32,
}

fn address(b: &[u8], v6: bool) -> IpAddr {
    let raw: [u8; 16] = b[..16].try_into().unwrap_or([0; 16]);
    if v6 {
        IpAddr::V6(Ipv6Addr::from(raw))
    } else {
        IpAddr::V4(Ipv4Addr::new(raw[12], raw[13], raw[14], raw[15]))
    }
}

fn put_address(out: &mut Vec<u8>, a: IpAddr) {
    match a {
        IpAddr::V4(v4) => {
            out.extend([0u8; 12]);
            out.extend(v4.octets());
        }
        IpAddr::V6(v6) => out.extend(v6.octets()),
    }
}

impl PeerHeader {
    pub fn read(b: &[u8]) -> Result<Self> {
        ensure!(
            b.len() >= PEER_HEADER_LEN,
            "the per-peer header is truncated"
        );
        let flags = b[1];
        Ok(Self {
            peer_type: b[0],
            flags,
            distinguisher: b[2..10].try_into()?,
            address: address(&b[10..26], flags & FLAG_V6 != 0),
            asn: u32::from_be_bytes(b[26..30].try_into()?),
            bgp_id: Ipv4Addr::from(<[u8; 4]>::try_from(&b[30..34])?),
            seconds: u32::from_be_bytes(b[34..38].try_into()?),
            micros: u32::from_be_bytes(b[38..42].try_into()?),
        })
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.peer_type);
        let v6 = if self.address.is_ipv6() { FLAG_V6 } else { 0 };
        out.push((self.flags & !FLAG_V6) | v6);
        out.extend(self.distinguisher);
        put_address(out, self.address);
        out.extend(self.asn.to_be_bytes());
        out.extend(self.bgp_id.octets());
        out.extend(self.seconds.to_be_bytes());
        out.extend(self.micros.to_be_bytes());
    }

    pub fn four_octet_as(&self) -> bool {
        self.flags & FLAG_LEGACY_AS == 0
    }

    pub fn to_json(&self) -> Json {
        let timestamp = (self.seconds != 0)
            .then(|| {
                chrono::DateTime::from_timestamp(
                    self.seconds as i64,
                    self.micros.min(999_999) * 1000,
                )
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
            })
            .flatten();
        json!({
            "type": peer_type_name(self.peer_type),
            "address": self.address.to_string(),
            "asn": self.asn,
            "bgp_id": self.bgp_id.to_string(),
            "distinguisher": distinguisher(&self.distinguisher),
            "post_policy": self.flags & FLAG_POST_POLICY != 0,
            "adj_rib_out": self.flags & FLAG_ADJ_RIB_OUT != 0,
            "four_octet_as": self.four_octet_as(),
            "timestamp": timestamp,
        })
    }
}

pub fn peer_type_name(t: u8) -> String {
    match t {
        0 => "global".into(),
        1 => "rd_instance".into(),
        2 => "local_instance".into(),
        3 => "loc_rib".into(),
        n => format!("type_{n}"),
    }
}

pub fn peer_type_code(name: &str) -> Option<u8> {
    Some(match name {
        "global" => 0,
        "rd_instance" => 1,
        "local_instance" => 2,
        "loc_rib" => 3,
        _ => return None,
    })
}

/// RFC 4364 route distinguisher text: type 0 `asn:n`, type 1 `ip:n`, type 2 `asn4:n`.
pub fn distinguisher(d: &[u8; 8]) -> String {
    let t = u16::from_be_bytes([d[0], d[1]]);
    match t {
        0 => format!(
            "{}:{}",
            u16::from_be_bytes([d[2], d[3]]),
            u32::from_be_bytes([d[4], d[5], d[6], d[7]])
        ),
        1 => format!(
            "{}:{}",
            Ipv4Addr::new(d[2], d[3], d[4], d[5]),
            u16::from_be_bytes([d[6], d[7]])
        ),
        2 => format!(
            "{}:{}",
            u32::from_be_bytes([d[2], d[3], d[4], d[5]]),
            u16::from_be_bytes([d[6], d[7]])
        ),
        _ => format!("type{t}:{}", hex::encode(&d[2..])),
    }
}

pub fn parse_distinguisher(s: &str) -> Result<[u8; 8]> {
    let (a, n) = s
        .rsplit_once(':')
        .context("a route distinguisher is admin:assigned, e.g. 65000:1 or 192.0.2.1:7")?;
    let mut d = [0u8; 8];
    if let Ok(ip) = a.parse::<Ipv4Addr>() {
        d[1] = 1;
        d[2..6].copy_from_slice(&ip.octets());
        d[6..8].copy_from_slice(&n.parse::<u16>()?.to_be_bytes());
    } else {
        let asn: u32 = a.parse().context("the distinguisher's admin part")?;
        match u16::try_from(asn) {
            Ok(a2) => {
                d[2..4].copy_from_slice(&a2.to_be_bytes());
                d[4..8].copy_from_slice(&n.parse::<u32>()?.to_be_bytes());
            }
            Err(_) => {
                d[1] = 2;
                d[2..6].copy_from_slice(&asn.to_be_bytes());
                d[6..8].copy_from_slice(&n.parse::<u16>()?.to_be_bytes());
            }
        }
    }
    Ok(d)
}

/// Type-length-value items with 2-byte type and length.
pub fn tlvs(mut b: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        ensure!(b.len() >= 4, "a TLV header is truncated");
        ensure!(out.len() < MAX_ITEMS, "more than {MAX_ITEMS} TLVs");
        let t = u16::from_be_bytes([b[0], b[1]]);
        let l = u16::from_be_bytes([b[2], b[3]]) as usize;
        ensure!(b.len() >= 4 + l, "a TLV value is truncated");
        out.push((t, &b[4..4 + l]));
        b = &b[4 + l..];
    }
    Ok(out)
}

pub fn put_tlv(out: &mut Vec<u8>, t: u16, v: &[u8]) -> Result<()> {
    let l = u16::try_from(v.len()).context("a TLV value is over 65535 bytes")?;
    out.extend(t.to_be_bytes());
    out.extend(l.to_be_bytes());
    out.extend(v);
    Ok(())
}

fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

/// The BGP PDU at the front of `b` and what follows it.
pub fn bgp_pdu(b: &[u8]) -> Result<(&[u8], &[u8])> {
    ensure!(b.len() >= wire::BGP_HEADER_LEN, "the BGP PDU is truncated");
    ensure!(
        b[..16] == wire::BGP_MARKER,
        "the BGP PDU's marker is not all ones"
    );
    let len = u16::from_be_bytes([b[16], b[17]]) as usize;
    ensure!(
        len >= wire::BGP_HEADER_LEN && len <= b.len(),
        "the BGP PDU's length {len} does not fit the BMP message"
    );
    Ok((&b[..len], &b[len..]))
}

fn open_json(pdu: &[u8]) -> Json {
    match wire::decode(pdu, true) {
        Ok(BgpMessage::Open(o)) => json!({
            "asn": o.my_asn4(),
            "hold_time": o.hold_time(),
            "bgp_id": o.bgp_id().to_string(),
            "capabilities": wire::capabilities_to_json(&o),
        }),
        Ok(_) => json!({"decode_error": "not an OPEN message"}),
        Err(e) => json!({"decode_error": e.to_string()}),
    }
}

fn notification_json(pdu: &[u8]) -> Json {
    if pdu.len() >= wire::BGP_HEADER_LEN + 2 && pdu[18] == wire::MSG_NOTIFICATION {
        let (code, sub) = (pdu[19], pdu[20]);
        json!({"code": code, "subcode": sub, "name": wire::error_name(code), "subcode_name": wire::error_subcode_name(code, sub)})
    } else {
        json!({"decode_error": "not a NOTIFICATION message"})
    }
}

pub fn termination_reason(code: u16) -> &'static str {
    match code {
        0 => "administratively_closed",
        1 => "unspecified",
        2 => "out_of_resources",
        3 => "redundant_connection",
        4 => "permanently_administratively_closed",
        _ => "unknown",
    }
}

pub fn termination_code(name: &str) -> Option<u16> {
    (0..=4).find(|c| termination_reason(*c) == name)
}

pub fn peer_down_reason(code: u8) -> &'static str {
    match code {
        1 => "local_notification",
        2 => "local_no_notification",
        3 => "remote_notification",
        4 => "remote_no_data",
        5 => "peer_deconfigured",
        6 => "local_system_closed",
        _ => "unknown",
    }
}

pub fn peer_down_code(name: &str) -> Option<u8> {
    (1..=6).find(|c| peer_down_reason(*c) == name)
}

/// IANA "BMP Statistics Types".
pub fn stat_name(t: u16) -> String {
    let name = match t {
        0 => "rejected_prefixes",
        1 => "duplicate_prefix_advertisements",
        2 => "duplicate_withdraws",
        3 => "invalidated_cluster_list_loop",
        4 => "invalidated_as_path_loop",
        5 => "invalidated_originator_id",
        6 => "invalidated_as_confed_loop",
        7 => "adj_rib_in_routes",
        8 => "loc_rib_routes",
        9 => "adj_rib_in_routes_per_afi_safi",
        10 => "loc_rib_routes_per_afi_safi",
        11 => "updates_treated_as_withdraw",
        12 => "prefixes_treated_as_withdraw",
        13 => "duplicate_update_messages",
        14 => "adj_rib_out_pre_policy_routes",
        15 => "adj_rib_out_post_policy_routes",
        16 => "adj_rib_out_pre_policy_routes_per_afi_safi",
        17 => "adj_rib_out_post_policy_routes_per_afi_safi",
        n => return format!("type_{n}"),
    };
    name.into()
}

pub fn stat_code(name: &str) -> Option<u16> {
    (0..=17).find(|c| stat_name(*c) == name)
}

/// Whether a statistics type is a 64-bit gauge (otherwise a 32-bit counter); per-AFI/SAFI
/// gauges carry AFI and SAFI before the value.
pub fn stat_shape(t: u16) -> (bool, bool) {
    match t {
        7 | 8 | 14 | 15 => (true, false),
        9 | 10 | 16 | 17 => (true, true),
        _ => (false, false),
    }
}

/// The handler's event for one message: (event id, data). Unknown message types are None.
pub fn describe(kind: u8, body: &[u8]) -> Result<Option<(&'static str, Json)>> {
    Ok(Some(match kind {
        INITIATION | TERMINATION => {
            let mut data = json!({"strings": []});
            for (t, v) in tlvs(body)? {
                match (kind, t) {
                    (_, 0) => data["strings"].as_array_mut().unwrap().push(json!(text(v))),
                    (INITIATION, 1) => data["sys_descr"] = json!(text(v)),
                    (INITIATION, 2) => data["sys_name"] = json!(text(v)),
                    (TERMINATION, 1) => {
                        ensure!(v.len() == 2, "a termination reason is two bytes");
                        let code = u16::from_be_bytes([v[0], v[1]]);
                        data["reason"] = json!(termination_reason(code));
                        data["reason_code"] = json!(code);
                    }
                    _ => {}
                }
            }
            let id = if kind == INITIATION {
                "bmp_initiation"
            } else {
                "bmp_termination"
            };
            (id, data)
        }
        ROUTE_MONITORING | STATISTICS | PEER_DOWN | PEER_UP | ROUTE_MIRRORING => {
            let peer = PeerHeader::read(body)?;
            let rest = &body[PEER_HEADER_LEN..];
            let mut data = json!({"peer": peer.to_json()});
            let id = match kind {
                ROUTE_MONITORING => {
                    let (pdu, _) = bgp_pdu(rest)?;
                    match wire::decode(pdu, peer.four_octet_as()) {
                        Ok(BgpMessage::Update(u)) => data["update"] = wire::update_to_json(&u),
                        Ok(_) => data["decode_error"] = json!("the BGP PDU is not an UPDATE"),
                        Err(e) => data["decode_error"] = json!(e.to_string()),
                    }
                    "bmp_route_monitoring"
                }
                STATISTICS => {
                    ensure!(rest.len() >= 4, "the statistics count is truncated");
                    let mut counters = Vec::new();
                    for (t, v) in tlvs(&rest[4..])? {
                        let mut c = json!({"type": stat_name(t), "type_code": t});
                        match v.len() {
                            4 => c["value"] = json!(u32::from_be_bytes(v.try_into()?)),
                            8 => c["value"] = json!(u64::from_be_bytes(v.try_into()?)),
                            11 => {
                                c["afi"] = json!(u16::from_be_bytes([v[0], v[1]]));
                                c["safi"] = json!(v[2]);
                                c["value"] = json!(u64::from_be_bytes(v[3..11].try_into()?));
                            }
                            n => c["value_length"] = json!(n),
                        }
                        counters.push(c);
                    }
                    data["counters"] = json!(counters);
                    "bmp_statistics"
                }
                PEER_DOWN => {
                    ensure!(!rest.is_empty(), "the peer down reason is missing");
                    data["reason"] = json!(peer_down_reason(rest[0]));
                    data["reason_code"] = json!(rest[0]);
                    let tail = &rest[1..];
                    match rest[0] {
                        1 | 3 => data["notification"] = notification_json(bgp_pdu(tail)?.0),
                        2 => {
                            ensure!(tail.len() >= 2, "the FSM event code is truncated");
                            data["fsm_event"] = json!(u16::from_be_bytes([tail[0], tail[1]]));
                        }
                        _ => {}
                    }
                    "bmp_peer_down"
                }
                PEER_UP => {
                    ensure!(rest.len() >= 20, "the peer up header is truncated");
                    data["local_address"] =
                        json!(address(&rest[..16], peer.flags & FLAG_V6 != 0).to_string());
                    data["local_port"] = json!(u16::from_be_bytes([rest[16], rest[17]]));
                    data["remote_port"] = json!(u16::from_be_bytes([rest[18], rest[19]]));
                    let (sent, after) = bgp_pdu(&rest[20..])?;
                    let (received, info) = bgp_pdu(after)?;
                    data["sent_open"] = open_json(sent);
                    data["received_open"] = open_json(received);
                    data["information"] = json!(tlvs(info)?
                        .into_iter()
                        .map(|(t, v)| json!({"type": t, "value": text(v)}))
                        .collect::<Vec<_>>());
                    "bmp_peer_up"
                }
                _ => {
                    let mut messages = Vec::new();
                    for (t, v) in tlvs(rest)? {
                        match t {
                            0 => messages.push(
                                match wire::decode(bgp_pdu(v)?.0, peer.four_octet_as()) {
                                    Ok(BgpMessage::Update(u)) => {
                                        json!({"update": wire::update_to_json(&u)})
                                    }
                                    Ok(m) => json!({"bgp_message": format!("{:?}", m.get_type())}),
                                    Err(e) => json!({"decode_error": e.to_string()}),
                                },
                            ),
                            1 if v.len() == 2 => messages.push(
                                json!({"information": match u16::from_be_bytes([v[0], v[1]]) {
                                    0 => "errored_pdu".to_string(),
                                    1 => "messages_lost".to_string(),
                                    n => format!("code_{n}"),
                                }}),
                            ),
                            _ => {}
                        }
                    }
                    data["messages"] = json!(messages);
                    "bmp_route_mirroring"
                }
            };
            (id, data)
        }
        _ => return Ok(None),
    }))
}

/// A message in its frame, from the exporter's side.
pub fn initiation(sys_name: &str, sys_descr: &str, strings: &[String]) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    put_tlv(&mut b, 2, sys_name.as_bytes())?;
    put_tlv(&mut b, 1, sys_descr.as_bytes())?;
    for s in strings {
        put_tlv(&mut b, 0, s.as_bytes())?;
    }
    Ok(frame(INITIATION, &b))
}

pub fn termination(reason: u16, message: Option<&str>) -> Result<Vec<u8>> {
    let mut b = Vec::new();
    if let Some(m) = message {
        put_tlv(&mut b, 0, m.as_bytes())?;
    }
    put_tlv(&mut b, 1, &reason.to_be_bytes())?;
    Ok(frame(TERMINATION, &b))
}

pub fn with_peer(kind: u8, peer: &PeerHeader, rest: &[u8]) -> Result<Vec<u8>> {
    let mut b = Vec::with_capacity(PEER_HEADER_LEN + rest.len());
    peer.write(&mut b);
    b.extend(rest);
    if b.len() + HEADER_LEN > MAX_MESSAGE {
        bail!("the BMP message would exceed {MAX_MESSAGE} bytes");
    }
    Ok(frame(kind, &b))
}
