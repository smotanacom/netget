//! RADIUS wire format (RFC 2865 / RFC 2866).
//!
//! Pure encode/decode plus the two MD5 constructions the protocol actually needs. No I/O,
//! no state, no LLM — so it can be tested against literal RFC bytes.
//!
//! # What is implemented, and what is not
//!
//! Implemented, and exercised by tests:
//! - The 20-byte header (code, identifier, length, 16-byte authenticator) and TLV attributes.
//! - The **Response Authenticator**, `MD5(Code | ID | Length | RequestAuth | Attributes |
//!   Secret)` — RFC 2865 §3. A real client rejects the reply if this is wrong, so it is the
//!   one computation that cannot be faked.
//! - **User-Password** hiding/unhiding — RFC 2865 §5.2.
//! - The **Accounting-Request Authenticator**, `MD5(Code | ID | Length | 16 zero bytes |
//!   Attributes | Secret)` — RFC 2866 §3. This one is *verifiable*, and the server verifies it.
//!
//! Deliberately **not** implemented, and not claimed anywhere else:
//! - **Message-Authenticator (attribute 80)**, the HMAC-MD5 of RFC 3579 §3.2. It is neither
//!   computed nor verified. A packet carrying one is accepted, and the reply does not carry one.
//! - **CHAP (RFC 2865 §5.3)**, **MS-CHAP**, and **EAP (RFC 3579)**. CHAP-Password and
//!   EAP-Message are decoded to hex and handed to the model as opaque; no challenge is
//!   validated and no EAP state machine exists.
//! - Proxy-State forwarding semantics beyond echoing the attribute back.

use md5::{Digest, Md5};
use std::net::Ipv4Addr;

/// RADIUS packet codes (RFC 2865 §3, RFC 2866 §3, RFC 5997).
pub const CODE_ACCESS_REQUEST: u8 = 1;
pub const CODE_ACCESS_ACCEPT: u8 = 2;
pub const CODE_ACCESS_REJECT: u8 = 3;
pub const CODE_ACCOUNTING_REQUEST: u8 = 4;
pub const CODE_ACCOUNTING_RESPONSE: u8 = 5;
pub const CODE_ACCESS_CHALLENGE: u8 = 11;
pub const CODE_STATUS_SERVER: u8 = 12;

/// Fixed header size: code(1) + identifier(1) + length(2) + authenticator(16).
pub const HEADER_LEN: usize = 20;

/// RFC 2865 §3: "The minimum length is 20 and maximum length is 4096."
pub const MIN_PACKET_LEN: usize = 20;
pub const MAX_PACKET_LEN: usize = 4096;

/// Longest User-Password this format can carry, in octets (RFC 2865 §5.2: "The password is
/// hidden … If the password is longer than 128 characters, it is an error").
///
/// One constant for both directions on purpose. [`decode_user_password`] enforces it against
/// a hostile peer and [`encode_user_password`] against the caller building an Access-Request,
/// and the two used to disagree — the encoder had no bound at all, so it produced ciphertext
/// its own decoder rejects.
pub const MAX_USER_PASSWORD_LEN: usize = 128;

