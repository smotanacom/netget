//! HSRP wire formats, both of them.
//!
//! **HSRPv1 and HSRPv2 are not the same protocol in a different dress — they are two
//! unrelated encodings that happen to share a name, a port and an election idea.** v1 is a
//! flat 20-byte struct (RFC 2281 §5); v2 is a sequence of TLVs. Nothing about one parses as
//! the other, and the state *numbers* differ in a way that silently mis-decodes rather than
//! failing: code `4` is **Speak** in v1 and **Standby** in v2. That collision is the single
//! most dangerous thing in this file, which is why the enums below never carry a bare integer
//! across a version boundary — `HsrpState` is converted through `from_v1_code`/`from_v2_code`
//! and `v1_code`/`v2_code`, and there is no `as u8` shortcut anywhere.
//!
//! This module is pure: no sockets, no LLM, no state. `mod.rs` owns all of that.

use anyhow::{anyhow, Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Exact size of an HSRPv1 datagram (RFC 2281 §5): eight one-byte fields, eight bytes of
/// authentication data, four bytes of virtual IP.
pub const V1_LEN: usize = 20;

/// The value HSRPv1 puts in its `Version` byte. **It is 0, not 1** (RFC 2281 §5: "Currently,
/// this is version 0"). Reading this as "v1 means byte 1" produces a packet no Cisco device
/// accepts, and is the first thing to check if interop ever fails.
pub const V1_VERSION_BYTE: u8 = 0;

/// HSRPv2 Group State TLV type.
pub const V2_TLV_GROUP_STATE: u8 = 1;
/// HSRPv2 Interface State TLV type. Parsed past, never generated.
pub const V2_TLV_INTERFACE_STATE: u8 = 2;
/// HSRPv2 Text Authentication TLV type — the v2 home of the same 8-byte plaintext field v1
/// carries inline.
pub const V2_TLV_TEXT_AUTH: u8 = 3;
/// HSRPv2 MD5 Authentication TLV type. Parsed structurally; see `Md5Auth`.
pub const V2_TLV_MD5_AUTH: u8 = 4;

/// Payload length of the Group State TLV, i.e. the byte after the type. Always 40.
pub const V2_GROUP_STATE_LEN: u8 = 40;
/// Total size of a Group State TLV including its two-byte type/length header.
pub const V2_GROUP_STATE_TOTAL: usize = 2 + V2_GROUP_STATE_LEN as usize;
/// Payload length of the Text Authentication TLV. Always 8, like v1's inline field.
pub const V2_TEXT_AUTH_LEN: u8 = 8;
/// Payload length of the MD5 Authentication TLV in the form this parser accepts.
pub const V2_MD5_AUTH_LEN: u8 = 28;

/// Width of the authentication field in both versions.
pub const AUTH_FIELD_LEN: usize = 8;

/// The notorious Cisco default. See `CLAUDE.md`: this is **plaintext on the wire and provides
/// no security whatsoever** — it is a misconfiguration guard, not authentication.
pub const DEFAULT_AUTH_DATA: &str = "cisco";

/// Which of the two incompatible encodings a message uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsrpVersion {
    V1,
    V2,
}

impl HsrpVersion {
    pub fn as_number(self) -> u8 {
        match self {
            HsrpVersion::V1 => 1,
            HsrpVersion::V2 => 2,
        }
    }

    /// Accepts the *protocol* version the operator means (1 or 2), not the byte on the wire.
    pub fn from_number(value: u64) -> Result<Self> {
        match value {
            1 => Ok(HsrpVersion::V1),
            2 => Ok(HsrpVersion::V2),
            other => Err(anyhow!(
                "HSRP version must be 1 or 2, got {other}. The two are different wire formats \
                 and are not interoperable."
            )),
        }
    }
}

/// What the message asserts. All three are elections moves, not queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    /// "I am here, at this priority, in this state."
    Hello,
    /// "I am taking Active from you, now."
    Coup,
    /// "I am giving up Active."
    Resign,
}

