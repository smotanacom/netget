//! NetBIOS Name Service wire codec (RFC 1001 §14, RFC 1002 §4).
//!
//! Pure functions, no I/O and no LLM. Everything here is exercised directly from
//! `tests/server/netbios_ns/e2e_test.rs` against literal bytes captured from Samba's
//! `nmblookup`, so the encoding is pinned by something NetGet did not write.
//!
//! # The header is 12 octets, not 16
//!
//! RFC 1002 §4.2.1.1 gives six 16-bit fields — `NAME_TRN_ID`, `FLAGS`, `QDCOUNT`, `ANCOUNT`,
//! `NSCOUNT`, `ARCOUNT` — which is 12 octets, exactly DNS's header. It is easy to write 16
//! by counting the fields wrong; a captured `nmblookup` query has its question section
//! starting at offset 12 and settles it.
//!
//! # First-level encoding is the thing implementations get wrong
//!
//! A NetBIOS name is *always* 16 octets: 15 octets of name, space-padded (0x20), followed by
//! one **suffix** octet that selects the service (0x00 workstation, 0x20 file server, 0x1B
//! domain master browser…). Those 16 octets are then expanded to 32 by splitting each octet
//! into two 4-bit nibbles, high nibble first, and adding `'A'` (0x41) to each — so every
//! output character lands in `A..=P`. See [`encode_first_level`].

use anyhow::{bail, Context, Result};
use std::net::Ipv4Addr;

/// RFC 1002 §4.2.1.1 header: six 16-bit fields.
pub const HEADER_LEN: usize = 12;

/// The 16 octets of an unencoded NetBIOS name: 15 of name plus one suffix.
pub const NAME_LEN: usize = 16;

/// First-level encoding doubles [`NAME_LEN`].
pub const ENCODED_NAME_LEN: usize = 32;

/// RFC 1002 §4.1: an NBNS datagram is at most 576 octets.
pub const MAX_DATAGRAM: usize = 576;

// --- Question / RR types -------------------------------------------------------------------

/// `NB` — the NetBIOS general name service resource record.
pub const QTYPE_NB: u16 = 0x0020;
/// `NBSTAT` — node status.
pub const QTYPE_NBSTAT: u16 = 0x0021;
/// `NULL`, carried by a negative name query response (RFC 1002 §4.2.14).
pub const RRTYPE_NULL: u16 = 0x000A;
/// `IN` — the only class NBNS uses.
pub const CLASS_IN: u16 = 0x0001;

// --- OPCODE (FLAGS bits 11..14) ------------------------------------------------------------

pub const OPCODE_QUERY: u16 = 0;
pub const OPCODE_REGISTRATION: u16 = 5;
pub const OPCODE_RELEASE: u16 = 6;
pub const OPCODE_WACK: u16 = 7;
pub const OPCODE_REFRESH: u16 = 8;

// --- FLAGS ---------------------------------------------------------------------------------

/// `R` — set on a response.
pub const FLAG_RESPONSE: u16 = 0x8000;
/// `AA` — authoritative answer.
pub const NM_FLAG_AA: u16 = 0x0400;
/// `TC` — truncated.
pub const NM_FLAG_TC: u16 = 0x0200;
/// `RD` — recursion desired.
pub const NM_FLAG_RD: u16 = 0x0100;
/// `RA` — recursion available.
pub const NM_FLAG_RA: u16 = 0x0080;
/// `B` — broadcast.
pub const NM_FLAG_B: u16 = 0x0010;

const OPCODE_SHIFT: u16 = 11;
const OPCODE_MASK: u16 = 0x7800;
const RCODE_MASK: u16 = 0x000F;

// --- RCODE ---------------------------------------------------------------------------------

pub const RCODE_OK: u16 = 0;
/// Request was invalidly formatted.
pub const RCODE_FMT_ERR: u16 = 0x1;
/// Server failure.
pub const RCODE_SRV_ERR: u16 = 0x2;
/// Name not found (the ordinary "no such name" answer).
pub const RCODE_NAM_ERR: u16 = 0x3;
/// Unsupported request.
pub const RCODE_IMP_ERR: u16 = 0x4;
/// Refused.
pub const RCODE_RFS_ERR: u16 = 0x5;
/// Name is already active (registration only).
pub const RCODE_ACT_ERR: u16 = 0x6;
/// Name is in conflict (registration only).
pub const RCODE_CFT_ERR: u16 = 0x7;