/// Attribute type numbers this server names. Anything else is reported to the model by
/// number, never dropped.
pub const ATTR_USER_NAME: u8 = 1;
pub const ATTR_USER_PASSWORD: u8 = 2;
pub const ATTR_CHAP_PASSWORD: u8 = 3;
pub const ATTR_NAS_IP_ADDRESS: u8 = 4;
pub const ATTR_NAS_PORT: u8 = 5;
pub const ATTR_SERVICE_TYPE: u8 = 6;
pub const ATTR_FRAMED_PROTOCOL: u8 = 7;
pub const ATTR_FRAMED_IP_ADDRESS: u8 = 8;
pub const ATTR_FRAMED_IP_NETMASK: u8 = 9;
pub const ATTR_FILTER_ID: u8 = 11;
pub const ATTR_FRAMED_MTU: u8 = 12;
pub const ATTR_REPLY_MESSAGE: u8 = 18;
pub const ATTR_STATE: u8 = 24;
pub const ATTR_CLASS: u8 = 25;
pub const ATTR_SESSION_TIMEOUT: u8 = 27;
pub const ATTR_IDLE_TIMEOUT: u8 = 28;
pub const ATTR_TERMINATION_ACTION: u8 = 29;
pub const ATTR_CALLED_STATION_ID: u8 = 30;
pub const ATTR_CALLING_STATION_ID: u8 = 31;
pub const ATTR_NAS_IDENTIFIER: u8 = 32;
pub const ATTR_PROXY_STATE: u8 = 33;
pub const ATTR_ACCT_STATUS_TYPE: u8 = 40;
pub const ATTR_ACCT_DELAY_TIME: u8 = 41;
pub const ATTR_ACCT_INPUT_OCTETS: u8 = 42;
pub const ATTR_ACCT_OUTPUT_OCTETS: u8 = 43;
pub const ATTR_ACCT_SESSION_ID: u8 = 44;
pub const ATTR_ACCT_AUTHENTIC: u8 = 45;
pub const ATTR_ACCT_SESSION_TIME: u8 = 46;
pub const ATTR_ACCT_TERMINATE_CAUSE: u8 = 49;
pub const ATTR_CHAP_CHALLENGE: u8 = 60;
pub const ATTR_NAS_PORT_TYPE: u8 = 61;
pub const ATTR_PORT_LIMIT: u8 = 62;
pub const ATTR_EAP_MESSAGE: u8 = 79;
pub const ATTR_MESSAGE_AUTHENTICATOR: u8 = 80;

/// How an attribute's value should be rendered for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrKind {
    /// UTF-8 text
    Text,
    /// 32-bit unsigned integer
    Integer,
    /// IPv4 address
    IpAddr,
    /// Opaque bytes; rendered as hex
    Octets,
}

/// The subset of the RADIUS dictionary this server names.
///
/// Returns `(canonical name, value kind)`. Unknown types get `Attribute-<n>` / `Octets`.
pub fn attribute_info(attr_type: u8) -> (&'static str, AttrKind) {
    match attr_type {
        ATTR_USER_NAME => ("User-Name", AttrKind::Text),
        ATTR_USER_PASSWORD => ("User-Password", AttrKind::Octets),
        ATTR_CHAP_PASSWORD => ("CHAP-Password", AttrKind::Octets),
        ATTR_NAS_IP_ADDRESS => ("NAS-IP-Address", AttrKind::IpAddr),
        ATTR_NAS_PORT => ("NAS-Port", AttrKind::Integer),
        ATTR_SERVICE_TYPE => ("Service-Type", AttrKind::Integer),
        ATTR_FRAMED_PROTOCOL => ("Framed-Protocol", AttrKind::Integer),
        ATTR_FRAMED_IP_ADDRESS => ("Framed-IP-Address", AttrKind::IpAddr),
        ATTR_FRAMED_IP_NETMASK => ("Framed-IP-Netmask", AttrKind::IpAddr),
        ATTR_FILTER_ID => ("Filter-Id", AttrKind::Text),
        ATTR_FRAMED_MTU => ("Framed-MTU", AttrKind::Integer),
        ATTR_REPLY_MESSAGE => ("Reply-Message", AttrKind::Text),
        ATTR_STATE => ("State", AttrKind::Octets),
        ATTR_CLASS => ("Class", AttrKind::Octets),
        ATTR_SESSION_TIMEOUT => ("Session-Timeout", AttrKind::Integer),
        ATTR_IDLE_TIMEOUT => ("Idle-Timeout", AttrKind::Integer),
        ATTR_TERMINATION_ACTION => ("Termination-Action", AttrKind::Integer),
        ATTR_CALLED_STATION_ID => ("Called-Station-Id", AttrKind::Text),
        ATTR_CALLING_STATION_ID => ("Calling-Station-Id", AttrKind::Text),
        ATTR_NAS_IDENTIFIER => ("NAS-Identifier", AttrKind::Text),
        ATTR_PROXY_STATE => ("Proxy-State", AttrKind::Octets),
        ATTR_ACCT_STATUS_TYPE => ("Acct-Status-Type", AttrKind::Integer),
        ATTR_ACCT_DELAY_TIME => ("Acct-Delay-Time", AttrKind::Integer),
        ATTR_ACCT_INPUT_OCTETS => ("Acct-Input-Octets", AttrKind::Integer),
        ATTR_ACCT_OUTPUT_OCTETS => ("Acct-Output-Octets", AttrKind::Integer),
        ATTR_ACCT_SESSION_ID => ("Acct-Session-Id", AttrKind::Text),
        ATTR_ACCT_AUTHENTIC => ("Acct-Authentic", AttrKind::Integer),
        ATTR_ACCT_SESSION_TIME => ("Acct-Session-Time", AttrKind::Integer),
        ATTR_ACCT_TERMINATE_CAUSE => ("Acct-Terminate-Cause", AttrKind::Integer),
        ATTR_CHAP_CHALLENGE => ("CHAP-Challenge", AttrKind::Octets),
        ATTR_NAS_PORT_TYPE => ("NAS-Port-Type", AttrKind::Integer),
        ATTR_PORT_LIMIT => ("Port-Limit", AttrKind::Integer),
        ATTR_EAP_MESSAGE => ("EAP-Message", AttrKind::Octets),
        ATTR_MESSAGE_AUTHENTICATOR => ("Message-Authenticator", AttrKind::Octets),
        _ => ("Unknown", AttrKind::Octets),
    }
}

