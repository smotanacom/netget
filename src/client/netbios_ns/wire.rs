//! NetBIOS Name Service **client** wire helpers (RFC 1001 §14, RFC 1002 §4).
//!
//! Pure functions, no I/O and no LLM. This module deliberately owns only the two directions
//! the server half has no use for — **encoding a request** and **decoding a response** —
//! because everything else already exists and must not be written twice:
//!
//! | Concern | Where it lives |
//! |---|---|
//! | first-level name encoding, padding, the wildcard's NUL rule | [`crate::server::netbios_ns::packet`] |
//! | the 12-octet header, opcodes, flags, rcodes, QTYPEs | same |
//! | MAC formatting, `NodeType` | same |
//! | building a query datagram | here |
//! | parsing a NAME QUERY / NODE STATUS / negative response | here |
//!
//! Sharing the codec is the point: `bgp`, `kafka` and `websocket` clients do the same thing
//! with their server halves. A second copy of `pad_netbios_name` in this file would be a
//! second place for the wildcard-padding bug to come back.
//!
//! # The two traps that cost real debugging on the server side
//!
//! Both are inherited from `packet.rs` rather than re-solved here, and both are asserted
//! against a real Samba datagram in `tests/client/netbios_ns/e2e_test.rs`:
//!
//! 1. **The wildcard `*` pads with NUL, not space** (RFC 1001 §17), so it encodes to
//!    `CKAAAA…` and not `CKCACA…`. Every node status query uses it.
//! 2. **`trim_end()` does not strip NULs**, so a name decoded out of a response has to be
//!    trimmed with `trim_end_matches([' ', '\0'])` — which is what
//!    [`crate::server::netbios_ns::packet::split_netbios_name`] does.

use anyhow::{bail, Context, Result};
use std::net::Ipv4Addr;

use crate::server::netbios_ns::packet as pkt;

/// A NetBIOS name query is single-shot per action, and 576 octets is the RFC 1002 §4.1 cap,
/// so this is the largest datagram we ever need to read.
pub const RECV_BUFFER: usize = pkt::MAX_DATAGRAM;

// ===========================================================================================
// Suffix decoding
// ===========================================================================================

/// Human label for a NetBIOS service suffix.
///
/// The suffix is the 16th octet of every NetBIOS name and it is the *service selector*:
/// `FILESERVER<0x00>` (the workstation service) and `FILESERVER<0x20>` (the file server
/// service) are different names that may resolve to different hosts. It is therefore surfaced
/// to the model as its own structured field and never folded into the name string; this
/// function only adds a readable label alongside it.
///
/// The table is the documented Microsoft NetBIOS suffix assignment. Anything not listed
/// returns `"unknown"` rather than a guess — inventing a label for an unassigned suffix would
/// be putting a claim about a service into the model's context that nothing supports.
pub fn suffix_label(suffix: u8) -> &'static str {
    match suffix {
        0x00 => "workstation",
        0x01 => "messenger_or_master_browser",
        0x03 => "messenger",
        0x06 => "ras_server",
        0x1b => "domain_master_browser",
        0x1c => "domain_controllers",
        0x1d => "master_browser",
        0x1e => "browser_service_elections",
        0x1f => "net_dde",
        0x20 => "file_server",
        0x21 => "ras_client",
        0x22 => "exchange_interchange",
        0x23 => "exchange_store",
        0x24 => "exchange_directory",
        0x30 => "modem_sharing_server",
        0x31 => "modem_sharing_client",
        0x43 => "sms_client_remote_control",
        0x44 => "sms_admin_remote_transfer",
        0x45 => "sms_client_remote_chat",
        0x46 => "sms_client_remote_transfer",
        0x4c => "dec_pathworks_tcpip",
        0x52 => "dec_pathworks_tcpip",
        0x6a => "exchange_im_service",
        0x87 => "exchange_ms_mail_connector",
        0xbe => "network_monitor_agent",
        0xbf => "network_monitor_utility",
        _ => "unknown",
    }
}

/// Human name for an RCODE, so a refusal reaches the model as `"name_not_found"` rather than
/// as the number 3. The spellings match
/// [`crate::server::netbios_ns::packet::rcode_by_name`], so a value that comes out of a
/// response can be handed straight back into a NetGet NBNS server's action.
pub fn rcode_label(rcode: u16) -> &'static str {
    match rcode {
        pkt::RCODE_OK => "ok",
        pkt::RCODE_FMT_ERR => "format_error",
        pkt::RCODE_SRV_ERR => "server_failure",
        pkt::RCODE_NAM_ERR => "name_not_found",
        pkt::RCODE_IMP_ERR => "unsupported_request",
        pkt::RCODE_RFS_ERR => "refused",
        pkt::RCODE_ACT_ERR => "name_active",
        pkt::RCODE_CFT_ERR => "name_conflict",
        _ => "unknown",
    }
}