/// Map a spoken rcode name to its numeric value. Returns `None` for anything unrecognised so
/// the caller can name the accepted set in its error.
pub fn rcode_by_name(name: &str) -> Option<u16> {
    match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "ok" | "no_error" | "success" => Some(RCODE_OK),
        "format_error" | "fmt_err" => Some(RCODE_FMT_ERR),
        "server_failure" | "srv_err" => Some(RCODE_SRV_ERR),
        "name_not_found" | "nam_err" | "name_error" => Some(RCODE_NAM_ERR),
        "unsupported_request" | "imp_err" => Some(RCODE_IMP_ERR),
        "refused" | "rfs_err" => Some(RCODE_RFS_ERR),
        "name_active" | "act_err" => Some(RCODE_ACT_ERR),
        "name_conflict" | "cft_err" => Some(RCODE_CFT_ERR),
        _ => None,
    }
}

/// Every rcode name this codec accepts, for error messages and documentation.
pub const RCODE_NAMES: &[&str] = &[
    "format_error",
    "server_failure",
    "name_not_found",
    "unsupported_request",
    "refused",
    "name_active",
    "name_conflict",
];

// --- NB_FLAGS (name query response RDATA, RFC 1002 §4.2.13) --------------------------------

/// `G` — the name is a group name rather than a unique name.
pub const NB_FLAG_GROUP: u16 = 0x8000;
const ONT_SHIFT: u16 = 13;

/// Owner node type, the `ONT` field of `NB_FLAGS` / `NAME_FLAGS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeType {
    /// Broadcast node.
    B,
    /// Point-to-point node.
    P,
    /// Mixed node.
    M,
    /// Hybrid node (Microsoft's extension; encoded as ONT 3).
    H,
}

impl NodeType {
    /// The two `ONT` bits, already shifted into their position in a flags word.
    pub fn ont_bits(self) -> u16 {
        let value = match self {
            NodeType::B => 0,
            NodeType::P => 1,
            NodeType::M => 2,
            NodeType::H => 3,
        };
        value << ONT_SHIFT
    }

    /// Parse `"b"`/`"p"`/`"m"`/`"h"` (case-insensitive, `"b-node"` accepted too).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().trim_end_matches("-node") {
            "b" | "broadcast" => Some(NodeType::B),
            "p" | "point-to-point" | "peer" => Some(NodeType::P),
            "m" | "mixed" => Some(NodeType::M),
            "h" | "hybrid" => Some(NodeType::H),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NodeType::B => "b",
            NodeType::P => "p",
            NodeType::M => "m",
            NodeType::H => "h",
        }
    }
}

// --- NAME_FLAGS (node status response, RFC 1002 §4.2.18) -----------------------------------

/// `G` — group name.
pub const NAME_FLAG_GROUP: u16 = 0x8000;
/// `ACT` — the name is active. A name list entry that is not active is meaningless, so this
/// is set for every name unless the caller explicitly asks otherwise.
pub const NAME_FLAG_ACTIVE: u16 = 0x0400;

/// The `STATISTICS` block of a node status response is a fixed 46 octets (RFC 1002 §4.2.18).
pub const STATISTICS_LEN: usize = 46;

// ===========================================================================================
// First-level encoding
// ===========================================================================================

/// Expand 16 octets of NetBIOS name into the 32 characters that go on the wire.
///
/// Each octet becomes two characters: `'A' + (octet >> 4)` then `'A' + (octet & 0x0F)`. Every
/// output character is therefore in `A..=P`.
///
/// The `'*'` wildcard name (`0x2A` followed by fifteen NULs) encodes to
/// `CKAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`, which is the literal a real `nmblookup -A` puts on the
/// wire and what the tests pin this against.
pub fn encode_first_level(name: &[u8; NAME_LEN]) -> [u8; ENCODED_NAME_LEN] {
    let mut out = [0u8; ENCODED_NAME_LEN];
    for (i, byte) in name.iter().enumerate() {
        out[i * 2] = b'A' + (byte >> 4);
        out[i * 2 + 1] = b'A' + (byte & 0x0F);
    }
    out
}