/// True for the codes whose safe default is *denial* rather than silence.
///
/// Access-Request and Status-Server both ask a yes/no question, so a missing answer must
/// become Access-Reject. Accounting-Request asks nothing — it reports — so its safe default
/// is to send nothing and let the NAS retransmit.
pub fn is_authorization_request(code: u8) -> bool {
    matches!(code, CODE_ACCESS_REQUEST | CODE_STATUS_SERVER)
}

/// Human-readable name for a packet code.
pub fn code_name(code: u8) -> &'static str {
    match code {
        CODE_ACCESS_REQUEST => "Access-Request",
        CODE_ACCESS_ACCEPT => "Access-Accept",
        CODE_ACCESS_REJECT => "Access-Reject",
        CODE_ACCOUNTING_REQUEST => "Accounting-Request",
        CODE_ACCOUNTING_RESPONSE => "Accounting-Response",
        CODE_ACCESS_CHALLENGE => "Access-Challenge",
        CODE_STATUS_SERVER => "Status-Server",
        _ => "Unknown",
    }
}

/// One decoded attribute, still in wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub attr_type: u8,
    pub value: Vec<u8>,
}

impl Attribute {
    pub fn new(attr_type: u8, value: Vec<u8>) -> Self {
        Self { attr_type, value }
    }

    pub fn text(attr_type: u8, value: &str) -> Self {
        Self::new(attr_type, value.as_bytes().to_vec())
    }

    pub fn integer(attr_type: u8, value: u32) -> Self {
        Self::new(attr_type, value.to_be_bytes().to_vec())
    }

    pub fn ipv4(attr_type: u8, value: Ipv4Addr) -> Self {
        Self::new(attr_type, value.octets().to_vec())
    }

    /// Wire encoding: `Type(1) | Length(1, including this header) | Value`.
    ///
    /// A value longer than 253 bytes cannot be expressed and is rejected rather than
    /// silently truncated.
    pub fn encode(&self) -> Result<Vec<u8>, RadiusError> {
        if self.value.len() > 253 {
            return Err(RadiusError::AttributeTooLong {
                attr_type: self.attr_type,
                len: self.value.len(),
            });
        }
        let mut out = Vec::with_capacity(2 + self.value.len());
        out.push(self.attr_type);
        out.push((self.value.len() + 2) as u8);
        out.extend_from_slice(&self.value);
        Ok(out)
    }
}

/// A decoded RADIUS packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusPacket {
    pub code: u8,
    pub identifier: u8,
    pub authenticator: [u8; 16],
    pub attributes: Vec<Attribute>,
}

