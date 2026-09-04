//! Pure EAPOL / EAP codec — IEEE 802.1X-2004 §11.3 and RFC 3748.
//!
//! No I/O, no locks, no async, no state. Everything here is a total function over bytes, so
//! `tests/server/eapol/codec_test.rs` can pin it against literal specification bytes without
//! a socket, an interface or a privilege. That split is the point: the transport in `mod.rs`
//! is a thin shell over these functions, and the transport is the part that has never run.
//!
//! # The one rule this file exists to enforce
//!
//! **An `EAP-Success` frame is an admission decision**, so there is exactly one function in
//! this crate that can produce those bytes — [`eapol_eap_success_frame`] — and it shares no
//! code with [`eapol_eap_failure_frame`]. Neither takes a code, a boolean, or anything else
//! that could select the other. Grep for `EAP_CODE_SUCCESS` and you will find the constant,
//! that one builder, and the classifier in `mod.rs` that recognises the byte after the fact.
//!
//! A generic `encode_eap_result(code, id)` would be shorter and is precisely what must not
//! exist: one wrong argument, one inverted condition, and an authentication bypass is a
//! single character wide.

use std::fmt;

// ---------------------------------------------------------------------------
// Wire constants
// ---------------------------------------------------------------------------

/// EtherType carrying EAPOL, IEEE 802.1X-2004 §11.3.1.
pub const ETHERTYPE_EAPOL: u16 = 0x888E;

/// The PAE group address supplicants send to, IEEE 802.1X-2004 §7.8 Table 7-2.
pub const PAE_GROUP_ADDRESS: [u8; 6] = [0x01, 0x80, 0xC2, 0x00, 0x00, 0x03];

/// Destination MAC (6) + source MAC (6) + EtherType (2).
pub const ETHERNET_HEADER_LEN: usize = 14;

/// Protocol version (1) + packet type (1) + body length (2).
pub const EAPOL_HEADER_LEN: usize = 4;

/// Code (1) + identifier (1) + length (2).
pub const EAP_HEADER_LEN: usize = 4;

/// An Ethernet frame is padded out to 60 octets before the FCS. EAPOL frames are far
/// shorter than that, so trailing padding is the normal case and must be ignored rather
/// than parsed.
pub const ETHERNET_MIN_FRAME_LEN: usize = 60;

// EAPOL packet types (IEEE 802.1X-2004 Table 11-3).
pub const EAPOL_TYPE_EAP_PACKET: u8 = 0;
pub const EAPOL_TYPE_START: u8 = 1;
pub const EAPOL_TYPE_LOGOFF: u8 = 2;
pub const EAPOL_TYPE_KEY: u8 = 3;
pub const EAPOL_TYPE_ASF_ALERT: u8 = 4;

/// 802.1X-2001. Still what many supplicants put on the wire.
pub const EAPOL_VERSION_2001: u8 = 1;
/// 802.1X-2004. NetGet's default.
pub const EAPOL_VERSION_2004: u8 = 2;
/// 802.1X-2010.
pub const EAPOL_VERSION_2010: u8 = 3;

// EAP codes (RFC 3748 §4).
pub const EAP_CODE_REQUEST: u8 = 1;
pub const EAP_CODE_RESPONSE: u8 = 2;
pub const EAP_CODE_SUCCESS: u8 = 3;
pub const EAP_CODE_FAILURE: u8 = 4;

// EAP method types (RFC 3748 §5, and the IANA EAP Method Type registry).
pub const EAP_TYPE_IDENTITY: u8 = 1;
pub const EAP_TYPE_NOTIFICATION: u8 = 2;
pub const EAP_TYPE_NAK: u8 = 3;
pub const EAP_TYPE_MD5_CHALLENGE: u8 = 4;
pub const EAP_TYPE_TLS: u8 = 13;
pub const EAP_TYPE_PEAP: u8 = 25;
pub const EAP_TYPE_MSCHAPV2: u8 = 26;

/// EAP-TLS flag bits, RFC 5216 §3.1. Shared by PEAP (RFC 7170 §3.1 uses the same layout).
pub const TLS_FLAG_LENGTH_INCLUDED: u8 = 0x80;
pub const TLS_FLAG_MORE_FRAGMENTS: u8 = 0x40;
pub const TLS_FLAG_START: u8 = 0x20;