/// Inverse of [`encode_first_level`].
///
/// Rejects anything outside `A..=P`: a character outside that range cannot have come from a
/// nibble, so accepting it would silently invent a name.
pub fn decode_first_level(encoded: &[u8]) -> Result<[u8; NAME_LEN]> {
    if encoded.len() != ENCODED_NAME_LEN {
        bail!(
            "first-level encoded name must be {} octets, got {}",
            ENCODED_NAME_LEN,
            encoded.len()
        );
    }
    let mut out = [0u8; NAME_LEN];
    for i in 0..NAME_LEN {
        let hi = encoded[i * 2];
        let lo = encoded[i * 2 + 1];
        if !(b'A'..=b'P').contains(&hi) || !(b'A'..=b'P').contains(&lo) {
            bail!(
                "first-level encoded name contains a character outside A-P at offset {}: {:?}",
                i * 2,
                String::from_utf8_lossy(encoded)
            );
        }
        out[i] = ((hi - b'A') << 4) | (lo - b'A');
    }
    Ok(out)
}

/// The wildcard name — "whatever names you hold" — used by node status requests.
pub const WILDCARD_NAME: &str = "*";

/// Build the 16 unencoded octets from a human name plus its suffix.
///
/// Ordinary names are **space**-padded to 15 octets. The wildcard `*` is the exception: RFC
/// 1001 §17 defines it as `'*'` followed by fifteen **NUL** octets, and a real `nmblookup -A`
/// puts exactly that on the wire — encoding it with spaces produces
/// `CKCACACACACACACACACACACACACACAAA`, which no NBNS implementation recognises as the
/// wildcard. Padding it with spaces was a live bug here until the captured Samba datagram
/// disagreed with what this function produced.
///
/// A name longer than 15 characters is an error rather than a silent truncation — truncating
/// changes which host is being talked about.
pub fn pad_netbios_name(name: &str, suffix: u8) -> Result<[u8; NAME_LEN]> {
    let bytes = name.as_bytes();
    if bytes.len() > NAME_LEN - 1 {
        bail!(
            "NetBIOS name '{}' is {} characters; the wire format allows at most {} \
             (the 16th octet is the service suffix)",
            name,
            bytes.len(),
            NAME_LEN - 1
        );
    }
    if !bytes.is_ascii() {
        bail!("NetBIOS name '{}' must be ASCII", name);
    }
    let pad = if name == WILDCARD_NAME { 0x00 } else { b' ' };
    let mut out = [pad; NAME_LEN];
    out[..bytes.len()].copy_from_slice(bytes);
    out[NAME_LEN - 1] = suffix;
    Ok(out)
}

/// Split the 16 unencoded octets back into a trimmed name and its suffix.
///
/// Both pad octets are trimmed: spaces for an ordinary name, NULs for the wildcard. `trim_end`
/// alone does not remove NULs — `char::is_whitespace` is false for `\0` — so the wildcard
/// would otherwise reach the model as `"*\0\0…"`, a name it cannot match on or echo back.
pub fn split_netbios_name(raw: &[u8; NAME_LEN]) -> (String, u8) {
    let name = String::from_utf8_lossy(&raw[..NAME_LEN - 1])
        .trim_end_matches([' ', '\0'])
        .to_string();
    (name, raw[NAME_LEN - 1])
}

