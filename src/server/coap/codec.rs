//! CoAP (RFC 7252) message codec.
//!
//! Hand-rolled on purpose: the server's encoder and the `coap` / `coap-lite` crates used
//! as the test peer are then genuinely independent implementations, so a round trip
//! through them is evidence rather than a tautology.
//!
//! Covers the 4-byte binary header, the four message types, tokens, the option
//! delta/length encoding with both extension forms, the payload marker, and the
//! request/response code space. Not covered: Observe (RFC 7641), Block-wise transfer
//! (RFC 7959), DTLS.

use std::fmt;

/// Fixed CoAP header length: Ver/T/TKL, Code, Message ID.
pub const HEADER_LEN: usize = 4;

/// Marks the start of the payload, after any options.
pub const PAYLOAD_MARKER: u8 = 0xFF;

/// Largest response payload this server will put on the wire, in bytes.
///
/// RFC 7252 §4.6: absent any knowledge of the path MTU, a CoAP endpoint must assume
/// `MAX_MESSAGE_SIZE` of 1152 bytes, "leading to a maximum payload size of 1024 bytes". A
/// constrained peer is entitled to drop anything larger, and without Block-wise transfer
/// (RFC 7959, not implemented here) there is no legal way to split it.
///
/// The bound is enforced in `actions.rs::decode_payload`, on the model's side of the wire,
/// so an oversize representation is a legible error the model can act on. Without it
/// `send_to` fails with `EMSGSIZE` somewhere past 65507 bytes and the peer simply never
/// hears back - a fail-silent, which is the one outcome a request/response protocol must
/// not produce.
pub const MAX_PAYLOAD_LEN: usize = 1024;

/// CoAP version implemented here.
pub const VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Option numbers (RFC 7252 §5.10, §12.2)
// ---------------------------------------------------------------------------

pub const OPT_IF_MATCH: u16 = 1;
pub const OPT_URI_HOST: u16 = 3;
pub const OPT_ETAG: u16 = 4;
pub const OPT_IF_NONE_MATCH: u16 = 5;
pub const OPT_URI_PORT: u16 = 7;
pub const OPT_LOCATION_PATH: u16 = 8;
pub const OPT_URI_PATH: u16 = 11;
pub const OPT_CONTENT_FORMAT: u16 = 12;
pub const OPT_MAX_AGE: u16 = 14;
pub const OPT_URI_QUERY: u16 = 15;
pub const OPT_ACCEPT: u16 = 17;
pub const OPT_LOCATION_QUERY: u16 = 20;

// ---------------------------------------------------------------------------
// Codes
// ---------------------------------------------------------------------------

/// The empty code, 0.00 — used by ACK and RST, and by a CoAP ping.
pub const CODE_EMPTY: u8 = 0x00;
pub const CODE_GET: u8 = 0x01;
pub const CODE_POST: u8 = 0x02;
pub const CODE_PUT: u8 = 0x03;
pub const CODE_DELETE: u8 = 0x04;

/// Build a code byte from the human `class.detail` pair, e.g. `2.05` -> `code(2, 5)`.
pub const fn code(class: u8, detail: u8) -> u8 {
    (class << 5) | (detail & 0x1F)
}

pub const CODE_CONTENT: u8 = code(2, 5); // 2.05
pub const CODE_BAD_REQUEST: u8 = code(4, 0); // 4.00
pub const CODE_NOT_FOUND: u8 = code(4, 4); // 4.04
pub const CODE_METHOD_NOT_ALLOWED: u8 = code(4, 5); // 4.05
pub const CODE_INTERNAL_SERVER_ERROR: u8 = code(5, 0); // 5.00
pub const CODE_SERVICE_UNAVAILABLE: u8 = code(5, 3); // 5.03

/// Class portion of a code byte (the digit before the dot).
pub const fn code_class(code: u8) -> u8 {
    code >> 5
}

/// Detail portion of a code byte (the two digits after the dot).
pub const fn code_detail(code: u8) -> u8 {
    code & 0x1F
}