/// RFC 1994 §2.2 recommends a challenge of at least the digest length.
pub const MD5_CHALLENGE_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can be wrong with a frame. Deliberately specific: an authenticator that
/// logs "bad packet" tells an operator nothing about which of six layers failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Fewer bytes than the layer's fixed header needs.
    TooShort {
        layer: &'static str,
        needed: usize,
        got: usize,
    },
    /// EAPOL protocol version outside 1..=3.
    UnsupportedVersion(u8),
    /// A declared length that the buffer cannot satisfy, or that is below the fixed header.
    BadLength {
        layer: &'static str,
        declared: usize,
        available: usize,
    },
    /// A Request or Response with no type octet, or a Success/Failure that is not 4 octets.
    MalformedEap(&'static str),
    /// Not the EtherType this server serves.
    NotEapol(u16),
    /// A MAC address that is not six colon- or dash-separated hex octets.
    BadMac(String),
    /// A field that will not fit the octet count the wire format gives it.
    FieldTooLong {
        field: &'static str,
        max: usize,
        got: usize,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::TooShort { layer, needed, got } => {
                write!(f, "{} needs at least {} octets, got {}", layer, needed, got)
            }
            CodecError::UnsupportedVersion(v) => write!(
                f,
                "EAPOL protocol version {} is not 1 (802.1X-2001), 2 (802.1X-2004) or 3 \
                 (802.1X-2010)",
                v
            ),
            CodecError::BadLength {
                layer,
                declared,
                available,
            } => write!(
                f,
                "{} declares a length of {} octets but only {} are present",
                layer, declared, available
            ),
            CodecError::MalformedEap(detail) => write!(f, "malformed EAP packet: {}", detail),
            CodecError::NotEapol(ethertype) => write!(
                f,
                "EtherType 0x{:04x} is not EAPOL (0x{:04x})",
                ethertype, ETHERTYPE_EAPOL
            ),
            CodecError::BadMac(s) => write!(
                f,
                "'{}' is not a MAC address; expected six hex octets like 02:00:00:00:00:01",
                s
            ),
            CodecError::FieldTooLong { field, max, got } => write!(
                f,
                "{} is {} octets; the wire format allows at most {}",
                field, got, max
            ),
        }
    }
}

impl std::error::Error for CodecError {}

pub type CodecResult<T> = Result<T, CodecError>;

// ---------------------------------------------------------------------------
// Ethernet
// ---------------------------------------------------------------------------

/// A parsed Ethernet II frame. `payload` has any trailing pad removed only insofar as the
/// EAPOL layer declares its own length — Ethernet itself cannot tell pad from data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthernetFrame {
    pub destination: [u8; 6],
    pub source: [u8; 6],
    pub ethertype: u16,
    pub payload: Vec<u8>,
}

pub fn parse_ethernet_frame(bytes: &[u8]) -> CodecResult<EthernetFrame> {
    if bytes.len() < ETHERNET_HEADER_LEN {
        return Err(CodecError::TooShort {
            layer: "Ethernet header",
            needed: ETHERNET_HEADER_LEN,
            got: bytes.len(),
        });
    }
    let mut destination = [0u8; 6];
    let mut source = [0u8; 6];
    destination.copy_from_slice(&bytes[0..6]);
    source.copy_from_slice(&bytes[6..12]);
    Ok(EthernetFrame {
        destination,
        source,
        ethertype: u16::from_be_bytes([bytes[12], bytes[13]]),
        payload: bytes[ETHERNET_HEADER_LEN..].to_vec(),
    })
}

/// Build an Ethernet II frame, padding the tail to the 60-octet minimum the MAC layer
/// requires. Pad octets are zero, and the EAPOL body-length field is what lets the receiver
/// ignore them.
pub fn build_ethernet_frame(
    destination: [u8; 6],
    source: [u8; 6],
    ethertype: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut out =
        Vec::with_capacity(ETHERNET_MIN_FRAME_LEN.max(ETHERNET_HEADER_LEN + payload.len()));
    out.extend_from_slice(&destination);
    out.extend_from_slice(&source);
    out.extend_from_slice(&ethertype.to_be_bytes());
    out.extend_from_slice(payload);
    if out.len() < ETHERNET_MIN_FRAME_LEN {
        out.resize(ETHERNET_MIN_FRAME_LEN, 0);
    }
    out
}