/// Anything that makes a datagram unusable. Every variant means "drop the packet",
/// which is what RFC 2865 §3 requires of a silently-discarded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadiusError {
    TooShort(usize),
    TooLong(usize),
    /// Header length field disagrees with the datagram
    LengthMismatch {
        declared: usize,
        actual: usize,
    },
    /// Attribute length byte is < 2 or runs past the end of the packet
    MalformedAttribute {
        offset: usize,
    },
    AttributeTooLong {
        attr_type: u8,
        len: usize,
    },
    /// User-Password ciphertext is not 16..=128 bytes, or not a multiple of 16
    BadPasswordLength(usize),
    /// Accounting-Request Authenticator did not verify against the shared secret
    BadAccountingAuthenticator,
    /// Encoded packet would exceed 4096 bytes
    ResponseTooLong(usize),
}

impl std::fmt::Display for RadiusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RadiusError::TooShort(n) => write!(f, "packet is {} bytes, minimum is 20", n),
            RadiusError::TooLong(n) => write!(f, "packet is {} bytes, maximum is 4096", n),
            RadiusError::LengthMismatch { declared, actual } => write!(
                f,
                "header declares {} bytes but datagram carries {}",
                declared, actual
            ),
            RadiusError::MalformedAttribute { offset } => {
                write!(f, "malformed attribute at offset {}", offset)
            }
            RadiusError::AttributeTooLong { attr_type, len } => write!(
                f,
                "attribute {} value is {} bytes, maximum is 253",
                attr_type, len
            ),
            RadiusError::BadPasswordLength(n) => write!(
                f,
                "User-Password is {} bytes; must be 16..=128 and a multiple of 16",
                n
            ),
            RadiusError::BadAccountingAuthenticator => {
                write!(f, "Accounting-Request Authenticator did not verify")
            }
            RadiusError::ResponseTooLong(n) => {
                write!(f, "response would be {} bytes, maximum is 4096", n)
            }
        }
    }
}

impl std::error::Error for RadiusError {}

impl RadiusPacket {
    /// Decode a datagram.
    ///
    /// RFC 2865 §3: octets outside the range of the Length field MUST be treated as padding
    /// and ignored on receipt, so a datagram longer than `length` is accepted and truncated.
    /// A datagram *shorter* than `length` is discarded.
    pub fn decode(data: &[u8]) -> Result<Self, RadiusError> {
        if data.len() < MIN_PACKET_LEN {
            return Err(RadiusError::TooShort(data.len()));
        }
        if data.len() > MAX_PACKET_LEN {
            return Err(RadiusError::TooLong(data.len()));
        }

        let declared = u16::from_be_bytes([data[2], data[3]]) as usize;
        if declared < MIN_PACKET_LEN || declared > data.len() {
            return Err(RadiusError::LengthMismatch {
                declared,
                actual: data.len(),
            });
        }

        let mut authenticator = [0u8; 16];
        authenticator.copy_from_slice(&data[4..20]);

        let attributes = decode_attributes(&data[HEADER_LEN..declared])?;

        Ok(Self {
            code: data[0],
            identifier: data[1],
            authenticator,
            attributes,
        })
    }

    /// First attribute of the given type, if any.
    pub fn first(&self, attr_type: u8) -> Option<&[u8]> {
        self.attributes
            .iter()
            .find(|a| a.attr_type == attr_type)
            .map(|a| a.value.as_slice())
    }

    /// All attributes of the given type, in order (EAP-Message and Proxy-State repeat).
    pub fn all(&self, attr_type: u8) -> Vec<&[u8]> {
        self.attributes
            .iter()
            .filter(|a| a.attr_type == attr_type)
            .map(|a| a.value.as_slice())
            .collect()
    }

    /// The wire bytes of just the attribute section.
    pub fn encoded_attributes(&self) -> Result<Vec<u8>, RadiusError> {
        let mut out = Vec::new();
        for attr in &self.attributes {
            out.extend_from_slice(&attr.encode()?);
        }
        Ok(out)
    }
}