/// Render a code byte in the conventional `c.dd` notation.
pub fn code_to_string(code: u8) -> String {
    format!("{}.{:02}", code_class(code), code_detail(code))
}

/// Parse `"2.05"` (or `"2.05 Content"`) into a code byte.
pub fn parse_code_string(s: &str) -> Option<u8> {
    let token = s.split_whitespace().next()?;
    let (class, detail) = token.split_once('.')?;
    let class: u8 = class.trim().parse().ok()?;
    let detail: u8 = detail.trim().parse().ok()?;
    if class > 7 || detail > 31 {
        return None;
    }
    Some(code(class, detail))
}

/// Method name for a request code, or `None` if this is not a request code.
pub fn method_name(code: u8) -> Option<&'static str> {
    match code {
        CODE_GET => Some("GET"),
        CODE_POST => Some("POST"),
        CODE_PUT => Some("PUT"),
        CODE_DELETE => Some("DELETE"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Content formats (RFC 7252 §12.3)
// ---------------------------------------------------------------------------

/// Media type name for a Content-Format identifier.
pub fn content_format_name(id: u16) -> Option<&'static str> {
    match id {
        0 => Some("text/plain"),
        40 => Some("application/link-format"),
        41 => Some("application/xml"),
        42 => Some("application/octet-stream"),
        47 => Some("application/exi"),
        50 => Some("application/json"),
        60 => Some("application/cbor"),
        _ => None,
    }
}

/// Content-Format identifier for a media type name.
pub fn content_format_id(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_lowercase();
    // Tolerate the charset parameter clients and models both like to add.
    let n = n.split(';').next().unwrap_or(&n).trim().to_string();
    match n.as_str() {
        "text/plain" | "text" | "plain" => Some(0),
        "application/link-format" | "link-format" => Some(40),
        "application/xml" | "xml" => Some(41),
        "application/octet-stream" | "octet-stream" | "binary" => Some(42),
        "application/exi" | "exi" => Some(47),
        "application/json" | "json" => Some(50),
        "application/cbor" | "cbor" => Some(60),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

/// CoAP message type (the `T` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Confirmable,
    NonConfirmable,
    Acknowledgement,
    Reset,
}

impl MessageType {
    pub fn from_bits(bits: u8) -> MessageType {
        match bits & 0x03 {
            0 => MessageType::Confirmable,
            1 => MessageType::NonConfirmable,
            2 => MessageType::Acknowledgement,
            _ => MessageType::Reset,
        }
    }

    pub fn to_bits(self) -> u8 {
        match self {
            MessageType::Confirmable => 0,
            MessageType::NonConfirmable => 1,
            MessageType::Acknowledgement => 2,
            MessageType::Reset => 3,
        }
    }

    /// Short name used in event data and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageType::Confirmable => "CON",
            MessageType::NonConfirmable => "NON",
            MessageType::Acknowledgement => "ACK",
            MessageType::Reset => "RST",
        }
    }
}

/// A decoded CoAP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoapMessage {
    pub mtype: MessageType,
    pub code: u8,
    pub message_id: u16,
    pub token: Vec<u8>,
    /// Options as `(number, value)`, in ascending order of number, repeats allowed.
    pub options: Vec<(u16, Vec<u8>)>,
    pub payload: Vec<u8>,
}

/// Why a datagram is not a CoAP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    TooShort {
        len: usize,
    },
    BadVersion {
        version: u8,
    },
    BadTokenLength {
        tkl: u8,
    },
    Truncated {
        what: &'static str,
    },
    /// Option delta or length nibble 15, which is reserved (0xFF is the payload marker).
    ReservedOptionNibble,
    /// A payload marker with nothing after it (RFC 7252 §3: this is a format error).
    EmptyPayloadAfterMarker,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::TooShort { len } => {
                write!(
                    f,
                    "datagram of {len} bytes is shorter than the 4-byte CoAP header"
                )
            }
            DecodeError::BadVersion { version } => {
                write!(f, "CoAP version {version} is not supported (expected 1)")
            }
            DecodeError::BadTokenLength { tkl } => {
                write!(f, "token length {tkl} is reserved (must be 0-8)")
            }
            DecodeError::Truncated { what } => write!(f, "message truncated inside {what}"),
            DecodeError::ReservedOptionNibble => {
                write!(f, "option delta/length nibble 15 is reserved")
            }
            DecodeError::EmptyPayloadAfterMarker => {
                write!(f, "payload marker present but payload is empty")
            }
        }
    }
}