impl Opcode {
    pub fn code(self) -> u8 {
        match self {
            Opcode::Hello => 0,
            Opcode::Coup => 1,
            Opcode::Resign => 2,
        }
    }

    pub fn from_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Opcode::Hello),
            1 => Ok(Opcode::Coup),
            2 => Ok(Opcode::Resign),
            other => Err(anyhow!("Unknown HSRP opcode {other} (expected 0, 1 or 2)")),
        }
    }

    /// Stable lowercase token used in event data, action names and log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Opcode::Hello => "hello",
            Opcode::Coup => "coup",
            Opcode::Resign => "resign",
        }
    }
}

/// Where the speaker says it is in the election.
///
/// **The numeric encodings differ between versions and overlap.** They are never exposed as
/// numbers outside this module; the model sees and produces the names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsrpState {
    Initial,
    Learn,
    Listen,
    Speak,
    Standby,
    Active,
}

impl HsrpState {
    /// RFC 2281 §5: a bit-per-state encoding, so Speak is 4 and Active is 16.
    pub fn v1_code(self) -> u8 {
        match self {
            HsrpState::Initial => 0,
            HsrpState::Learn => 1,
            HsrpState::Listen => 2,
            HsrpState::Speak => 4,
            HsrpState::Standby => 8,
            HsrpState::Active => 16,
        }
    }

    /// HSRPv2 renumbered these densely, so 4 is Standby here and Speak in v1.
    pub fn v2_code(self) -> u8 {
        match self {
            HsrpState::Initial => 0,
            HsrpState::Learn => 1,
            HsrpState::Listen => 2,
            HsrpState::Speak => 3,
            HsrpState::Standby => 4,
            HsrpState::Active => 5,
        }
    }

    pub fn from_v1_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(HsrpState::Initial),
            1 => Ok(HsrpState::Learn),
            2 => Ok(HsrpState::Listen),
            4 => Ok(HsrpState::Speak),
            8 => Ok(HsrpState::Standby),
            16 => Ok(HsrpState::Active),
            other => Err(anyhow!(
                "Unknown HSRPv1 state code {other} (expected 0, 1, 2, 4, 8 or 16)"
            )),
        }
    }

    pub fn from_v2_code(code: u8) -> Result<Self> {
        match code {
            0 => Ok(HsrpState::Initial),
            1 => Ok(HsrpState::Learn),
            2 => Ok(HsrpState::Listen),
            3 => Ok(HsrpState::Speak),
            4 => Ok(HsrpState::Standby),
            5 => Ok(HsrpState::Active),
            other => Err(anyhow!("Unknown HSRPv2 state code {other} (expected 0-5)")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HsrpState::Initial => "initial",
            HsrpState::Learn => "learn",
            HsrpState::Listen => "listen",
            HsrpState::Speak => "speak",
            HsrpState::Standby => "standby",
            HsrpState::Active => "active",
        }
    }

    /// Parse the name the model uses. Deliberately name-only: accepting a number here would
    /// reintroduce the v1/v2 code collision at the one place it is most likely to be wrong.
    pub fn from_str_name(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "initial" => Ok(HsrpState::Initial),
            "learn" => Ok(HsrpState::Learn),
            "listen" => Ok(HsrpState::Listen),
            "speak" => Ok(HsrpState::Speak),
            "standby" => Ok(HsrpState::Standby),
            "active" => Ok(HsrpState::Active),
            other => Err(anyhow!(
                "Unknown HSRP state '{other}'. Use one of: initial, learn, listen, speak, \
                 standby, active. (State names are used rather than numbers because HSRPv1 and \
                 HSRPv2 encode them differently - code 4 is Speak in v1 and Standby in v2.)"
            )),
        }
    }

    /// True when this state claims the virtual IP, i.e. claims to be the segment's gateway.
    pub fn claims_gateway(self) -> bool {
        matches!(self, HsrpState::Active)
    }
}