/// The `ONT` bits of an `NB_FLAGS` / `NAME_FLAGS` word, as `b`/`p`/`m`/`h`.
///
/// `packet::NodeType` can turn a node type *into* these bits; nothing there reads them back,
/// because a server chooses the value and a client observes it.
pub fn node_type_label(flags: u16) -> &'static str {
    match (flags >> 13) & 0b11 {
        0 => "b",
        1 => "p",
        2 => "m",
        _ => "h",
    }
}

// ===========================================================================================
// Requests
// ===========================================================================================

/// Build a NAME QUERY REQUEST (RFC 1002 §4.2.12).
///
/// `broadcast` selects the flag word:
///
/// * `false` — `FLAGS = 0x0000`, which is byte-for-byte what Samba's `nmblookup -U <addr>
///   <NAME>` puts on the wire for a directed query to a name server. This is the default, and
///   `tests/client/netbios_ns/e2e_test.rs` asserts the whole datagram against a captured
///   `nmblookup` one.
/// * `true` — `B|RD` (`0x0110`), the broadcast form: "anyone holding this name, answer", with
///   recursion desired set as RFC 1002 §4.2.12 shows for the broadcast case.
///
/// NetGet never actually sends to a broadcast address on its own initiative — the caller
/// supplies the destination — so this flag describes the *question*, not the routing.
pub fn encode_name_query(trn_id: u16, name: &str, suffix: u8, broadcast: bool) -> Result<Vec<u8>> {
    let flags = if broadcast {
        pkt::NM_FLAG_B | pkt::NM_FLAG_RD
    } else {
        0
    };
    encode_query(trn_id, flags, pkt::QTYPE_NB, name, suffix)
}

/// Build a NODE STATUS REQUEST (RFC 1002 §4.2.17) — the `nbtstat -A` question.
///
/// `FLAGS` is always zero: a node status request is answered by the node itself and there is
/// nothing to recurse to, which is also exactly what `nmblookup -A` sends.
pub fn encode_node_status_query(trn_id: u16, name: &str, suffix: u8) -> Result<Vec<u8>> {
    encode_query(trn_id, 0, pkt::QTYPE_NBSTAT, name, suffix)
}

fn encode_query(trn_id: u16, flags: u16, qtype: u16, name: &str, suffix: u8) -> Result<Vec<u8>> {
    let header = pkt::Header {
        trn_id,
        flags,
        qdcount: 1,
        ancount: 0,
        nscount: 0,
        arcount: 0,
    };
    // `encode_name_field` applies the wildcard's NUL padding and the first-level encoding.
    let name_field = pkt::encode_name_field(name, suffix, None)?;

    let mut out = Vec::with_capacity(pkt::HEADER_LEN + name_field.len() + 4);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&name_field);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&pkt::CLASS_IN.to_be_bytes());
    Ok(out)
}

// ===========================================================================================
// Responses
// ===========================================================================================

/// A POSITIVE NAME QUERY RESPONSE (RFC 1002 §4.2.13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameAnswer {
    pub name: String,
    pub suffix: u8,
    pub addresses: Vec<Ipv4Addr>,
    pub ttl: u32,
    pub group: bool,
    pub node_type: &'static str,
}

/// One entry of a NODE STATUS RESPONSE name list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatusName {
    pub name: String,
    pub suffix: u8,
    pub group: bool,
    pub active: bool,
}

/// A NODE STATUS RESPONSE (RFC 1002 §4.2.18) — the whole point of the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStatusAnswer {
    /// The name the request asked about, echoed back (`*` for the usual wildcard question).
    pub name: String,
    pub suffix: u8,
    pub names: Vec<NodeStatusName>,
    /// Formatted `"00:11:22:33:44:55"`, never raw bytes.
    pub mac_address: String,
}

/// A NEGATIVE NAME QUERY RESPONSE (RFC 1002 §4.2.14), or any response carrying a non-zero
/// RCODE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegativeAnswer {
    pub name: String,
    pub suffix: u8,
    pub rcode: u16,
    pub rcode_name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NbnsAnswer {
    Name(NameAnswer),
    NodeStatus(NodeStatusAnswer),
    Negative(NegativeAnswer),
}