/// The full wire NAME field: length octet 0x20, the 32 encoded characters, then the scope
/// labels, then the root terminator.
pub fn encode_name_field(name: &str, suffix: u8, scope: Option<&str>) -> Result<Vec<u8>> {
    let raw = pad_netbios_name(name, suffix)?;
    let encoded = encode_first_level(&raw);

    let mut out = Vec::with_capacity(1 + ENCODED_NAME_LEN + 2);
    out.push(ENCODED_NAME_LEN as u8);
    out.extend_from_slice(&encoded);

    if let Some(scope) = scope {
        for label in scope.split('.').filter(|l| !l.is_empty()) {
            if label.len() > 63 {
                bail!("scope label '{}' exceeds 63 octets", label);
            }
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
    }
    out.push(0);
    Ok(out)
}

/// A NAME field read off the wire.
#[derive(Debug, Clone)]
pub struct NameField {
    /// The field exactly as it appeared, terminator included. Responses echo this verbatim so
    /// the scope survives a round trip without this codec having to re-encode it.
    pub raw: Vec<u8>,
    /// Trimmed 15-character portion.
    pub name: String,
    /// The service selector octet.
    pub suffix: u8,
    /// Dotted scope, if the sender supplied one.
    pub scope: Option<String>,
    /// Offset one past the field.
    pub end: usize,
}

/// Read a NAME field starting at `pos`.
///
/// Label compression (a leading `0xC0`) is refused rather than followed. NBNS requests never
/// use it — a question is the first thing in the datagram, so there is nothing to point at —
/// and following a pointer would leave us unable to echo the field verbatim.
pub fn read_name_field(buf: &[u8], pos: usize) -> Result<NameField> {
    let mut cursor = pos;
    let mut first: Option<[u8; NAME_LEN]> = None;
    let mut scope_labels: Vec<String> = Vec::new();

    loop {
        let len = *buf
            .get(cursor)
            .context("datagram ended inside a NetBIOS NAME field")?;
        if len & 0xC0 == 0xC0 {
            bail!("compressed (pointer) NetBIOS NAME field is not supported");
        }
        if len & 0xC0 != 0 {
            bail!("reserved label length bits set in a NetBIOS NAME field");
        }
        cursor += 1;
        if len == 0 {
            break;
        }
        let end = cursor + len as usize;
        let label = buf
            .get(cursor..end)
            .context("datagram ended inside a NetBIOS NAME label")?;
        if first.is_none() {
            if len as usize != ENCODED_NAME_LEN {
                bail!(
                    "first NetBIOS label is {} octets; first-level encoding always produces {}",
                    len,
                    ENCODED_NAME_LEN
                );
            }
            first = Some(decode_first_level(label)?);
        } else {
            scope_labels.push(String::from_utf8_lossy(label).into_owned());
        }
        cursor = end;
    }

    let raw16 = first.context("NetBIOS NAME field carried no name label")?;
    let (name, suffix) = split_netbios_name(&raw16);

    Ok(NameField {
        raw: buf[pos..cursor].to_vec(),
        name,
        suffix,
        scope: if scope_labels.is_empty() {
            None
        } else {
            Some(scope_labels.join("."))
        },
        end: cursor,
    })
}

// ===========================================================================================
// Header
// ===========================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub trn_id: u16,
    pub flags: u16,
    pub qdcount: u16,
    pub ancount: u16,
    pub nscount: u16,
    pub arcount: u16,
}

impl Header {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            bail!(
                "NBNS datagram is {} octets; the header alone is {}",
                buf.len(),
                HEADER_LEN
            );
        }
        let at = |i: usize| u16::from_be_bytes([buf[i], buf[i + 1]]);
        Ok(Self {
            trn_id: at(0),
            flags: at(2),
            qdcount: at(4),
            ancount: at(6),
            nscount: at(8),
            arcount: at(10),
        })
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..2].copy_from_slice(&self.trn_id.to_be_bytes());
        out[2..4].copy_from_slice(&self.flags.to_be_bytes());
        out[4..6].copy_from_slice(&self.qdcount.to_be_bytes());
        out[6..8].copy_from_slice(&self.ancount.to_be_bytes());
        out[8..10].copy_from_slice(&self.nscount.to_be_bytes());
        out[10..12].copy_from_slice(&self.arcount.to_be_bytes());
        out
    }

    pub fn opcode(&self) -> u16 {
        (self.flags & OPCODE_MASK) >> OPCODE_SHIFT
    }

    pub fn is_response(&self) -> bool {
        self.flags & FLAG_RESPONSE != 0
    }

    pub fn rcode(&self) -> u16 {
        self.flags & RCODE_MASK
    }

    pub fn recursion_desired(&self) -> bool {
        self.flags & NM_FLAG_RD != 0
    }

    pub fn broadcast(&self) -> bool {
        self.flags & NM_FLAG_B != 0
    }
}