/// What an HSRPv2 MD5 Authentication TLV said, minus the digest.
///
/// **The digest is deliberately not carried here and never leaves the parser.** It is 16 raw
/// bytes, which the root `CLAUDE.md` forbids putting in event data, and it is unverifiable
/// anyway: verifying it needs the shared key, and NetGet stores nothing. What the model gets
/// is the structural facts — that MD5 auth was used, by which key id, claiming which sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Md5Auth {
    pub algorithm: u8,
    pub flags: u16,
    pub sender_address: Ipv4Addr,
    pub key_id: u32,
}

/// One HSRP advertisement, version-independent.
///
/// Times are held in **seconds** in both versions. v1 carries seconds on the wire; v2 carries
/// milliseconds and this struct converts at the edge, so nothing above the codec has to
/// remember which unit a given version uses. The cost is that v2's sub-second timers are not
/// expressible; `CLAUDE.md` records that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HsrpMessage {
    pub version: HsrpVersion,
    pub opcode: Opcode,
    pub state: HsrpState,
    pub hellotime_secs: u32,
    pub holdtime_secs: u32,
    pub priority: u32,
    pub group: u16,
    /// Plaintext authentication string, trailing NULs stripped. `None` means the v2 message
    /// carried no Text Authentication TLV. A v1 message always has the field, so `None` there
    /// means it was all zeros.
    pub auth_data: Option<String>,
    pub virtual_ip: IpAddr,
    /// v2 only: the sender's 6-byte identifier, in practice its MAC. Zero for v1.
    pub identifier: [u8; 6],
    /// v2 only, parse direction only.
    pub md5_auth: Option<Md5Auth>,
}

impl HsrpMessage {
    /// Render the identifier the way a human reads a MAC.
    pub fn identifier_str(&self) -> String {
        self.identifier
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// Pad a plaintext authentication string into the fixed 8-byte field.
///
/// Longer than 8 bytes is an error rather than a truncation: a silently truncated
/// authentication string produces a packet the peer rejects for a reason nothing logs, which
/// is indistinguishable from this server having said nothing.
fn encode_auth_field(auth: Option<&str>) -> Result<[u8; AUTH_FIELD_LEN]> {
    let mut field = [0u8; AUTH_FIELD_LEN];
    let Some(auth) = auth else {
        return Ok(field);
    };
    let bytes = auth.as_bytes();
    if bytes.len() > AUTH_FIELD_LEN {
        return Err(anyhow!(
            "HSRP auth_data must be at most {AUTH_FIELD_LEN} bytes, got {} ('{auth}'). The \
             field is a fixed 8-byte NUL-padded string.",
            bytes.len()
        ));
    }
    field[..bytes.len()].copy_from_slice(bytes);
    Ok(field)
}

/// Read the fixed 8-byte authentication field back into a string.
///
/// Lossy on purpose: the field is nominally ASCII but nothing on the wire enforces it, and a
/// hostile sender putting arbitrary bytes there must not be able to fail the parse (which
/// would drop the whole advertisement) or panic a UTF-8 slice.
fn decode_auth_field(field: &[u8]) -> Option<String> {
    let trimmed: Vec<u8> = field.iter().copied().take_while(|b| *b != 0).collect();
    if trimmed.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&trimmed).into_owned())
}