/// Parse the attribute section: repeated `Type(1) | Length(1) | Value(Length-2)`.
pub fn decode_attributes(mut body: &[u8]) -> Result<Vec<Attribute>, RadiusError> {
    let total = body.len();
    let mut attributes = Vec::new();
    while !body.is_empty() {
        let offset = total - body.len();
        if body.len() < 2 {
            return Err(RadiusError::MalformedAttribute { offset });
        }
        let attr_type = body[0];
        let len = body[1] as usize;
        if len < 2 || len > body.len() {
            return Err(RadiusError::MalformedAttribute { offset });
        }
        attributes.push(Attribute::new(attr_type, body[2..len].to_vec()));
        body = &body[len..];
    }
    Ok(attributes)
}

/// Response Authenticator, RFC 2865 §3.
///
/// `MD5(Code | Identifier | Length | RequestAuthenticator | Attributes | Secret)`
///
/// Length is the *response's* total length (20 + attributes), not the request's. Getting
/// that wrong produces a reply every real client silently discards, which is
/// indistinguishable from the server being down — so it is worth stating explicitly.
pub fn response_authenticator(
    code: u8,
    identifier: u8,
    attributes: &[u8],
    request_authenticator: &[u8; 16],
    secret: &[u8],
) -> [u8; 16] {
    let length = (HEADER_LEN + attributes.len()) as u16;
    let mut hasher = Md5::new();
    hasher.update([code, identifier]);
    hasher.update(length.to_be_bytes());
    hasher.update(request_authenticator);
    hasher.update(attributes);
    hasher.update(secret);
    hasher.finalize().into()
}

/// Encode a reply, computing and inserting its Response Authenticator.
pub fn encode_response(
    code: u8,
    identifier: u8,
    attributes: &[Attribute],
    request_authenticator: &[u8; 16],
    secret: &[u8],
) -> Result<Vec<u8>, RadiusError> {
    let mut attr_bytes = Vec::new();
    for attr in attributes {
        attr_bytes.extend_from_slice(&attr.encode()?);
    }

    let total = HEADER_LEN + attr_bytes.len();
    if total > MAX_PACKET_LEN {
        return Err(RadiusError::ResponseTooLong(total));
    }

    let auth = response_authenticator(code, identifier, &attr_bytes, request_authenticator, secret);

    let mut out = Vec::with_capacity(total);
    out.push(code);
    out.push(identifier);
    out.extend_from_slice(&(total as u16).to_be_bytes());
    out.extend_from_slice(&auth);
    out.extend_from_slice(&attr_bytes);
    Ok(out)
}

/// Accounting-Request Authenticator, RFC 2866 §3.
///
/// `MD5(Code | Identifier | Length | 16 zero octets | Attributes | Secret)`
///
/// Unlike an Access-Request's authenticator (a random nonce, unverifiable), this one is a
/// keyed digest over the whole packet, so it *can* be checked — and is.
pub fn accounting_request_authenticator(
    identifier: u8,
    length: u16,
    attributes: &[u8],
    secret: &[u8],
) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update([CODE_ACCOUNTING_REQUEST, identifier]);
    hasher.update(length.to_be_bytes());
    hasher.update([0u8; 16]);
    hasher.update(attributes);
    hasher.update(secret);
    hasher.finalize().into()
}

/// Verify an Accounting-Request's authenticator against the shared secret.
///
/// A mismatch means the sender does not hold the secret; the packet must be dropped.
pub fn verify_accounting_request(packet: &RadiusPacket, secret: &[u8]) -> Result<(), RadiusError> {
    let attrs = packet.encoded_attributes()?;
    let length = (HEADER_LEN + attrs.len()) as u16;
    let expected = accounting_request_authenticator(packet.identifier, length, &attrs, secret);
    if constant_time_eq(&expected, &packet.authenticator) {
        Ok(())
    } else {
        Err(RadiusError::BadAccountingAuthenticator)
    }
}