/// Human name for an opcode, for logs.
pub fn opcode_name(opcode: u16) -> &'static str {
    match opcode {
        OPCODE_QUERY => "QUERY",
        OPCODE_REGISTRATION => "REGISTRATION",
        OPCODE_RELEASE => "RELEASE",
        OPCODE_WACK => "WACK",
        OPCODE_REFRESH => "REFRESH",
        _ => "UNKNOWN",
    }
}

/// Human name for a question type, for logs and for the event payload.
pub fn qtype_name(qtype: u16) -> &'static str {
    match qtype {
        QTYPE_NB => "NB",
        QTYPE_NBSTAT => "NBSTAT",
        RRTYPE_NULL => "NULL",
        _ => "UNKNOWN",
    }
}

// ===========================================================================================
// Request
// ===========================================================================================

/// One address a NAME REGISTRATION REQUEST is claiming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressEntry {
    pub flags: u16,
    pub address: Ipv4Addr,
}

impl AddressEntry {
    pub fn is_group(&self) -> bool {
        self.flags & NB_FLAG_GROUP != 0
    }
}

/// A decoded inbound request.
#[derive(Debug, Clone)]
pub struct NbnsRequest {
    pub header: Header,
    pub question_name: NameField,
    pub qtype: u16,
    pub qclass: u16,
    /// Addresses carried in the ADDITIONAL section — a registration's claimed address.
    pub addresses: Vec<AddressEntry>,
}

/// Parse a request datagram.
///
/// Refuses anything that is already a response: an NBNS server answering a response would
/// amplify a spoofed packet back at whoever it was aimed at.
pub fn parse_request(buf: &[u8]) -> Result<NbnsRequest> {
    if buf.len() > MAX_DATAGRAM {
        bail!(
            "NBNS datagram is {} octets; RFC 1002 §4.1 caps it at {}",
            buf.len(),
            MAX_DATAGRAM
        );
    }
    let header = Header::parse(buf)?;
    if header.is_response() {
        bail!("datagram is a response (R=1), not a request");
    }
    if header.qdcount != 1 {
        bail!(
            "NBNS request must carry exactly one question, QDCOUNT={}",
            header.qdcount
        );
    }

    let question_name = read_name_field(buf, HEADER_LEN)?;
    let after_name = question_name.end;
    let qtype = u16::from_be_bytes([
        *buf.get(after_name)
            .context("question truncated before TYPE")?,
        *buf.get(after_name + 1)
            .context("question truncated inside TYPE")?,
    ]);
    let qclass = u16::from_be_bytes([
        *buf.get(after_name + 2)
            .context("question truncated before CLASS")?,
        *buf.get(after_name + 3)
            .context("question truncated inside CLASS")?,
    ]);

    // The ADDITIONAL section of a registration carries the address being claimed. Anything
    // that does not parse is skipped rather than failing the whole datagram: the question is
    // what the model is being asked about, and a mangled additional record must not silence
    // the server.
    let mut addresses = Vec::new();
    if header.arcount > 0 {
        let mut cursor = after_name + 4;
        for _ in 0..header.arcount {
            let Ok(rr_name) = read_name_field(buf, cursor) else {
                break;
            };
            let mut p = rr_name.end;
            // TYPE(2) CLASS(2) TTL(4) RDLENGTH(2)
            if p + 10 > buf.len() {
                break;
            }
            let rr_type = u16::from_be_bytes([buf[p], buf[p + 1]]);
            let rdlength = u16::from_be_bytes([buf[p + 8], buf[p + 9]]) as usize;
            p += 10;
            if p + rdlength > buf.len() {
                break;
            }
            if rr_type == QTYPE_NB {
                let mut q = p;
                while q + 6 <= p + rdlength {
                    let flags = u16::from_be_bytes([buf[q], buf[q + 1]]);
                    let address = Ipv4Addr::new(buf[q + 2], buf[q + 3], buf[q + 4], buf[q + 5]);
                    addresses.push(AddressEntry { flags, address });
                    q += 6;
                }
            }
            cursor = p + rdlength;
        }
    }

    Ok(NbnsRequest {
        header,
        question_name,
        qtype,
        qclass,
        addresses,
    })
}

