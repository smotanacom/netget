//! PFCP (3GPP TS 29.244) framing and information elements, shared by the UPF-side server and
//! the SMF-side client (`src/client/pfcp/`).
//!
//! IEs are carried to and from the model as a JSON object keyed by IE name. A grouped IE is an
//! object of the same shape, and a key whose value is an array is that IE repeated
//! (`"create_pdr": [{…}, {…}]`). The table below gives the IEs a model needs for associations
//! and sessions their own readable forms (addresses, flag names, cause names). Any other IE
//! keeps its number and its bytes, as `"ie_<type>": {"hex": "…"}`, so nothing is lost on the
//! way through.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::net::{Ipv4Addr, Ipv6Addr};

pub const PORT: u16 = 8805;
pub const MAX_DATAGRAM: usize = 65_507;
/// Grouped IEs nest at most this deep (Create PDR > PDI > … is three or four).
pub const MAX_DEPTH: usize = 8;
/// IEs in one message, counting every nested one.
pub const MAX_IES: usize = 1024;
/// Seconds between 1900 (NTP) and 1970.
const NTP_UNIX: u64 = 2_208_988_800;

pub const MESSAGES: &[(u8, &str)] = &[
    (1, "heartbeat_request"),
    (2, "heartbeat_response"),
    (3, "pfd_management_request"),
    (4, "pfd_management_response"),
    (5, "association_setup_request"),
    (6, "association_setup_response"),
    (7, "association_update_request"),
    (8, "association_update_response"),
    (9, "association_release_request"),
    (10, "association_release_response"),
    (11, "version_not_supported_response"),
    (12, "node_report_request"),
    (13, "node_report_response"),
    (14, "session_set_deletion_request"),
    (15, "session_set_deletion_response"),
    (50, "session_establishment_request"),
    (51, "session_establishment_response"),
    (52, "session_modification_request"),
    (53, "session_modification_response"),
    (54, "session_deletion_request"),
    (55, "session_deletion_response"),
    (56, "session_report_request"),
    (57, "session_report_response"),
];

pub fn message_name(t: u8) -> String {
    MESSAGES
        .iter()
        .find(|(n, _)| *n == t)
        .map(|(_, s)| s.to_string())
        .unwrap_or_else(|| format!("message_{t}"))
}

pub fn message_type(name: &str) -> Result<u8> {
    MESSAGES
        .iter()
        .find(|(_, s)| *s == name)
        .map(|(n, _)| *n)
        .with_context(|| format!("{name:?} is not a PFCP message name"))
}

/// Session-related messages (type ≥ 50) carry a SEID in their header.
pub fn has_seid(t: u8) -> bool {
    t >= 50
}

/// The request types (TS 29.244 §7.3); each one's response is the next number.
pub fn is_request(t: u8) -> bool {
    matches!(t, 1 | 3 | 5 | 7 | 9 | 12 | 14 | 16 | 50 | 52 | 54 | 56)
}

pub const CAUSES: &[(u8, &str)] = &[
    (1, "request_accepted"),
    (2, "more_usage_report_to_send"),
    (64, "request_rejected"),
    (65, "session_context_not_found"),
    (66, "mandatory_ie_missing"),
    (67, "conditional_ie_missing"),
    (68, "invalid_length"),
    (69, "mandatory_ie_incorrect"),
    (70, "invalid_forwarding_policy"),
    (71, "invalid_f_teid_allocation_option"),
    (72, "no_established_pfcp_association"),
    (73, "rule_creation_modification_failure"),
    (74, "pfcp_entity_in_congestion"),
    (75, "no_resources_available"),
    (76, "service_not_supported"),
    (77, "system_failure"),
    (78, "redirection_requested"),
];

pub fn cause_name(c: u8) -> String {
    CAUSES
        .iter()
        .find(|(n, _)| *n == c)
        .map(|(_, s)| s.to_string())
        .unwrap_or_else(|| format!("cause_{c}"))
}