impl CoapMessage {
    /// Decode a datagram.
    pub fn decode(buf: &[u8]) -> Result<CoapMessage, DecodeError> {
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::TooShort { len: buf.len() });
        }

        let version = buf[0] >> 6;
        if version != VERSION {
            return Err(DecodeError::BadVersion { version });
        }
        let mtype = MessageType::from_bits(buf[0] >> 4);
        let tkl = buf[0] & 0x0F;
        if tkl > 8 {
            return Err(DecodeError::BadTokenLength { tkl });
        }
        let code = buf[1];
        let message_id = u16::from_be_bytes([buf[2], buf[3]]);

        let mut pos = HEADER_LEN;
        if buf.len() < pos + tkl as usize {
            return Err(DecodeError::Truncated { what: "token" });
        }
        let token = buf[pos..pos + tkl as usize].to_vec();
        pos += tkl as usize;

        let mut options: Vec<(u16, Vec<u8>)> = Vec::new();
        let mut current_number: u16 = 0;
        let mut payload = Vec::new();

        while pos < buf.len() {
            if buf[pos] == PAYLOAD_MARKER {
                pos += 1;
                if pos >= buf.len() {
                    return Err(DecodeError::EmptyPayloadAfterMarker);
                }
                payload = buf[pos..].to_vec();
                break;
            }

            let delta_nibble = buf[pos] >> 4;
            let length_nibble = buf[pos] & 0x0F;
            pos += 1;

            let delta = read_extended(buf, &mut pos, delta_nibble)?;
            let length = read_extended(buf, &mut pos, length_nibble)?;

            if buf.len() < pos + length as usize {
                return Err(DecodeError::Truncated {
                    what: "option value",
                });
            }
            current_number = current_number.saturating_add(delta);
            options.push((current_number, buf[pos..pos + length as usize].to_vec()));
            pos += length as usize;
        }

        Ok(CoapMessage {
            mtype,
            code,
            message_id,
            token,
            options,
            payload,
        })
    }

    /// Encode to a datagram.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.token.len() + self.payload.len() + 16);
        let tkl = self.token.len().min(8) as u8;
        out.push((VERSION << 6) | (self.mtype.to_bits() << 4) | tkl);
        out.push(self.code);
        out.extend_from_slice(&self.message_id.to_be_bytes());
        out.extend_from_slice(&self.token[..tkl as usize]);

        let mut sorted = self.options.clone();
        // Options must be emitted in ascending order; `sort_by_key` is stable, so
        // repeated options (Uri-Path segments) keep the order they were given in.
        sorted.sort_by_key(|(n, _)| *n);

        let mut last: u16 = 0;
        for (number, value) in &sorted {
            let delta = number.saturating_sub(last);
            last = *number;

            let (delta_nibble, delta_ext) = split_extended(delta);
            let (length_nibble, length_ext) = split_extended(value.len() as u16);

            out.push((delta_nibble << 4) | length_nibble);
            out.extend_from_slice(&delta_ext);
            out.extend_from_slice(&length_ext);
            out.extend_from_slice(value);
        }

        if !self.payload.is_empty() {
            out.push(PAYLOAD_MARKER);
            out.extend_from_slice(&self.payload);
        }

        out
    }

    /// Values of every occurrence of an option, in order.
    pub fn option_values(&self, number: u16) -> Vec<&[u8]> {
        self.options
            .iter()
            .filter(|(n, _)| *n == number)
            .map(|(_, v)| v.as_slice())
            .collect()
    }

    /// First value of an option, decoded as a big-endian unsigned integer.
    pub fn option_uint(&self, number: u16) -> Option<u32> {
        self.option_values(number)
            .first()
            .map(|v| v.iter().take(4).fold(0u32, |acc, b| (acc << 8) | *b as u32))
    }

    /// Request path assembled from the Uri-Path options, always leading-slashed.
    pub fn uri_path(&self) -> String {
        let segments: Vec<String> = self
            .option_values(OPT_URI_PATH)
            .iter()
            .map(|v| String::from_utf8_lossy(v).to_string())
            .collect();
        if segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", segments.join("/"))
        }
    }

    /// Uri-Path options as individual segments.
    pub fn path_segments(&self) -> Vec<String> {
        self.option_values(OPT_URI_PATH)
            .iter()
            .map(|v| String::from_utf8_lossy(v).to_string())
            .collect()
    }

    /// Query string assembled from the Uri-Query options, `&`-joined, or `None`.
    pub fn uri_query(&self) -> Option<String> {
        let parts: Vec<String> = self
            .option_values(OPT_URI_QUERY)
            .iter()
            .map(|v| String::from_utf8_lossy(v).to_string())
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("&"))
        }
    }

    /// True when this message is empty (code 0.00 and no token, options or payload).
    pub fn is_empty_message(&self) -> bool {
        self.code == CODE_EMPTY
    }

    /// True when the code is one of the four defined request methods.
    pub fn is_request(&self) -> bool {
        method_name(self.code).is_some()
    }
}