// ===========================================================================================
// Responses
// ===========================================================================================

/// Flags for a response: R, the request's opcode, AA, RA, plus RD echoed back, and the rcode.
fn response_flags(opcode: u16, recursion_desired: bool, rcode: u16) -> u16 {
    let mut flags = FLAG_RESPONSE | ((opcode << OPCODE_SHIFT) & OPCODE_MASK) | NM_FLAG_AA;
    if recursion_desired {
        // RFC 1002 §4.2.13: RD is echoed and RA set when the server is willing to recurse.
        flags |= NM_FLAG_RD | NM_FLAG_RA;
    }
    flags | (rcode & RCODE_MASK)
}

/// POSITIVE NAME QUERY RESPONSE (RFC 1002 §4.2.13), and — with `opcode` set to
/// [`OPCODE_REGISTRATION`] — POSITIVE NAME REGISTRATION RESPONSE (§4.2.6). The two differ
/// only in the OPCODE, which is why the server supplies it from the request rather than the
/// model supplying it as a parameter.
pub fn encode_name_query_response(
    trn_id: u16,
    opcode: u16,
    recursion_desired: bool,
    name_field: &[u8],
    addresses: &[AddressEntry],
    ttl: u32,
) -> Result<Vec<u8>> {
    if addresses.is_empty() {
        bail!("a positive NetBIOS name response must carry at least one address");
    }
    let header = Header {
        trn_id,
        flags: response_flags(opcode, recursion_desired, RCODE_OK),
        qdcount: 0,
        ancount: 1,
        nscount: 0,
        arcount: 0,
    };

    let rdlength = addresses.len() * 6;
    let rdlength = u16::try_from(rdlength).context("too many addresses for one response")?;

    let mut out = Vec::with_capacity(HEADER_LEN + name_field.len() + 10 + rdlength as usize);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(name_field);
    out.extend_from_slice(&QTYPE_NB.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&rdlength.to_be_bytes());
    for entry in addresses {
        out.extend_from_slice(&entry.flags.to_be_bytes());
        out.extend_from_slice(&entry.address.octets());
    }
    Ok(out)
}

/// NEGATIVE NAME QUERY RESPONSE (RFC 1002 §4.2.14): one answer RR of type NULL, zero RDATA,
/// and a non-zero RCODE. Also serves as a negative registration response when `opcode` is
/// [`OPCODE_REGISTRATION`].
pub fn encode_negative_response(
    trn_id: u16,
    opcode: u16,
    recursion_desired: bool,
    name_field: &[u8],
    rcode: u16,
) -> Result<Vec<u8>> {
    if rcode == RCODE_OK {
        bail!("a negative NetBIOS response must carry a non-zero RCODE");
    }
    let header = Header {
        trn_id,
        flags: response_flags(opcode, recursion_desired, rcode),
        qdcount: 0,
        ancount: 1,
        nscount: 0,
        arcount: 0,
    };

    let mut out = Vec::with_capacity(HEADER_LEN + name_field.len() + 10);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(name_field);
    out.extend_from_slice(&RRTYPE_NULL.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // TTL
    out.extend_from_slice(&0u16.to_be_bytes()); // RDLENGTH
    Ok(out)
}

/// One entry of a node status name list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeName {
    /// The 16 **unencoded** octets. RFC 1002 §4.2.18's NODE_NAME is raw ASCII, not
    /// first-level encoded — encoding it here is a classic way to produce a name list that
    /// every real client renders as gibberish.
    pub raw: [u8; NAME_LEN],
    pub flags: u16,
}

