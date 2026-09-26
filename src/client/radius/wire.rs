//! RADIUS client packets: what the server's `packet.rs` leaves out because a server never needs
//! it — building requests, and checking a reply's two authenticators against the request.
//!
//! The header, attribute TLVs, User-Password hiding (RFC 2865 §5.2), the Response
//! Authenticator (§3) and the Accounting-Request Authenticator (RFC 2866 §3) are
//! `src/server/radius/packet.rs`, shared. This module adds the three constructions only a
//! client makes: CHAP-Password (RFC 2865 §2.2), the Message-Authenticator HMAC-MD5 (RFC 3579
//! §3.2) on both requests and replies, and the order they must be computed in.
//!
//! Pure functions; no I/O, no state. The shared secret is a parameter and is returned in
//! nothing.

use crate::server::radius::packet::{
    accounting_request_authenticator, encode_user_password, response_authenticator, Attribute,
    RadiusPacket, ATTR_CHAP_PASSWORD, ATTR_MESSAGE_AUTHENTICATOR, ATTR_USER_PASSWORD,
    CODE_ACCESS_ACCEPT, CODE_ACCESS_CHALLENGE, CODE_ACCESS_REJECT, CODE_ACCESS_REQUEST,
    CODE_ACCOUNTING_REQUEST, CODE_ACCOUNTING_RESPONSE, CODE_STATUS_SERVER, HEADER_LEN,
    MAX_PACKET_LEN,
};
use md5::{Digest, Md5};

/// HMAC-MD5 (RFC 2104), which RFC 3579's Message-Authenticator is.
pub fn hmac_md5(key: &[u8], message: &[u8]) -> [u8; 16] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..16].copy_from_slice(&Md5::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Md5::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Md5::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// CHAP-Password's value: the CHAP identifier, then `MD5(id | password | challenge)`.
///
/// The Request Authenticator is used as the challenge and no CHAP-Challenge attribute is sent,
/// which RFC 2865 §2.2 permits and every server implements.
pub fn chap_password(chap_id: u8, password: &[u8], challenge: &[u8; 16]) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update([chap_id]);
    hasher.update(password);
    hasher.update(challenge);
    let mut out = vec![chap_id];
    out.extend_from_slice(&hasher.finalize());
    out
}