fn u16_at(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

fn u32_at(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Serialize a message in whichever format its `version` names.
pub fn encode(message: &HsrpMessage) -> Result<Vec<u8>> {
    match message.version {
        HsrpVersion::V1 => encode_v1(message),
        HsrpVersion::V2 => encode_v2(message),
    }
}

/// HSRPv1, RFC 2281 §5. Exactly 20 bytes, every field one byte except auth (8) and the
/// virtual IP (4).
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |   Version     |   Op Code     |     State     |   Hellotime   |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |   Holdtime    |   Priority    |     Group     |   Reserved    |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                   Authentication  Data                        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                   Authentication  Data                        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                    Virtual IP Address                         |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
fn encode_v1(message: &HsrpMessage) -> Result<Vec<u8>> {
    let IpAddr::V4(vip) = message.virtual_ip else {
        return Err(anyhow!(
            "HSRPv1 carries a 4-byte virtual IP and has no IPv6 form; '{}' is IPv6. Use \
             version 2 for IPv6.",
            message.virtual_ip
        ));
    };

    let priority = u8::try_from(message.priority).map_err(|_| {
        anyhow!(
            "HSRPv1 priority is a single byte (0-255), got {}. HSRPv2 widened it to 32 bits.",
            message.priority
        )
    })?;
    let group = u8::try_from(message.group).map_err(|_| {
        anyhow!(
            "HSRPv1 group is a single byte (0-255), got {}. HSRPv2 widened it to 0-4095.",
            message.group
        )
    })?;
    let hellotime = u8::try_from(message.hellotime_secs).map_err(|_| {
        anyhow!(
            "HSRPv1 hellotime is a single byte of seconds (1-255), got {}.",
            message.hellotime_secs
        )
    })?;
    let holdtime = u8::try_from(message.holdtime_secs).map_err(|_| {
        anyhow!(
            "HSRPv1 holdtime is a single byte of seconds (1-255), got {}.",
            message.holdtime_secs
        )
    })?;

    let auth = encode_auth_field(message.auth_data.as_deref())?;

    let mut out = Vec::with_capacity(V1_LEN);
    out.push(V1_VERSION_BYTE);
    out.push(message.opcode.code());
    out.push(message.state.v1_code());
    out.push(hellotime);
    out.push(holdtime);
    out.push(priority);
    out.push(group);
    out.push(0); // Reserved
    out.extend_from_slice(&auth);
    out.extend_from_slice(&vip.octets());

    debug_assert_eq!(out.len(), V1_LEN);
    Ok(out)
}

/// HSRPv2 — a Group State TLV, optionally followed by a Text Authentication TLV.
///
/// ```text
/// +--------+--------+---------------------------------------------+
/// | Type=1 | Len=40 |  Version | Opcode | State | IP Ver |  Group  |
/// +--------+--------+---------------------------------------------+
/// |         Identifier (6 bytes)        |     Priority (4)        |
/// +---------------------------------------------------------------+
/// |    Hellotime (4, ms)   |    Holdtime (4, ms)                   |
/// +---------------------------------------------------------------+
/// |            Virtual IP Address (16 bytes)                       |
/// +---------------------------------------------------------------+
/// ```
///
/// HSRPv2 has no RFC; this layout is taken from Cisco's published documentation and from
/// packet-capture dissectors, and `CLAUDE.md` records that it has never been checked against a
/// real Cisco device.
fn encode_v2(message: &HsrpMessage) -> Result<Vec<u8>> {
    if message.group > 4095 {
        return Err(anyhow!(
            "HSRPv2 group must be 0-4095, got {}.",
            message.group
        ));
    }

    // v2 times are milliseconds on the wire. A value large enough to overflow is a modelling
    // error, not something to wrap silently into a fast timer.
    let hellotime_ms = message.hellotime_secs.checked_mul(1000).ok_or_else(|| {
        anyhow!(
            "HSRPv2 hellotime {}s does not fit the 32-bit millisecond field.",
            message.hellotime_secs
        )
    })?;
    let holdtime_ms = message.holdtime_secs.checked_mul(1000).ok_or_else(|| {
        anyhow!(
            "HSRPv2 holdtime {}s does not fit the 32-bit millisecond field.",
            message.holdtime_secs
        )
    })?;

    let (ip_version, vip_bytes): (u8, [u8; 16]) = match message.virtual_ip {
        IpAddr::V4(v4) => {
            // The field is 16 bytes whatever the family; an IPv4 address occupies the first
            // four and the rest stay zero.
            let mut buf = [0u8; 16];
            buf[..4].copy_from_slice(&v4.octets());
            (4, buf)
        }
        IpAddr::V6(v6) => (6, v6.octets()),
    };

    let mut out = Vec::with_capacity(V2_GROUP_STATE_TOTAL + 2 + AUTH_FIELD_LEN);
    out.push(V2_TLV_GROUP_STATE);
    out.push(V2_GROUP_STATE_LEN);
    out.push(HsrpVersion::V2.as_number());
    out.push(message.opcode.code());
    out.push(message.state.v2_code());
    out.push(ip_version);
    out.extend_from_slice(&message.group.to_be_bytes());
    out.extend_from_slice(&message.identifier);
    out.extend_from_slice(&message.priority.to_be_bytes());
    out.extend_from_slice(&hellotime_ms.to_be_bytes());
    out.extend_from_slice(&holdtime_ms.to_be_bytes());
    out.extend_from_slice(&vip_bytes);

    debug_assert_eq!(out.len(), V2_GROUP_STATE_TOTAL);

    // In v2 the plaintext string is its own TLV rather than an inline field, so "no auth" is
    // expressible here in a way it is not in v1.
    if let Some(auth) = message.auth_data.as_deref() {
        let field = encode_auth_field(Some(auth))?;
        out.push(V2_TLV_TEXT_AUTH);
        out.push(V2_TEXT_AUTH_LEN);
        out.extend_from_slice(&field);
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Parse a datagram as whichever of the two formats it is.
///
/// The two are told apart by their first bytes and cannot be confused: a v1 packet starts with
/// its Version byte, which is **0**, while a v2 packet starts with a TLV type, which is never
/// 0. Sniffing rather than trusting a configured version is deliberate — a real segment can
/// carry both at once, and refusing to parse the one the operator did not configure would
/// simply hide a neighbour from the model.
pub fn decode(buf: &[u8]) -> Result<HsrpMessage> {
    match buf.first() {
        Some(&V1_VERSION_BYTE) => decode_v1(buf),
        Some(_) => decode_v2(buf),
        None => Err(anyhow!("Empty HSRP datagram")),
    }
}

fn decode_v1(buf: &[u8]) -> Result<HsrpMessage> {
    if buf.len() < V1_LEN {
        return Err(anyhow!(
            "HSRPv1 datagram is {} bytes, needs {V1_LEN}",
            buf.len()
        ));
    }

    let opcode = Opcode::from_code(buf[1])?;
    let state = HsrpState::from_v1_code(buf[2])?;
    let virtual_ip = Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]);

    Ok(HsrpMessage {
        version: HsrpVersion::V1,
        opcode,
        state,
        hellotime_secs: u32::from(buf[3]),
        holdtime_secs: u32::from(buf[4]),
        priority: u32::from(buf[5]),
        group: u16::from(buf[6]),
        // buf[7] is Reserved and is ignored rather than validated: RFC 2281 does not require
        // receivers to reject a non-zero value, and dropping the advertisement would hide a
        // neighbour over a field nothing acts on.
        auth_data: decode_auth_field(&buf[8..16]),
        virtual_ip: IpAddr::V4(virtual_ip),
        identifier: [0u8; 6],
        md5_auth: None,
    })
}

/// Walk the TLV chain. The Group State TLV is mandatory; Text and MD5 authentication are
/// optional and may appear in either order; anything else is skipped by its own length.
fn decode_v2(buf: &[u8]) -> Result<HsrpMessage> {
    let mut group_state: Option<HsrpMessage> = None;
    let mut text_auth: Option<String> = None;
    let mut md5_auth: Option<Md5Auth> = None;

    let mut offset = 0usize;
    while offset + 2 <= buf.len() {
        let tlv_type = buf[offset];
        let tlv_len = usize::from(buf[offset + 1]);
        let body_start = offset + 2;
        let body_end = body_start
            .checked_add(tlv_len)
            .filter(|end| *end <= buf.len())
            .ok_or_else(|| {
                anyhow!(
                    "HSRPv2 TLV type {tlv_type} at offset {offset} claims {tlv_len} bytes, past \
                     the end of a {}-byte datagram",
                    buf.len()
                )
            })?;
        let body = &buf[body_start..body_end];

        match tlv_type {
            V2_TLV_GROUP_STATE => {
                group_state = Some(decode_v2_group_state(body)?);
            }
            V2_TLV_TEXT_AUTH => {
                text_auth = decode_auth_field(body);
            }
            V2_TLV_MD5_AUTH => {
                md5_auth = decode_v2_md5_auth(body);
            }
            V2_TLV_INTERFACE_STATE => {
                // Carries the sender's interface MTU/state. Nothing here acts on it.
            }
            _ => {
                // Unknown TLVs are skipped by their length rather than treated as a parse
                // failure: HSRPv2's whole point is that a speaker can add TLVs a peer does not
                // know, and dropping the advertisement over one would hide the neighbour.
            }
        }

        offset = body_end;
    }

    let mut message = group_state.context(
        "HSRPv2 datagram carried no Group State TLV (type 1); there is nothing to report",
    )?;
    message.auth_data = text_auth;
    message.md5_auth = md5_auth;
    Ok(message)
}

fn decode_v2_group_state(body: &[u8]) -> Result<HsrpMessage> {
    if body.len() < usize::from(V2_GROUP_STATE_LEN) {
        return Err(anyhow!(
            "HSRPv2 Group State TLV is {} bytes, needs {V2_GROUP_STATE_LEN}",
            body.len()
        ));
    }

    let wire_version = body[0];
    if wire_version != HsrpVersion::V2.as_number() {
        return Err(anyhow!(
            "HSRPv2 Group State TLV declares version {wire_version}, expected 2"
        ));
    }

    let opcode = Opcode::from_code(body[1])?;
    let state = HsrpState::from_v2_code(body[2])?;
    let ip_version = body[3];
    let group = u16_at(body, 4);

    let mut identifier = [0u8; 6];
    identifier.copy_from_slice(&body[6..12]);

    let priority = u32_at(body, 12);
    let hellotime_ms = u32_at(body, 16);
    let holdtime_ms = u32_at(body, 20);

    let virtual_ip = match ip_version {
        4 => IpAddr::V4(Ipv4Addr::new(body[24], body[25], body[26], body[27])),
        6 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&body[24..40]);
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        other => {
            return Err(anyhow!(
                "HSRPv2 Group State TLV declares IP version {other}, expected 4 or 6"
            ))
        }
    };

    Ok(HsrpMessage {
        version: HsrpVersion::V2,
        opcode,
        state,
        // Integer-second view of a millisecond field; see the struct doc for what this costs.
        hellotime_secs: hellotime_ms / 1000,
        holdtime_secs: holdtime_ms / 1000,
        priority,
        group,
        auth_data: None,
        virtual_ip,
        identifier,
        md5_auth: None,
    })
}

/// Read the structural fields of an MD5 Authentication TLV, and **not** the digest.
///
/// Returns `None` rather than an error for a short TLV: a malformed authentication option is
/// not a reason to discard an otherwise well-formed advertisement, and the model is told
/// nothing rather than something wrong.
fn decode_v2_md5_auth(body: &[u8]) -> Option<Md5Auth> {
    if body.len() < usize::from(V2_MD5_AUTH_LEN) {
        return None;
    }
    Some(Md5Auth {
        algorithm: body[0],
        // body[1] is padding.
        flags: u16_at(body, 2),
        sender_address: Ipv4Addr::new(body[4], body[5], body[6], body[7]),
        key_id: u32_at(body, 8),
        // body[12..28] is the 16-byte digest. Deliberately not read: see `Md5Auth`.
    })
}