pub fn cause_code(name: &str) -> Result<u8> {
    CAUSES
        .iter()
        .find(|(_, s)| *s == name)
        .map(|(n, _)| *n)
        .or_else(|| name.strip_prefix("cause_").and_then(|n| n.parse().ok()))
        .with_context(|| {
            format!(
                "cause {name:?} is not one of {}",
                CAUSES
                    .iter()
                    .map(|(_, s)| *s)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Grouped,
    U8,
    U16,
    U32,
    Cause,
    NodeId,
    FSeid,
    FTeid,
    UeIp,
    ApplyAction,
    Interface,
    Text,
    Recovery,
    OuterHeaderCreation,
    OuterHeaderRemoval,
    Hex,
}

const IES: &[(u16, &str, K)] = &[
    (1, "create_pdr", K::Grouped),
    (2, "pdi", K::Grouped),
    (3, "create_far", K::Grouped),
    (4, "forwarding_parameters", K::Grouped),
    (5, "duplicating_parameters", K::Grouped),
    (6, "create_urr", K::Grouped),
    (7, "create_qer", K::Grouped),
    (8, "created_pdr", K::Grouped),
    (9, "update_pdr", K::Grouped),
    (10, "update_far", K::Grouped),
    (11, "update_forwarding_parameters", K::Grouped),
    (12, "update_bar_session_report_response", K::Grouped),
    (13, "update_urr", K::Grouped),
    (14, "update_qer", K::Grouped),
    (15, "remove_pdr", K::Grouped),
    (16, "remove_far", K::Grouped),
    (17, "remove_urr", K::Grouped),
    (18, "remove_qer", K::Grouped),
    (19, "cause", K::Cause),
    (20, "source_interface", K::Interface),
    (21, "f_teid", K::FTeid),
    (22, "network_instance", K::Text),
    (23, "sdf_filter", K::Hex),
    (24, "application_id", K::Text),
    (25, "gate_status", K::U8),
    (26, "mbr", K::Hex),
    (27, "gbr", K::Hex),
    (28, "qer_correlation_id", K::U32),
    (29, "precedence", K::U32),
    (37, "measurement_method", K::U8),
    (39, "report_type", K::U8),
    (40, "offending_ie", K::U16),
    (42, "destination_interface", K::Interface),
    (43, "up_function_features", K::Hex),
    (44, "apply_action", K::ApplyAction),
    (54, "downlink_data_service_information", K::Hex),
    (56, "pdr_id", K::U16),
    (57, "f_seid", K::FSeid),
    (60, "node_id", K::NodeId),
    (81, "urr_id", K::U32),
    (83, "downlink_data_report", K::Grouped),
    (84, "outer_header_creation", K::OuterHeaderCreation),
    (85, "create_bar", K::Grouped),
    (86, "update_bar", K::Grouped),
    (87, "remove_bar", K::Grouped),
    (88, "bar_id", K::U8),
    (89, "cp_function_features", K::Hex),
    (93, "ue_ip_address", K::UeIp),
    (95, "outer_header_removal", K::OuterHeaderRemoval),
    (96, "recovery_time_stamp", K::Recovery),
    (108, "far_id", K::U32),
    (109, "qer_id", K::U32),
    (111, "pdn_type", K::U8),
    (114, "failed_rule_id", K::Hex),
    (124, "qfi", K::U8),
    (155, "network_instance_dnn", K::Text),
];

fn by_type(t: u16) -> Option<(&'static str, K)> {
    IES.iter()
        .find(|(n, _, _)| *n == t)
        .map(|(_, s, k)| (*s, *k))
}

fn by_name(name: &str) -> Result<(u16, K)> {
    if let Some((t, _, k)) = IES.iter().find(|(_, s, _)| *s == name) {
        return Ok((*t, *k));
    }
    if let Some(n) = name.strip_prefix("ie_").and_then(|n| n.parse().ok()) {
        return Ok((n, K::Hex));
    }
    bail!("{name:?} is not a PFCP IE NetGet knows; give unknown ones as ie_<type> with {{\"hex\": ...}}")
}

const INTERFACES: &[&str] = &[
    "access",
    "core",
    "sgi_lan_n6_lan",
    "cp_function",
    "5g_vn_internal",
];
const APPLY: &[&str] = &[
    "DROP", "FORW", "BUFF", "NOCP", "DUPL", "IPMA", "IPMD", "DFRT", "EDRT", "BDPN", "DDPN",
];
const OHC: &[&str] = &[
    "gtpu_udp_ipv4",
    "gtpu_udp_ipv6",
    "udp_ipv4",
    "udp_ipv6",
    "ipv4",
    "ipv6",
    "c_tag",
    "s_tag",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub message_type: u8,
    pub seid: Option<u64>,
    pub sequence: u32,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub header: Header,
    /// The IEs in the model's form.
    pub ies: Value,
}

/// Parse one datagram. Further messages after the first (follow-on) are ignored.
pub fn parse(b: &[u8]) -> Result<Message> {
    ensure!(b.len() <= MAX_DATAGRAM, "a {}-byte datagram", b.len());
    ensure!(
        b.len() >= 8,
        "a {}-byte datagram is shorter than a PFCP header",
        b.len()
    );
    let version = b[0] >> 5;
    ensure!(version == 1, "PFCP version {version}");
    let s = b[0] & 0x01 != 0;
    let message_type = b[1];
    let length = u16::from_be_bytes([b[2], b[3]]) as usize;
    ensure!(
        4 + length <= b.len(),
        "Message Length {length} runs past the datagram"
    );
    let body = &b[4..4 + length];
    let (seid, rest) = if s {
        ensure!(body.len() >= 12, "a truncated header");
        (Some(u64::from_be_bytes(body[..8].try_into()?)), &body[8..])
    } else {
        ensure!(body.len() >= 4, "a truncated header");
        (None, body)
    };
    let sequence = u32::from_be_bytes([0, rest[0], rest[1], rest[2]]);
    let mut count = 0;
    let ies = decode_ies(&rest[4..], 0, &mut count)?;
    Ok(Message {
        header: Header {
            message_type,
            seid,
            sequence,
        },
        ies,
    })
}

fn decode_ies(mut b: &[u8], depth: usize, count: &mut usize) -> Result<Value> {
    ensure!(
        depth < MAX_DEPTH,
        "grouped IEs nest more than {MAX_DEPTH} deep"
    );
    let mut out = Map::new();
    let mut repeated = std::collections::HashSet::new();
    while !b.is_empty() {
        ensure!(b.len() >= 4, "a truncated IE header");
        let t = u16::from_be_bytes([b[0], b[1]]);
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        ensure!(4 + len <= b.len(), "IE {t} runs past its container");
        *count += 1;
        ensure!(*count <= MAX_IES, "more than {MAX_IES} IEs");
        // Enterprise-specific IEs (bit 15) carry a 2-byte enterprise id first; kept as bytes.
        let v = &b[4..4 + len];
        let (name, value) = match by_type(t) {
            Some((name, kind)) if t & 0x8000 == 0 => (
                name.to_string(),
                decode_value(kind, v, depth, count).with_context(|| format!("IE {name}"))?,
            ),
            _ => (format!("ie_{t}"), json!({"hex": hex::encode(v)})),
        };
        match out.get_mut(&name) {
            None => {
                out.insert(name, value);
            }
            Some(existing) => {
                // A repeated IE becomes an array of its values.
                if repeated.contains(&name) {
                    if let Value::Array(list) = existing {
                        list.push(value);
                    }
                } else {
                    let first = existing.take();
                    *existing = Value::Array(vec![first, value]);
                    repeated.insert(name);
                }
            }
        }
        b = &b[4 + len..];
    }
    Ok(Value::Object(out))
}

fn ip4(b: &[u8]) -> Result<Ipv4Addr> {
    ensure!(b.len() >= 4, "a truncated IPv4 address");
    Ok(Ipv4Addr::new(b[0], b[1], b[2], b[3]))
}

fn ip6(b: &[u8]) -> Result<Ipv6Addr> {
    ensure!(b.len() >= 16, "a truncated IPv6 address");
    let a: [u8; 16] = b[..16].try_into()?;
    Ok(Ipv6Addr::from(a))
}

fn flags(bits: u32, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .map(|(_, n)| n.to_string())
        .collect()
}

/// A Network Instance: DNS label encoding when it parses as one, else the text.
fn text(v: &[u8]) -> String {
    let mut labels = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let n = v[i] as usize;
        if n == 0
            || i + 1 + n > v.len()
            || !v[i + 1..i + 1 + n].iter().all(|c| c.is_ascii_graphic())
        {
            return String::from_utf8_lossy(v).into_owned();
        }
        labels.push(String::from_utf8_lossy(&v[i + 1..i + 1 + n]).into_owned());
        i += 1 + n;
    }
    if labels.len() > 1 {
        labels.join(".")
    } else {
        String::from_utf8_lossy(v).into_owned()
    }
}

fn decode_value(kind: K, v: &[u8], depth: usize, count: &mut usize) -> Result<Value> {
    let need = |n: usize| -> Result<()> {
        ensure!(v.len() >= n, "{} bytes where {n} are needed", v.len());
        Ok(())
    };
    Ok(match kind {
        K::Grouped => decode_ies(v, depth + 1, count)?,
        K::U8 => {
            need(1)?;
            json!(v[0])
        }
        K::U16 => {
            need(2)?;
            json!(u16::from_be_bytes([v[0], v[1]]))
        }
        K::U32 => {
            need(4)?;
            json!(u32::from_be_bytes(v[..4].try_into()?))
        }
        K::Cause => {
            need(1)?;
            json!(cause_name(v[0]))
        }
        K::NodeId => {
            need(1)?;
            match v[0] & 0x0f {
                0 => json!({"ipv4": ip4(&v[1..])?.to_string()}),
                1 => json!({"ipv6": ip6(&v[1..])?.to_string()}),
                2 => json!({"fqdn": text(&v[1..])}),
                other => bail!("Node ID type {other}"),
            }
        }
        K::FSeid => {
            need(9)?;
            let mut o = json!({"seid": u64::from_be_bytes(v[1..9].try_into()?)});
            let mut at = 9;
            if v[0] & 0x02 != 0 {
                o["ipv4"] = json!(ip4(&v[at..])?.to_string());
                at += 4;
            }
            if v[0] & 0x01 != 0 {
                o["ipv6"] = json!(ip6(&v[at..])?.to_string());
            }
            o
        }
        K::FTeid => {
            need(1)?;
            let f = v[0];
            if f & 0x04 != 0 {
                // CH: "choose one for me"; with CHID, one more byte follows.
                let mut o = json!({"choose": true, "ipv4": f & 0x01 != 0, "ipv6": f & 0x02 != 0});
                if f & 0x08 != 0 {
                    need(2)?;
                    o["choose_id"] = json!(v[1]);
                }
                o
            } else {
                need(5)?;
                let mut o = json!({"teid": u32::from_be_bytes(v[1..5].try_into()?)});
                let mut at = 5;
                if f & 0x01 != 0 {
                    o["ipv4"] = json!(ip4(&v[at..])?.to_string());
                    at += 4;
                }
                if f & 0x02 != 0 {
                    o["ipv6"] = json!(ip6(&v[at..])?.to_string());
                }
                o
            }
        }
        K::UeIp => {
            need(1)?;
            let f = v[0];
            let mut o = json!({"direction": if f & 0x04 != 0 { "destination" } else { "source" }});
            let mut at = 1;
            if f & 0x02 != 0 {
                o["ipv4"] = json!(ip4(&v[at..])?.to_string());
                at += 4;
            }
            if f & 0x01 != 0 {
                o["ipv6"] = json!(ip6(&v[at..])?.to_string());
            }
            o
        }
        K::ApplyAction => {
            need(1)?;
            let bits = u32::from(v[0]) | v.get(1).map_or(0, |b| u32::from(*b) << 8);
            json!(flags(bits, APPLY))
        }
        K::Interface => {
            need(1)?;
            let i = (v[0] & 0x0f) as usize;
            json!(INTERFACES
                .get(i)
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("interface_{i}")))
        }
        K::Text => json!(text(v)),
        K::Recovery => {
            need(4)?;
            let ntp = u64::from(u32::from_be_bytes(v[..4].try_into()?));
            json!(ntp.saturating_sub(NTP_UNIX))
        }
        K::OuterHeaderCreation => {
            need(2)?;
            // Octet 5 is the description (bit 1 GTP-U/UDP/IPv4 … bit 8 S-TAG); octet 6 holds
            // the N19/N6 indications.
            let d = v[0];
            let mut o = json!({"description": flags(u32::from(d), OHC)});
            let mut at = 2;
            if d & 0x03 != 0 {
                need(at + 4)?;
                o["teid"] = json!(u32::from_be_bytes(v[at..at + 4].try_into()?));
                at += 4;
            }
            if d & 0x15 != 0 {
                o["ipv4"] = json!(ip4(&v[at..])?.to_string());
                at += 4;
            }
            if d & 0x2a != 0 {
                o["ipv6"] = json!(ip6(&v[at..])?.to_string());
                at += 16;
            }
            if d & 0x0c != 0 {
                need(at + 2)?;
                o["port"] = json!(u16::from_be_bytes([v[at], v[at + 1]]));
            }
            o
        }
        K::OuterHeaderRemoval => {
            need(1)?;
            json!(v[0])
        }
        K::Hex => json!({"hex": hex::encode(v)}),
    })
}

/// Encode a message: header, then the IEs of `ies` (the model's form).
pub fn encode(message_type: u8, seid: Option<u64>, sequence: u32, ies: &Value) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    encode_ies(ies, &mut body, 0)?;
    let mut out = Vec::with_capacity(16 + body.len());
    out.push((1 << 5) | u8::from(seid.is_some()));
    out.push(message_type);
    let len = body.len() + 4 + if seid.is_some() { 8 } else { 0 };
    ensure!(
        len <= u16::MAX as usize,
        "a {len}-byte message is too long for PFCP"
    );
    out.extend_from_slice(&(len as u16).to_be_bytes());
    if let Some(s) = seid {
        out.extend_from_slice(&s.to_be_bytes());
    }
    out.extend_from_slice(&sequence.to_be_bytes()[1..]);
    out.push(0);
    out.extend_from_slice(&body);
    ensure!(
        out.len() <= MAX_DATAGRAM,
        "the message is too large for one datagram"
    );
    Ok(out)
}