/// A decoded response datagram plus the transaction id it claims to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NbnsResponse {
    pub trn_id: u16,
    pub answer: NbnsAnswer,
}

/// Parse a response datagram.
///
/// Refuses anything with `R=0`: a request is not an answer, and treating one as an answer is
/// how a client ends up caching a name from a datagram nobody asked for. The caller still has
/// to check the transaction id — see [`NbnsResponse::trn_id`] and the matching loop in
/// `mod.rs`; this function reports the id, it does not decide whether it is the right one.
pub fn parse_response(buf: &[u8]) -> Result<NbnsResponse> {
    if buf.len() > pkt::MAX_DATAGRAM {
        bail!(
            "NBNS datagram is {} octets; RFC 1002 §4.1 caps it at {}",
            buf.len(),
            pkt::MAX_DATAGRAM
        );
    }
    let header = pkt::Header::parse(buf)?;
    if !header.is_response() {
        bail!("datagram is a request (R=0), not a response");
    }

    // Some implementations echo the question section back; RFC 1002's own examples set
    // QDCOUNT to 0. Skip whatever is there rather than assuming either shape.
    let mut cursor = pkt::HEADER_LEN;
    for _ in 0..header.qdcount {
        let question = pkt::read_name_field(buf, cursor)?;
        cursor = question
            .end
            .checked_add(4)
            .context("response truncated inside an echoed question")?;
    }

    let rcode = header.rcode();
    if header.ancount == 0 {
        // A refusal with no answer RR is unusual but unambiguous; a success with no answer RR
        // carries nothing at all and is an error rather than an empty result.
        if rcode != pkt::RCODE_OK {
            return Ok(NbnsResponse {
                trn_id: header.trn_id,
                answer: NbnsAnswer::Negative(NegativeAnswer {
                    name: String::new(),
                    suffix: 0,
                    rcode,
                    rcode_name: rcode_label(rcode),
                }),
            });
        }
        bail!("NBNS response has RCODE 0 and ANCOUNT 0: it asserts nothing");
    }

    let rr_name = pkt::read_name_field(buf, cursor)?;
    let mut p = rr_name.end;
    // TYPE(2) CLASS(2) TTL(4) RDLENGTH(2)
    if p + 10 > buf.len() {
        bail!("NBNS response truncated inside the answer record header");
    }
    let rr_type = u16::from_be_bytes([buf[p], buf[p + 1]]);
    let ttl = u32::from_be_bytes([buf[p + 4], buf[p + 5], buf[p + 6], buf[p + 7]]);
    let rdlength = u16::from_be_bytes([buf[p + 8], buf[p + 9]]) as usize;
    p += 10;
    let rdata = buf
        .get(p..p + rdlength)
        .context("NBNS response truncated inside the answer RDATA")?;

    if rcode != pkt::RCODE_OK {
        return Ok(NbnsResponse {
            trn_id: header.trn_id,
            answer: NbnsAnswer::Negative(NegativeAnswer {
                name: rr_name.name,
                suffix: rr_name.suffix,
                rcode,
                rcode_name: rcode_label(rcode),
            }),
        });
    }

    let answer = match rr_type {
        pkt::QTYPE_NB => {
            NbnsAnswer::Name(parse_nb_rdata(rr_name.name, rr_name.suffix, ttl, rdata)?)
        }
        pkt::QTYPE_NBSTAT => {
            NbnsAnswer::NodeStatus(parse_nbstat_rdata(rr_name.name, rr_name.suffix, rdata)?)
        }
        // A NULL RR with RCODE 0 claims neither an address nor a refusal.
        pkt::RRTYPE_NULL => bail!(
            "NBNS response carries a NULL answer record with RCODE 0, which asserts nothing \
             (RFC 1002 §4.2.14 requires a non-zero RCODE on a negative response)"
        ),
        other => bail!(
            "unsupported NBNS answer record type 0x{:04x} ({})",
            other,
            pkt::qtype_name(other)
        ),
    };

    Ok(NbnsResponse {
        trn_id: header.trn_id,
        answer,
    })
}