/// Lower-case colon-separated form, which is what every event field carries.
pub fn format_mac(mac: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Accepts `aa:bb:cc:dd:ee:ff`, `aa-bb-cc-dd-ee-ff` and `aabbccddeeff`, any case.
pub fn parse_mac(s: &str) -> CodecResult<[u8; 6]> {
    let cleaned: String = s
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.' | ' '))
        .collect();
    if cleaned.len() != 12 || !cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CodecError::BadMac(s.to_string()));
    }
    let mut mac = [0u8; 6];
    for (i, slot) in mac.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16)
            .map_err(|_| CodecError::BadMac(s.to_string()))?;
    }
    Ok(mac)
}

// ---------------------------------------------------------------------------
// EAPOL
// ---------------------------------------------------------------------------

/// An EAPOL PDU: protocol version, packet type, and the body the length field delimits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EapolFrame {
    pub version: u8,
    pub packet_type: u8,
    pub body: Vec<u8>,
}

impl EapolFrame {
    /// Decode an EAPOL PDU from the Ethernet payload.
    ///
    /// Trailing octets beyond the declared body length are **ignored**, not rejected: an
    /// EAPOL-Start is 4 octets and every Ethernet frame carrying one is padded to 60, so a
    /// strict length equality check would reject every real frame on the wire.
    pub fn decode(bytes: &[u8]) -> CodecResult<Self> {
        if bytes.len() < EAPOL_HEADER_LEN {
            return Err(CodecError::TooShort {
                layer: "EAPOL header",
                needed: EAPOL_HEADER_LEN,
                got: bytes.len(),
            });
        }
        let version = bytes[0];
        if !(EAPOL_VERSION_2001..=EAPOL_VERSION_2010).contains(&version) {
            return Err(CodecError::UnsupportedVersion(version));
        }
        let declared = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        let available = bytes.len() - EAPOL_HEADER_LEN;
        if declared > available {
            return Err(CodecError::BadLength {
                layer: "EAPOL body",
                declared,
                available,
            });
        }
        Ok(EapolFrame {
            version,
            packet_type: bytes[1],
            body: bytes[EAPOL_HEADER_LEN..EAPOL_HEADER_LEN + declared].to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(EAPOL_HEADER_LEN + self.body.len());
        out.push(self.version);
        out.push(self.packet_type);
        out.extend_from_slice(&(self.body.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

pub fn eapol_packet_type_name(packet_type: u8) -> &'static str {
    match packet_type {
        EAPOL_TYPE_EAP_PACKET => "EAP-Packet",
        EAPOL_TYPE_START => "EAPOL-Start",
        EAPOL_TYPE_LOGOFF => "EAPOL-Logoff",
        EAPOL_TYPE_KEY => "EAPOL-Key",
        EAPOL_TYPE_ASF_ALERT => "EAPOL-Encapsulated-ASF-Alert",
        _ => "Unknown-EAPOL-Type",
    }
}

/// Wrap an already-encoded EAP packet in an EAPOL header.
///
/// This takes an opaque body and cannot choose an EAP code, which is why it is safe for both
/// directions. The Success and Failure builders below deliberately do **not** call it.
pub fn eapol_wrap_eap(version: u8, eap: &[u8]) -> Vec<u8> {
    EapolFrame {
        version,
        packet_type: EAPOL_TYPE_EAP_PACKET,
        body: eap.to_vec(),
    }
    .encode()
}

/// An EAPOL-Start, used by the supplicant role and by tests playing one.
pub fn eapol_start_frame(version: u8) -> Vec<u8> {
    vec![version, EAPOL_TYPE_START, 0x00, 0x00]
}

/// An EAPOL-Logoff.
pub fn eapol_logoff_frame(version: u8) -> Vec<u8> {
    vec![version, EAPOL_TYPE_LOGOFF, 0x00, 0x00]
}

// ---------------------------------------------------------------------------
// EAP
// ---------------------------------------------------------------------------

/// A decoded EAP packet. `eap_type` is present only for Request and Response (RFC 3748 §4.1);
/// Success and Failure have no type octet and are exactly four octets long (§4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EapPacket {
    pub code: u8,
    pub identifier: u8,
    pub eap_type: Option<u8>,
    pub type_data: Vec<u8>,
}

impl EapPacket {
    pub fn decode(bytes: &[u8]) -> CodecResult<Self> {
        if bytes.len() < EAP_HEADER_LEN {
            return Err(CodecError::TooShort {
                layer: "EAP header",
                needed: EAP_HEADER_LEN,
                got: bytes.len(),
            });
        }
        let code = bytes[0];
        let identifier = bytes[1];
        let declared = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
        if declared < EAP_HEADER_LEN {
            return Err(CodecError::BadLength {
                layer: "EAP packet",
                declared,
                available: bytes.len(),
            });
        }
        if declared > bytes.len() {
            return Err(CodecError::BadLength {
                layer: "EAP packet",
                declared,
                available: bytes.len(),
            });
        }
        let body = &bytes[..declared];

        match code {
            EAP_CODE_REQUEST | EAP_CODE_RESPONSE => {
                if declared < EAP_HEADER_LEN + 1 {
                    return Err(CodecError::MalformedEap(
                        "a Request or Response must carry a type octet (RFC 3748 §4.1)",
                    ));
                }
                Ok(EapPacket {
                    code,
                    identifier,
                    eap_type: Some(body[4]),
                    type_data: body[5..].to_vec(),
                })
            }
            EAP_CODE_SUCCESS | EAP_CODE_FAILURE => {
                if declared != EAP_HEADER_LEN {
                    return Err(CodecError::MalformedEap(
                        "a Success or Failure is exactly four octets (RFC 3748 §4.2)",
                    ));
                }
                Ok(EapPacket {
                    code,
                    identifier,
                    eap_type: None,
                    type_data: Vec::new(),
                })
            }
            _ => Err(CodecError::MalformedEap(
                "EAP code must be 1 (Request), 2 (Response), 3 (Success) or 4 (Failure)",
            )),
        }
    }
}

pub fn eap_code_name(code: u8) -> &'static str {
    match code {
        EAP_CODE_REQUEST => "EAP-Request",
        EAP_CODE_RESPONSE => "EAP-Response",
        EAP_CODE_SUCCESS => "EAP-Success",
        EAP_CODE_FAILURE => "EAP-Failure",
        _ => "Unknown-EAP-Code",
    }
}

pub fn eap_type_name(eap_type: u8) -> &'static str {
    match eap_type {
        EAP_TYPE_IDENTITY => "identity",
        EAP_TYPE_NOTIFICATION => "notification",
        EAP_TYPE_NAK => "nak",
        EAP_TYPE_MD5_CHALLENGE => "md5-challenge",
        EAP_TYPE_TLS => "tls",
        EAP_TYPE_PEAP => "peap",
        EAP_TYPE_MSCHAPV2 => "mschapv2",
        _ => "unknown",
    }
}

/// Encode a Request or Response body. Private: every caller goes through a named builder, so
/// no call site can pass an arbitrary code.
fn encode_eap_typed(
    code: u8,
    identifier: u8,
    eap_type: u8,
    type_data: &[u8],
) -> CodecResult<Vec<u8>> {
    let length = EAP_HEADER_LEN + 1 + type_data.len();
    if length > u16::MAX as usize {
        return Err(CodecError::FieldTooLong {
            field: "EAP type-data",
            max: u16::MAX as usize - EAP_HEADER_LEN - 1,
            got: type_data.len(),
        });
    }
    let mut out = Vec::with_capacity(length);
    out.push(code);
    out.push(identifier);
    out.extend_from_slice(&(length as u16).to_be_bytes());
    out.push(eap_type);
    out.extend_from_slice(type_data);
    Ok(out)
}

/// `EAP-Request/Identity` — the first thing an authenticator says. Carries no prompt.
pub fn eap_request_identity(identifier: u8) -> Vec<u8> {
    // 4-octet header + 1 type octet; cannot exceed u16, so the Result is not interesting.
    vec![EAP_CODE_REQUEST, identifier, 0x00, 0x05, EAP_TYPE_IDENTITY]
}

/// `EAP-Request/Notification` — a displayable message, RFC 3748 §5.2. Not a decision.
pub fn eap_request_notification(identifier: u8, text: &str) -> CodecResult<Vec<u8>> {
    encode_eap_typed(
        EAP_CODE_REQUEST,
        identifier,
        EAP_TYPE_NOTIFICATION,
        text.as_bytes(),
    )
}

/// `EAP-Request/MD5-Challenge` — RFC 3748 §5.4, whose value field follows RFC 1994 §4.1:
/// `Value-Size (1) | Value | Name`.
pub fn eap_request_md5_challenge(
    identifier: u8,
    challenge: &[u8],
    name: &str,
) -> CodecResult<Vec<u8>> {
    let value = encode_md5_value(challenge, name)?;
    encode_eap_typed(EAP_CODE_REQUEST, identifier, EAP_TYPE_MD5_CHALLENGE, &value)
}

/// `EAP-Request/TLS` or `/PEAP` carrying only the Start flag, RFC 5216 §3.2.
///
/// NetGet sends the Start and surfaces whatever the supplicant answers; it does not carry a
/// TLS handshake. `metadata()` and the module's CLAUDE.md both say so.
pub fn eap_request_tls_start(identifier: u8, eap_type: u8) -> CodecResult<Vec<u8>> {
    encode_eap_typed(EAP_CODE_REQUEST, identifier, eap_type, &[TLS_FLAG_START])
}

/// Encode a Response body. Used by the supplicant role and by tests playing a supplicant.
pub fn eap_response(identifier: u8, eap_type: u8, type_data: &[u8]) -> CodecResult<Vec<u8>> {
    encode_eap_typed(EAP_CODE_RESPONSE, identifier, eap_type, type_data)
}

/// `EAP-Response/Identity` carrying a claimed identity.
pub fn eap_response_identity(identifier: u8, identity: &str) -> CodecResult<Vec<u8>> {
    encode_eap_typed(
        EAP_CODE_RESPONSE,
        identifier,
        EAP_TYPE_IDENTITY,
        identity.as_bytes(),
    )
}

// ---------------------------------------------------------------------------
// The two terminal frames. Read the module header before touching either.
// ---------------------------------------------------------------------------

/// **The only function in NetGet that can produce an `EAP-Success`.**
///
/// It takes a version and an identifier and nothing else. There is no code parameter, no
/// boolean, and no shared helper with [`eapol_eap_failure_frame`] — the eight octets are
/// literals here and literals there. `EapolServer::decide` is the single caller, reached
/// only from the `send_eap_success` arm of `execute_action` on a protocol instance that
/// carries an established identity.
///
/// Layout (802.1X-2004 §11.3 + RFC 3748 §4.2):
/// `version | 0x00 EAP-Packet | 0x0004 body length | 0x03 Success | id | 0x0004 length`
pub fn eapol_eap_success_frame(version: u8, identifier: u8) -> Vec<u8> {
    vec![
        version,
        EAPOL_TYPE_EAP_PACKET,
        0x00,
        0x04,
        EAP_CODE_SUCCESS,
        identifier,
        0x00,
        0x04,
    ]
}

/// The denial. Every fail-closed path in `mod.rs` calls this directly, bypassing the action
/// executor entirely, so no model output and no error path can reach the function above.
///
/// Layout is identical to [`eapol_eap_success_frame`] but for the code octet, and the two are
/// written out separately on purpose: a shared body parameterised by the code is one wrong
/// argument away from an authentication bypass.
pub fn eapol_eap_failure_frame(version: u8, identifier: u8) -> Vec<u8> {
    vec![
        version,
        EAPOL_TYPE_EAP_PACKET,
        0x00,
        0x04,
        EAP_CODE_FAILURE,
        identifier,
        0x00,
        0x04,
    ]
}

// ---------------------------------------------------------------------------
// Method payloads
// ---------------------------------------------------------------------------

/// `Value-Size (1) | Value | Name` — RFC 1994 §4.1, reused by EAP-MD5-Challenge.
pub fn encode_md5_value(value: &[u8], name: &str) -> CodecResult<Vec<u8>> {
    if value.len() > u8::MAX as usize {
        return Err(CodecError::FieldTooLong {
            field: "MD5-Challenge value",
            max: u8::MAX as usize,
            got: value.len(),
        });
    }
    let mut out = Vec::with_capacity(1 + value.len() + name.len());
    out.push(value.len() as u8);
    out.extend_from_slice(value);
    out.extend_from_slice(name.as_bytes());
    Ok(out)
}

/// Split an MD5-Challenge type-data field into its value and its trailing name.
pub fn decode_md5_value(type_data: &[u8]) -> CodecResult<(Vec<u8>, String)> {
    if type_data.is_empty() {
        return Err(CodecError::TooShort {
            layer: "MD5-Challenge value",
            needed: 1,
            got: 0,
        });
    }
    let size = type_data[0] as usize;
    if type_data.len() < 1 + size {
        return Err(CodecError::BadLength {
            layer: "MD5-Challenge value",
            declared: size,
            available: type_data.len() - 1,
        });
    }
    Ok((
        type_data[1..1 + size].to_vec(),
        String::from_utf8_lossy(&type_data[1 + size..]).into_owned(),
    ))
}

/// Flags on an EAP-TLS/PEAP fragment, RFC 5216 §3.1, as a structured triple. Returns `None`
/// when the fragment carries no flags octet at all.
pub fn tls_flags(type_data: &[u8]) -> Option<(bool, bool, bool)> {
    type_data.first().map(|f| {
        (
            f & TLS_FLAG_LENGTH_INCLUDED != 0,
            f & TLS_FLAG_MORE_FRAGMENTS != 0,
            f & TLS_FLAG_START != 0,
        )
    })
}

// ---------------------------------------------------------------------------
// MD5 (RFC 1321)
// ---------------------------------------------------------------------------
//
// Hand-rolled because the `md-5` crate is an optional dependency gated on the `radius`
// feature and `eapol = ["pnet", "dep:pcap"]` does not pull it in; adding it would mean
// editing Cargo.toml, which this module is not allowed to do. It is ~60 lines of pure
// arithmetic, checked in `tests/server/eapol/codec_test.rs` against the RFC 1321 §A.5
// published digest suite — an oracle written by neither this file nor its test.
//
// If `dep:md-5` is ever added to the `eapol` feature, delete this block and use `md5::Md5`
// as `src/server/radius/packet.rs` does. Nothing else here would change.

const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `floor(2^32 * abs(sin(i + 1)))`, RFC 1321 §3.4.
const MD5_SINE: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// RFC 1321 MD5. Pure; no allocation beyond the padded message.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_le_bytes());

    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];