/// Fixed-time 16-byte comparison, so a wrong secret cannot be recovered byte by byte.
fn constant_time_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Reverse the User-Password hiding of RFC 2865 §5.2.
///
/// ```text
/// b1 = MD5(S + RA)          p1 = c(1) xor b1
/// b2 = MD5(S + c(1))        p2 = c(2) xor b2
/// ...
/// ```
///
/// Trailing NULs are the pad the NAS added to reach a 16-octet boundary and are stripped.
/// The result is returned as bytes: a password is not guaranteed to be UTF-8, and callers
/// that need a string should say what they do with invalid sequences.
pub fn decode_user_password(
    ciphertext: &[u8],
    request_authenticator: &[u8; 16],
    secret: &[u8],
) -> Result<Vec<u8>, RadiusError> {
    if ciphertext.is_empty()
        || ciphertext.len() > MAX_USER_PASSWORD_LEN
        || !ciphertext.len().is_multiple_of(16)
    {
        return Err(RadiusError::BadPasswordLength(ciphertext.len()));
    }

    let mut plaintext = Vec::with_capacity(ciphertext.len());
    let mut previous: [u8; 16] = *request_authenticator;

    for chunk in ciphertext.chunks(16) {
        let mut hasher = Md5::new();
        hasher.update(secret);
        hasher.update(previous);
        let b: [u8; 16] = hasher.finalize().into();

        for (i, byte) in chunk.iter().enumerate() {
            plaintext.push(byte ^ b[i]);
        }
        previous.copy_from_slice(chunk);
    }

    // Strip the NUL padding the NAS added, per §5.2.
    while plaintext.last() == Some(&0) {
        plaintext.pop();
    }
    Ok(plaintext)
}

/// Apply the §5.2 hiding. Used only by tests and by anything that needs to *build* an
/// Access-Request; the server itself never encrypts a password.
///
/// Refuses a plaintext past [`MAX_USER_PASSWORD_LEN`], which is the same ceiling
/// [`decode_user_password`] enforces against the wire. Without the check the two directions
/// disagreed: a 129-octet plaintext encrypted happily into 144 octets of ciphertext that this
/// module's own decoder — and every RFC 2865 implementation — rejects.
pub fn encode_user_password(
    plaintext: &[u8],
    request_authenticator: &[u8; 16],
    secret: &[u8],
) -> Result<Vec<u8>, RadiusError> {
    if plaintext.len() > MAX_USER_PASSWORD_LEN {
        return Err(RadiusError::BadPasswordLength(plaintext.len()));
    }

    let mut padded = plaintext.to_vec();
    if padded.is_empty() {
        padded.resize(16, 0);
    } else if !padded.len().is_multiple_of(16) {
        let pad = 16 - (padded.len() % 16);
        padded.resize(padded.len() + pad, 0);
    }

    let mut out = Vec::with_capacity(padded.len());
    let mut previous: [u8; 16] = *request_authenticator;

    for chunk in padded.chunks(16) {
        let mut hasher = Md5::new();
        hasher.update(secret);
        hasher.update(previous);
        let b: [u8; 16] = hasher.finalize().into();

        let mut cipher = [0u8; 16];
        for i in 0..16 {
            cipher[i] = chunk[i] ^ b[i];
        }
        out.extend_from_slice(&cipher);
        previous = cipher;
    }
    Ok(out)
}

/// Render one attribute's value the way the model should see it: never raw bytes,
/// always a typed JSON value, with hex reserved for genuinely opaque fields.
pub fn attribute_value_json(attr_type: u8, value: &[u8]) -> serde_json::Value {
    let (_, kind) = attribute_info(attr_type);
    match kind {
        AttrKind::Text => serde_json::Value::String(String::from_utf8_lossy(value).into_owned()),
        AttrKind::Integer => {
            if value.len() == 4 {
                serde_json::Value::from(u32::from_be_bytes([
                    value[0], value[1], value[2], value[3],
                ]))
            } else {
                serde_json::Value::String(hex::encode(value))
            }
        }
        AttrKind::IpAddr => {
            if value.len() == 4 {
                serde_json::Value::String(
                    Ipv4Addr::new(value[0], value[1], value[2], value[3]).to_string(),
                )
            } else {
                serde_json::Value::String(hex::encode(value))
            }
        }
        AttrKind::Octets => serde_json::Value::String(hex::encode(value)),
    }
}