/// NODE STATUS RESPONSE (RFC 1002 §4.2.18).
///
/// RDATA is `NUM_NAMES` (1 octet), then 18 octets per name (16 raw + 2 flag octets), then the
/// fixed 46-octet statistics block whose first six octets are the adapter's `UNIT_ID` (its
/// MAC). Every other statistic is reported as zero: NetGet has no adapter counters to report
/// and inventing them would be fabricating data about a machine.
pub fn encode_node_status_response(
    trn_id: u16,
    name_field: &[u8],
    names: &[NodeName],
    mac: [u8; 6],
) -> Result<Vec<u8>> {
    if names.is_empty() {
        bail!("a node status response must list at least one name");
    }
    let num_names = u8::try_from(names.len())
        .context("a node status response can list at most 255 names (NUM_NAMES is one octet)")?;

    let rdlength = 1 + names.len() * 18 + STATISTICS_LEN;
    let rdlength = u16::try_from(rdlength).context("node status RDATA exceeds 65535 octets")?;

    // A node status response is not an answer to a name query, so RD/RA do not apply; RFC
    // 1002 §4.2.18 shows AA set and nothing else.
    let header = Header {
        trn_id,
        flags: FLAG_RESPONSE | NM_FLAG_AA,
        qdcount: 0,
        ancount: 1,
        nscount: 0,
        arcount: 0,
    };

    let mut out = Vec::with_capacity(HEADER_LEN + name_field.len() + 10 + rdlength as usize);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(name_field);
    out.extend_from_slice(&QTYPE_NBSTAT.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // TTL is 0 for node status
    out.extend_from_slice(&rdlength.to_be_bytes());
    out.push(num_names);
    for entry in names {
        out.extend_from_slice(&entry.raw);
        out.extend_from_slice(&entry.flags.to_be_bytes());
    }
    let mut statistics = [0u8; STATISTICS_LEN];
    statistics[..6].copy_from_slice(&mac);
    out.extend_from_slice(&statistics);

    if out.len() > MAX_DATAGRAM {
        bail!(
            "node status response is {} octets, over the {} octet limit; list fewer names",
            out.len(),
            MAX_DATAGRAM
        );
    }
    Ok(out)
}

/// Read a `suffix` action parameter.
///
/// **One copy, shared by the server's actions and the client's, because the two disagreeing
/// silently is a wrong-name bug rather than a wrong-value one.** `FILESERVER<0x20>` and
/// `FILESERVER<0x14>` are different names, so a suffix that decodes to the wrong octet does
/// not fail — it resolves, or answers for, a name nobody asked about.
///
/// Accepted: a number `0..=255`, or a string with an explicit `0x` prefix. A **bare** digit
/// string such as `"20"` is refused, deliberately: NetBIOS suffixes are written in hex by
/// convention (`<20>` is the file server) while JSON strings of digits read as decimal, and
/// `"20"` is simultaneously a valid spelling of 32 and of 20 with only the sender knowing
/// which. That is the same ambiguity the root `CLAUDE.md` records for `send_tcp_data`'s
/// text-or-hex field, and the same answer: make the sender say which, rather than sniff.
pub fn parse_suffix_value(value: Option<&serde_json::Value>) -> Result<u8> {
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
            let trimmed = s.trim();
            match trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("0X"))
            {
                Some(hex) => u8::from_str_radix(hex, 16).with_context(|| {
                    format!("'suffix' string '{s}' is not a hex octet (0x00-0xff)")
                }),
                None => bail!(
                    "'suffix' string '{s}' is ambiguous: NetBIOS suffixes are conventionally \
                     written in hex, but a bare string of digits reads as decimal, so '{s}' \
                     could mean two different names. Give a number (32) or an explicit hex \
                     string (\"0x20\")."
                ),
            }
        }
        other => bail!("'suffix' must be a number (32) or a hex string (\"0x20\"), got {other}"),
    }
}

/// Parse `"00:11:22:33:44:55"` (or `-` separated) into six octets.
///
/// A MAC reaches the model as a formatted string, never as a byte array — models cannot
/// reliably produce byte arrays, and this is the one place a hardware address is needed.
pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = text.split(['-', ':']).collect();
    if parts.len() != 6 {
        bail!(
            "MAC address '{}' must have six octets separated by ':' or '-', e.g. \
             \"00:11:22:33:44:55\"",
            text
        );
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(part, 16)
            .with_context(|| format!("'{}' in MAC address '{}' is not a hex octet", part, text))?;
    }
    Ok(out)
}

/// Render six octets back as `"00:11:22:33:44:55"`.
pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(":")
}