fn parse_nb_rdata(name: String, suffix: u8, ttl: u32, rdata: &[u8]) -> Result<NameAnswer> {
    // Zero *is* a multiple of six, so the emptiness check is separate and not redundant: an
    // NB answer with no addresses resolves the name to nothing and must be refused.
    if rdata.is_empty() || !rdata.len().is_multiple_of(6) {
        bail!(
            "NB answer RDATA is {} octets; RFC 1002 §4.2.13 requires a non-zero multiple of 6 \
             (2 flag octets + 4 address octets per entry)",
            rdata.len()
        );
    }
    let mut addresses = Vec::with_capacity(rdata.len() / 6);
    let mut first_flags = 0u16;
    // The length was just checked to be a non-zero multiple of 6, so the remainder is empty.
    for (i, chunk) in rdata.as_chunks::<6>().0.iter().enumerate() {
        let flags = u16::from_be_bytes([chunk[0], chunk[1]]);
        if i == 0 {
            first_flags = flags;
        }
        addresses.push(Ipv4Addr::new(chunk[2], chunk[3], chunk[4], chunk[5]));
    }

    // Each entry carries its own NB_FLAGS. In practice every entry of one answer describes the
    // same name and therefore repeats the same G bit and ONT; the first is reported and the
    // rest are not silently averaged into something no entry actually said.
    Ok(NameAnswer {
        name,
        suffix,
        addresses,
        ttl,
        group: first_flags & pkt::NB_FLAG_GROUP != 0,
        node_type: node_type_label(first_flags),
    })
}

fn parse_nbstat_rdata(name: String, suffix: u8, rdata: &[u8]) -> Result<NodeStatusAnswer> {
    let num_names = *rdata
        .first()
        .context("node status RDATA is empty; NUM_NAMES is missing")? as usize;

    let names_len = num_names
        .checked_mul(18)
        .context("node status NUM_NAMES overflows the name list length")?;
    let needed = 1 + names_len + pkt::STATISTICS_LEN;
    if rdata.len() < needed {
        bail!(
            "node status RDATA is {} octets but NUM_NAMES={} needs {} \
             (1 + 18 per name + {} of statistics)",
            rdata.len(),
            num_names,
            needed,
            pkt::STATISTICS_LEN
        );
    }

    let mut names = Vec::with_capacity(num_names);
    for i in 0..num_names {
        let at = 1 + i * 18;
        // RFC 1002 §4.2.18: NODE_NAME is the **raw** 16 octets, not the first-level encoded 32.
        // Decoding it as encoded here is the mirror image of the classic server-side bug.
        let mut raw = [0u8; pkt::NAME_LEN];
        raw.copy_from_slice(&rdata[at..at + pkt::NAME_LEN]);
        let flags = u16::from_be_bytes([rdata[at + 16], rdata[at + 17]]);
        // `split_netbios_name` trims both space and NUL padding, so a wildcard or a
        // NUL-padded name does not reach the model with its padding attached.
        let (entry_name, entry_suffix) = pkt::split_netbios_name(&raw);
        names.push(NodeStatusName {
            name: entry_name,
            suffix: entry_suffix,
            group: flags & pkt::NAME_FLAG_GROUP != 0,
            active: flags & pkt::NAME_FLAG_ACTIVE != 0,
        });
    }

    let stats_at = 1 + names_len;
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&rdata[stats_at..stats_at + 6]);

    Ok(NodeStatusAnswer {
        name,
        suffix,
        names,
        // A formatted string, never a byte array: models cannot reliably produce or read one.
        mac_address: pkt::format_mac(&mac),
    })
}

// ===========================================================================================
// Action parameter helpers
// ===========================================================================================

/// Read a `suffix` field from an action.
///
/// Accepts a number (`32`) or a hex string (`"0x20"`), which is the same contract the NBNS
/// **server**'s actions use, so a suffix observed in an event can be handed straight back.
/// A bare decimal string (`"32"`) is also accepted; `"20"` therefore means twenty, not 0x20 —
/// stated here because the ambiguity is real and only the sender knows which was meant.
pub fn parse_suffix(value: Option<&serde_json::Value>) -> Result<u8> {
    let Some(value) = value else { return Ok(0) };
    match value {
        serde_json::Value::Null => Ok(0),
        serde_json::Value::Number(n) => {
            let n = n
                .as_u64()
                .context("'suffix' must be a whole number between 0 and 255")?;
            u8::try_from(n).context("'suffix' must be between 0 and 255")
        }
        serde_json::Value::String(s) => {
            let s = s.trim();
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => u8::from_str_radix(hex, 16).ok(),
                None => s.parse::<u8>().ok(),
            };
            parsed.with_context(|| {
                format!(
                    "'suffix' string '{s}' is not a service suffix; use a number (32) or a hex \
                     string (\"0x20\")"
                )
            })
        }
        other => bail!("'suffix' must be a number or a hex string, got {other}"),
    }
}