/// Build the piggybacked/separate response shell for a request.
///
/// The message id and token are taken from the request, which is why the model never
/// sees or supplies them: reliability matching is the transport's job, not a decision.
/// A Confirmable request is answered with an Acknowledgement carrying the same message
/// id (RFC 7252 §5.2.1); a Non-confirmable request is answered with a Non-confirmable
/// message carrying a fresh message id.
pub fn response_to(request: &CoapMessage, fresh_message_id: u16, code: u8) -> CoapMessage {
    let (mtype, message_id) = match request.mtype {
        MessageType::Confirmable => (MessageType::Acknowledgement, request.message_id),
        _ => (MessageType::NonConfirmable, fresh_message_id),
    };
    CoapMessage {
        mtype,
        code,
        message_id,
        token: request.token.clone(),
        options: Vec::new(),
        payload: Vec::new(),
    }
}

/// An empty Reset for the given message id (RFC 7252 §4.2).
pub fn reset_for(message_id: u16) -> CoapMessage {
    CoapMessage {
        mtype: MessageType::Reset,
        code: CODE_EMPTY,
        message_id,
        token: Vec::new(),
        options: Vec::new(),
        payload: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Option delta/length extension encoding
// ---------------------------------------------------------------------------

/// Read the extended value that a nibble of 13 or 14 introduces.
fn read_extended(buf: &[u8], pos: &mut usize, nibble: u8) -> Result<u16, DecodeError> {
    match nibble {
        0..=12 => Ok(nibble as u16),
        13 => {
            let b = *buf.get(*pos).ok_or(DecodeError::Truncated {
                what: "option extension byte",
            })?;
            *pos += 1;
            Ok(b as u16 + 13)
        }
        14 => {
            if buf.len() < *pos + 2 {
                return Err(DecodeError::Truncated {
                    what: "option extension bytes",
                });
            }
            let v = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]);
            *pos += 2;
            Ok(v.saturating_add(269))
        }
        _ => Err(DecodeError::ReservedOptionNibble),
    }
}

/// Split a delta or length into its nibble and extension bytes.
fn split_extended(value: u16) -> (u8, Vec<u8>) {
    if value < 13 {
        (value as u8, Vec::new())
    } else if value < 269 {
        (13, vec![(value - 13) as u8])
    } else {
        (14, (value - 269).to_be_bytes().to_vec())
    }
}