fn encode_ies(ies: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    ensure!(
        depth < MAX_DEPTH,
        "grouped IEs nest more than {MAX_DEPTH} deep"
    );
    let map = match ies {
        Value::Object(m) => m,
        Value::Null => return Ok(()),
        _ => bail!("IEs are an object keyed by IE name"),
    };
    for (name, value) in map {
        let (t, kind) = by_name(name)?;
        // An array is the IE repeated, except apply_action, whose own value is a list of
        // flag names (a list of such lists is the repetition).
        let repeated = match (kind, value) {
            (K::ApplyAction, Value::Array(list)) => list.iter().all(Value::is_array),
            (_, Value::Array(_)) => true,
            _ => false,
        };
        let items: Vec<&Value> = match value {
            Value::Array(list) if repeated => list.iter().collect(),
            other => vec![other],
        };
        for item in items {
            let mut v = Vec::new();
            encode_value(kind, item, &mut v, depth).with_context(|| format!("IE {name}"))?;
            ensure!(v.len() <= u16::MAX as usize, "IE {name} is too long");
            out.extend_from_slice(&t.to_be_bytes());
            out.extend_from_slice(&(v.len() as u16).to_be_bytes());
            out.extend_from_slice(&v);
        }
    }
    Ok(())
}

fn num(v: &Value) -> Result<u64> {
    v.as_u64().context("a whole number")
}