/// How an Access-Request carries its password.
pub enum Credential<'a> {
    Pap(&'a [u8]),
    Chap(&'a [u8]),
    /// No password: a challenge response carrying only State, or a probe.
    None,
}

/// Why a packet could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildError(pub String);

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn header(
    code: u8,
    identifier: u8,
    authenticator: &[u8; 16],
    attrs: &[u8],
) -> Result<Vec<u8>, BuildError> {
    let total = HEADER_LEN + attrs.len();
    if total > MAX_PACKET_LEN {
        return Err(BuildError(format!(
            "the packet would be {total} bytes; RADIUS allows at most {MAX_PACKET_LEN}"
        )));
    }
    let mut out = Vec::with_capacity(total);
    out.push(code);
    out.push(identifier);
    // Range-checked above.
    out.extend_from_slice(&u16::try_from(total).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(authenticator);
    out.extend_from_slice(attrs);
    Ok(out)
}

fn encode_all(attributes: &[Attribute]) -> Result<Vec<u8>, BuildError> {
    let mut out = Vec::new();
    for a in attributes {
        out.extend_from_slice(&a.encode().map_err(|e| BuildError(e.to_string()))?);
    }
    Ok(out)
}

/// Append a Message-Authenticator to a finished packet and fill it in: HMAC-MD5 over the whole
/// packet with the attribute's value zeroed (RFC 3579 §3.2). The packet's length field is
/// updated first, because the HMAC covers it.
fn sign_with_message_authenticator(
    mut packet: Vec<u8>,
    secret: &[u8],
) -> Result<Vec<u8>, BuildError> {
    let at = packet.len();
    packet.extend_from_slice(&[ATTR_MESSAGE_AUTHENTICATOR, 18]);
    packet.extend_from_slice(&[0u8; 16]);
    if packet.len() > MAX_PACKET_LEN {
        return Err(BuildError(format!(
            "the packet would be {} bytes; RADIUS allows at most {MAX_PACKET_LEN}",
            packet.len()
        )));
    }
    let len = u16::try_from(packet.len()).unwrap_or(u16::MAX);
    packet[2..4].copy_from_slice(&len.to_be_bytes());
    let mac = hmac_md5(secret, &packet);
    packet[at + 2..at + 18].copy_from_slice(&mac);
    Ok(packet)
}

/// An Access-Request (RFC 2865 §4.1), always carrying a Message-Authenticator.
///
/// The Message-Authenticator is sent on every Access-Request, not only EAP ones: servers
/// patched for BlastRADIUS (CVE-2024-3596) require it, and it is what lets a reply be
/// authenticated by more than the MD5 Response Authenticator alone.
pub fn access_request(
    identifier: u8,
    request_authenticator: &[u8; 16],
    credential: Credential<'_>,
    attributes: &[Attribute],
    secret: &[u8],
) -> Result<Vec<u8>, BuildError> {
    let mut attrs = attributes.to_vec();
    match credential {
        Credential::Pap(password) => {
            let hidden = encode_user_password(password, request_authenticator, secret)
                .map_err(|e| BuildError(e.to_string()))?;
            attrs.push(Attribute::new(ATTR_USER_PASSWORD, hidden));
        }
        Credential::Chap(password) => {
            // The CHAP identifier only has to vary between challenges; the packet identifier
            // does.
            attrs.push(Attribute::new(
                ATTR_CHAP_PASSWORD,
                chap_password(identifier, password, request_authenticator),
            ));
        }
        Credential::None => {}
    }
    let packet = header(
        CODE_ACCESS_REQUEST,
        identifier,
        request_authenticator,
        &encode_all(&attrs)?,
    )?;
    sign_with_message_authenticator(packet, secret)
}

/// A Status-Server (RFC 5997 §3), which must carry a Message-Authenticator.
pub fn status_server(
    identifier: u8,
    request_authenticator: &[u8; 16],
    secret: &[u8],
) -> Result<Vec<u8>, BuildError> {
    let packet = header(CODE_STATUS_SERVER, identifier, request_authenticator, &[])?;
    sign_with_message_authenticator(packet, secret)
}

/// An Accounting-Request (RFC 2866 §4.1). Its authenticator is a keyed digest of the whole
/// packet, so it is computed here, and returned so replies can be checked against it.
pub fn accounting_request(
    identifier: u8,
    attributes: &[Attribute],
    secret: &[u8],
) -> Result<(Vec<u8>, [u8; 16]), BuildError> {
    let attrs = encode_all(attributes)?;
    let length = u16::try_from(HEADER_LEN + attrs.len())
        .ok()
        .filter(|l| usize::from(*l) <= MAX_PACKET_LEN)
        .ok_or_else(|| BuildError("accounting attributes exceed 4096 bytes".to_string()))?;
    let auth = accounting_request_authenticator(identifier, length, &attrs, secret);
    Ok((
        header(CODE_ACCOUNTING_REQUEST, identifier, &auth, &attrs)?,
        auth,
    ))
}

/// Why a reply was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyError {
    /// Not a RADIUS packet at all.
    Malformed(String),
    /// A code that does not answer the request (an Accounting-Response to an Access-Request).
    UnexpectedCode(u8),
    /// The Response Authenticator does not verify: the sender does not hold the secret, or the
    /// packet was altered.
    BadAuthenticator,
    /// A Message-Authenticator is present and does not verify.
    BadMessageAuthenticator,
    /// An Access-Accept/Reject/Challenge, or the answer to a Status-Server, without a
    /// Message-Authenticator. Refused rather than trusted on the MD5 authenticator alone
    /// (BlastRADIUS).
    MissingMessageAuthenticator,
}

impl ReplyError {
    pub fn kind(&self) -> &'static str {
        match self {
            ReplyError::Malformed(_) => "malformed",
            ReplyError::UnexpectedCode(_) => "unexpected_code",
            ReplyError::BadAuthenticator => "bad_authenticator",
            ReplyError::BadMessageAuthenticator => "bad_message_authenticator",
            ReplyError::MissingMessageAuthenticator => "missing_message_authenticator",
        }
    }
}