    for chunk in message.as_chunks::<64>().0 {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }

        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let rotated = f
                .wrapping_add(a)
                .wrapping_add(MD5_SINE[i])
                .wrapping_add(m[g])
                .rotate_left(MD5_SHIFTS[i]);
            b = b.wrapping_add(rotated);
            a = tmp;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut out = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// RFC 1994 §2.2: `MD5(Identifier || Secret || Challenge)`.
///
/// The order matters and is the classic implementation mistake — a digest computed over
/// `Secret || Identifier || Challenge` is self-consistent and rejects every real supplicant.
pub fn md5_challenge_digest(identifier: u8, secret: &str, challenge: &[u8]) -> [u8; 16] {
    let mut input = Vec::with_capacity(1 + secret.len() + challenge.len());
    input.push(identifier);
    input.extend_from_slice(secret.as_bytes());
    input.extend_from_slice(challenge);
    md5(&input)
}

/// Whether a supplicant's MD5-Challenge response matches the digest of the expected secret.
///
/// Comparison is constant-time in the length-equal case, as `radius` does for its
/// Accounting-Request authenticator: a timing side channel on an authentication comparison
/// is a real, if slow, oracle.
pub fn md5_response_matches(
    identifier: u8,
    secret: &str,
    challenge: &[u8],
    response_value: &[u8],
) -> bool {
    let expected = md5_challenge_digest(identifier, secret, challenge);
    if response_value.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(response_value.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}