fn parse_ip4(v: &Value) -> Result<Ipv4Addr> {
    v.as_str()
        .context("an IPv4 address string")?
        .parse()
        .context("an IPv4 address")
}

fn parse_ip6(v: &Value) -> Result<Ipv6Addr> {
    v.as_str()
        .context("an IPv6 address string")?
        .parse()
        .context("an IPv6 address")
}

fn flag_bits(v: &Value, names: &[&str]) -> Result<u32> {
    let list: Vec<&str> = match v {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().context("flag names are strings"))
            .collect::<Result<_>>()?,
        _ => bail!("one of {} or a list of them", names.join(", ")),
    };
    let mut bits = 0;
    for f in list {
        let i = names
            .iter()
            .position(|n| n.eq_ignore_ascii_case(f))
            .with_context(|| format!("{f:?} is not one of {}", names.join(", ")))?;
        bits |= 1 << i;
    }
    Ok(bits)
}

fn encode_value(kind: K, v: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    match kind {
        K::Grouped => encode_ies(v, out, depth + 1)?,
        K::U8 => out.push(u8::try_from(num(v)?).context("0-255")?),
        K::U16 => out.extend_from_slice(&u16::try_from(num(v)?).context("0-65535")?.to_be_bytes()),
        K::U32 => out.extend_from_slice(
            &u32::try_from(num(v)?)
                .context("a 32-bit number")?
                .to_be_bytes(),
        ),
        K::Cause => {
            let c = match v {
                Value::String(s) => cause_code(s)?,
                n => u8::try_from(num(n)?).context("0-255")?,
            };
            out.push(c);
        }
        K::NodeId => {
            if let Some(a) = v.get("ipv4") {
                out.push(0);
                out.extend_from_slice(&parse_ip4(a)?.octets());
            } else if let Some(a) = v.get("ipv6") {
                out.push(1);
                out.extend_from_slice(&parse_ip6(a)?.octets());
            } else if let Some(f) = v.get("fqdn").and_then(Value::as_str) {
                out.push(2);
                for label in f.split('.').filter(|l| !l.is_empty()) {
                    ensure!(label.len() < 64, "an FQDN label is longer than 63 bytes");
                    out.push(label.len() as u8);
                    out.extend_from_slice(label.as_bytes());
                }
            } else {
                bail!("node_id is {{\"ipv4\": …}}, {{\"ipv6\": …}} or {{\"fqdn\": …}}");
            }
        }
        K::FSeid => {
            let (v4, v6) = (v.get("ipv4"), v.get("ipv6"));
            out.push(u8::from(v4.is_some()) << 1 | u8::from(v6.is_some()));
            out.extend_from_slice(&num(v.get("seid").context("f_seid needs seid")?)?.to_be_bytes());
            if let Some(a) = v4 {
                out.extend_from_slice(&parse_ip4(a)?.octets());
            }
            if let Some(a) = v6 {
                out.extend_from_slice(&parse_ip6(a)?.octets());
            }
        }
        K::FTeid => {
            if v.get("choose").and_then(Value::as_bool) == Some(true) {
                let v4 = v.get("ipv4").and_then(Value::as_bool).unwrap_or(true);
                let v6 = v.get("ipv6").and_then(Value::as_bool).unwrap_or(false);
                let chid = v.get("choose_id").map(num).transpose()?;
                out.push(0x04 | u8::from(v4) | u8::from(v6) << 1 | u8::from(chid.is_some()) << 3);
                if let Some(id) = chid {
                    out.push(u8::try_from(id).context("choose_id is 0-255")?);
                }
            } else {
                let (v4, v6) = (v.get("ipv4"), v.get("ipv6"));
                out.push(u8::from(v4.is_some()) | u8::from(v6.is_some()) << 1);
                out.extend_from_slice(
                    &u32::try_from(num(v
                        .get("teid")
                        .context("f_teid needs teid (or choose: true)")?)?)?
                    .to_be_bytes(),
                );
                if let Some(a) = v4 {
                    out.extend_from_slice(&parse_ip4(a)?.octets());
                }
                if let Some(a) = v6 {
                    out.extend_from_slice(&parse_ip6(a)?.octets());
                }
            }
        }
        K::UeIp => {
            let (v4, v6) = (v.get("ipv4"), v.get("ipv6"));
            let dest = v.get("direction").and_then(Value::as_str) == Some("destination");
            out.push(u8::from(dest) << 2 | u8::from(v4.is_some()) << 1 | u8::from(v6.is_some()));
            if let Some(a) = v4 {
                out.extend_from_slice(&parse_ip4(a)?.octets());
            }
            if let Some(a) = v6 {
                out.extend_from_slice(&parse_ip6(a)?.octets());
            }
        }
        K::ApplyAction => {
            let bits = flag_bits(v, APPLY)?;
            out.push(bits as u8);
            if bits > 0xff {
                out.push((bits >> 8) as u8);
            }
        }
        K::Interface => {
            let i = match v {
                Value::String(s) => INTERFACES.iter().position(|n| n == s).with_context(|| {
                    format!("interface {s:?} is not one of {}", INTERFACES.join(", "))
                })?,
                n => num(n)? as usize,
            };
            out.push(u8::try_from(i).context("an interface number")? & 0x0f);
        }
        K::Text => out.extend_from_slice(v.as_str().context("text")?.as_bytes()),
        K::Recovery => {
            let unix = num(v)?;
            out.extend_from_slice(&((unix + NTP_UNIX) as u32).to_be_bytes());
        }
        K::OuterHeaderCreation => {
            let d = flag_bits(
                v.get("description")
                    .context("outer_header_creation needs description")?,
                OHC,
            )? as u8;
            ensure!(
                d & 0xc0 == 0,
                "C-TAG and S-TAG outer headers are not supported"
            );
            out.extend_from_slice(&[d, 0]);
            if d & 0x03 != 0 {
                out.extend_from_slice(
                    &u32::try_from(num(v.get("teid").context("GTP-U needs teid")?)?)?.to_be_bytes(),
                );
            }
            if d & 0x15 != 0 {
                out.extend_from_slice(
                    &parse_ip4(v.get("ipv4").context("an IPv4 outer header needs ipv4")?)?.octets(),
                );
            }
            if d & 0x2a != 0 {
                out.extend_from_slice(
                    &parse_ip6(v.get("ipv6").context("an IPv6 outer header needs ipv6")?)?.octets(),
                );
            }
            if d & 0x0c != 0 {
                out.extend_from_slice(
                    &u16::try_from(num(v.get("port").context("UDP needs port")?)?)?.to_be_bytes(),
                );
            }
        }
        K::OuterHeaderRemoval => out.push(u8::try_from(num(v)?).context("0-255")?),
        K::Hex => {
            let h = v
                .get("hex")
                .and_then(Value::as_str)
                .or(v.as_str())
                .context("give this IE as {\"hex\": \"…\"}")?;
            out.extend(hex::decode(h).context("not hex")?);
        }
    }
    Ok(())
}

/// The current time as a recovery time stamp's unix seconds.
pub fn now_unix() -> u64 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