impl std::fmt::Display for ReplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplyError::Malformed(e) => write!(f, "not a RADIUS packet: {e}"),
            ReplyError::UnexpectedCode(c) => write!(
                f,
                "reply code {c} ({}) does not answer the request",
                crate::server::radius::packet::code_name(*c)
            ),
            ReplyError::BadAuthenticator => write!(
                f,
                "the Response Authenticator does not verify: the sender does not hold the \
                 shared secret, or the packet was altered"
            ),
            ReplyError::BadMessageAuthenticator => {
                write!(f, "the Message-Authenticator does not verify")
            }
            ReplyError::MissingMessageAuthenticator => write!(
                f,
                "the reply carries no Message-Authenticator, which this client requires on \
                 Access-Accept/Reject/Challenge and Status-Server answers"
            ),
        }
    }
}

/// The reply codes that answer a request code.
fn answers(request_code: u8, reply_code: u8) -> bool {
    match request_code {
        CODE_ACCESS_REQUEST => matches!(
            reply_code,
            CODE_ACCESS_ACCEPT | CODE_ACCESS_REJECT | CODE_ACCESS_CHALLENGE
        ),
        CODE_ACCOUNTING_REQUEST => reply_code == CODE_ACCOUNTING_RESPONSE,
        CODE_STATUS_SERVER => matches!(reply_code, CODE_ACCESS_ACCEPT | CODE_ACCOUNTING_RESPONSE),
        _ => false,
    }
}

/// Decode a reply and check it against the request it answers, in the order that matters:
/// the code, then the Response Authenticator (RFC 2865 §3), then the Message-Authenticator
/// (RFC 3579 §3.2, computed over the reply with the *Request* Authenticator in the
/// authenticator field). Nothing in an unverified reply reaches the model.
pub fn verify_reply(
    datagram: &[u8],
    request_code: u8,
    request_authenticator: &[u8; 16],
    secret: &[u8],
) -> Result<RadiusPacket, ReplyError> {
    let packet =
        RadiusPacket::decode(datagram).map_err(|e| ReplyError::Malformed(e.to_string()))?;
    if !answers(request_code, packet.code) {
        return Err(ReplyError::UnexpectedCode(packet.code));
    }
    // Bytes past the Length field are padding (RFC 2865 §3) and covered by neither digest.
    let declared = usize::from(u16::from_be_bytes([datagram[2], datagram[3]]));
    let attrs = &datagram[HEADER_LEN..declared];
    let expected = response_authenticator(
        packet.code,
        packet.identifier,
        attrs,
        request_authenticator,
        secret,
    );
    if !constant_time_eq(&expected, &packet.authenticator) {
        return Err(ReplyError::BadAuthenticator);
    }

    // Walk the attributes for the Message-Authenticator's offset.
    let mut offset = HEADER_LEN;
    let mut ma_at = None;
    while offset + 2 <= declared {
        let len = usize::from(datagram[offset + 1]);
        if datagram[offset] == ATTR_MESSAGE_AUTHENTICATOR && len == 18 {
            ma_at = Some(offset);
        }
        if len < 2 {
            break;
        }
        offset += len;
    }
    let requires_ma = request_code == CODE_STATUS_SERVER || request_code == CODE_ACCESS_REQUEST;
    match ma_at {
        None if requires_ma => Err(ReplyError::MissingMessageAuthenticator),
        None => Ok(packet),
        Some(at) => {
            let mut copy = datagram[..declared].to_vec();
            copy[4..20].copy_from_slice(request_authenticator);
            copy[at + 2..at + 18].fill(0);
            let mac = hmac_md5(secret, &copy);
            let mut got = [0u8; 16];
            got.copy_from_slice(&datagram[at + 2..at + 18]);
            if constant_time_eq(&mac, &got) {
                Ok(packet)
            } else {
                Err(ReplyError::BadMessageAuthenticator)
            }
        }
    }
}

fn constant_time_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}
