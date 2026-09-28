//! The authentication messages SESSION_SETUP carries: SPNEGO (RFC 4178) around NTLMSSP
//! (MS-NLMP).
//!
//! **This authenticates nothing.** No password is checked and no session key is derived: the
//! server walks a client through the three NTLMSSP messages because that is the only way a real
//! client will finish SESSION_SETUP, and it hands the *model* the user name the client
//! presented so the model can admit or refuse it. Every session it grants is reported to the
//! client as a guest session (or a null session, for an anonymous login), which is what tells
//! a client not to expect signing — there is no key to sign with.
//!
//! The encodings are fixed shapes written out by hand rather than a general ASN.1 codec: the
//! server emits three tokens and reads one field out of the client's, and a DER encoder for
//! three tokens is smaller than a dependency.

/// Signature at the start of every NTLMSSP message.
pub const NTLMSSP_SIGNATURE: &[u8; 8] = b"NTLMSSP\0";

pub const NTLMSSP_NEGOTIATE: u32 = 1;
pub const NTLMSSP_CHALLENGE: u32 = 2;
pub const NTLMSSP_AUTHENTICATE: u32 = 3;

// NegotiateFlags (MS-NLMP 2.2.2.5).
pub const NEGOTIATE_UNICODE: u32 = 0x0000_0001;
pub const NEGOTIATE_OEM: u32 = 0x0000_0002;
pub const REQUEST_TARGET: u32 = 0x0000_0004;
pub const NEGOTIATE_SIGN: u32 = 0x0000_0010;
pub const NEGOTIATE_SEAL: u32 = 0x0000_0020;
pub const NEGOTIATE_NTLM: u32 = 0x0000_0200;
pub const NEGOTIATE_ANONYMOUS: u32 = 0x0000_0800;
pub const NEGOTIATE_ALWAYS_SIGN: u32 = 0x0000_8000;
pub const TARGET_TYPE_SERVER: u32 = 0x0002_0000;
pub const NEGOTIATE_EXTENDED_SESSIONSECURITY: u32 = 0x0008_0000;
pub const NEGOTIATE_TARGET_INFO: u32 = 0x0080_0000;
pub const NEGOTIATE_VERSION: u32 = 0x0200_0000;
pub const NEGOTIATE_128: u32 = 0x2000_0000;
pub const NEGOTIATE_KEY_EXCH: u32 = 0x4000_0000;
pub const NEGOTIATE_56: u32 = 0x8000_0000;

/// Flags the server echoes when the client asked for them.
const ECHOED_FLAGS: u32 = NEGOTIATE_SIGN
    | NEGOTIATE_SEAL
    | NEGOTIATE_EXTENDED_SESSIONSECURITY
    | NEGOTIATE_VERSION
    | NEGOTIATE_128
    | NEGOTIATE_KEY_EXCH
    | NEGOTIATE_56;

/// Name the server gives itself in the CHALLENGE's target fields.
const TARGET_NETBIOS: &str = "NETGET";
const TARGET_DNS: &str = "netget";

/// DER encoding of the NTLMSSP mechanism OID, 1.3.6.1.4.1.311.2.2.10.
const NTLMSSP_OID: &[u8] = &[
    0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x02, 0x0a,
];
/// DER encoding of the SPNEGO OID, 1.3.6.1.5.5.2.
const SPNEGO_OID: &[u8] = &[0x06, 0x06, 0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];

/// How the client wrapped its NTLMSSP token, which is how the reply is wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrapping {
    /// A bare NTLMSSP message in the security buffer.
    Raw,
    /// NTLMSSP inside SPNEGO (`negTokenInit` first, `negTokenResp` after).
    Spnego,
}

/// Encode one DER TLV.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len <= 0xFF {
        out.extend_from_slice(&[0x81, len as u8]);
    } else {
        out.extend_from_slice(&[0x82, (len >> 8) as u8, len as u8]);
    }
    out.extend_from_slice(content);
    out
}

/// The security buffer of the NEGOTIATE response: a GSS-API `InitialContextToken` carrying an
/// SPNEGO `negTokenInit` that offers NTLMSSP and nothing else.
pub fn negotiate_blob() -> Vec<u8> {
    let mech_types = der(0x30, NTLMSSP_OID);
    let neg_token_init = der(0x30, &der(0xa0, &mech_types));
    let mut inner = SPNEGO_OID.to_vec();
    inner.extend_from_slice(&der(0xa0, &neg_token_init));
    der(0x60, &inner)
}

/// SPNEGO `negTokenResp` (RFC 4178 4.2.2).
fn neg_token_resp(neg_state: u8, supported_mech: bool, response_token: Option<&[u8]>) -> Vec<u8> {
    let mut seq = der(0xa0, &[0x0a, 0x01, neg_state]);
    if supported_mech {
        seq.extend_from_slice(&der(0xa1, NTLMSSP_OID));
    }
    if let Some(token) = response_token {
        seq.extend_from_slice(&der(0xa2, &der(0x04, token)));
    }
    der(0xa1, &der(0x30, &seq))
}

const NEG_STATE_ACCEPT_COMPLETED: u8 = 0;
const NEG_STATE_ACCEPT_INCOMPLETE: u8 = 1;

/// Wrap a CHALLENGE the way the client wrapped its NEGOTIATE.
pub fn wrap_challenge(challenge: &[u8], wrapping: Wrapping) -> Vec<u8> {
    match wrapping {
        Wrapping::Raw => challenge.to_vec(),
        Wrapping::Spnego => neg_token_resp(NEG_STATE_ACCEPT_INCOMPLETE, true, Some(challenge)),
    }
}

/// The final security buffer of a granted session: SPNEGO `accept-completed`, or nothing at
/// all for a bare NTLMSSP exchange, which has no fourth message.
pub fn accept_completed(wrapping: Wrapping) -> Vec<u8> {
    match wrapping {
        Wrapping::Raw => Vec::new(),
        Wrapping::Spnego => neg_token_resp(NEG_STATE_ACCEPT_COMPLETED, false, None),
    }
}

/// Locate the NTLMSSP message in a SESSION_SETUP security buffer.
///
/// SPNEGO carries it as the OCTET STRING of `mechToken` (in `negTokenInit`) or `responseToken`
/// (in `negTokenResp`). Rather than walk the DER, this finds the NTLMSSP signature: every field
/// of an NTLMSSP message is addressed by an offset from that signature, so whatever follows the
/// token inside the SPNEGO wrapper (a `mechListMIC`) sits past every field and is never read.
/// A buffer with no NTLMSSP signature at all is a mechanism this server does not speak.
pub fn find_ntlmssp(buffer: &[u8]) -> Option<(&[u8], Wrapping)> {
    if buffer.starts_with(NTLMSSP_SIGNATURE) {
        return Some((buffer, Wrapping::Raw));
    }
    let start = buffer
        .windows(NTLMSSP_SIGNATURE.len())
        .position(|w| w == NTLMSSP_SIGNATURE)?;
    Some((&buffer[start..], Wrapping::Spnego))
}

/// The `MessageType` of an NTLMSSP message.
pub fn message_type(token: &[u8]) -> Option<u32> {
    (token.len() >= 12 && token.starts_with(NTLMSSP_SIGNATURE)).then(|| le32(token, 8))
}

/// The `NegotiateFlags` of an NTLMSSP NEGOTIATE message (MS-NLMP 2.2.1.1).
pub fn negotiate_flags(token: &[u8]) -> u32 {
    if token.len() >= 16 {
        le32(token, 12)
    } else {
        NEGOTIATE_UNICODE
    }
}

/// Build an NTLMSSP CHALLENGE (MS-NLMP 2.2.1.2) answering a NEGOTIATE with `client_flags`.
pub fn challenge(client_flags: u32, server_challenge: [u8; 8], timestamp: u64) -> Vec<u8> {
    let flags = NEGOTIATE_UNICODE
        | REQUEST_TARGET
        | NEGOTIATE_NTLM
        | NEGOTIATE_ALWAYS_SIGN
        | TARGET_TYPE_SERVER
        | NEGOTIATE_TARGET_INFO
        | (client_flags & ECHOED_FLAGS);

    let target_name = utf16le(TARGET_NETBIOS);
    let mut target_info = Vec::new();
    for (id, value) in [
        (2u16, utf16le(TARGET_NETBIOS)),          // MsvAvNbDomainName
        (1u16, utf16le(TARGET_NETBIOS)),          // MsvAvNbComputerName
        (4u16, utf16le(TARGET_DNS)),              // MsvAvDnsDomainName
        (3u16, utf16le(TARGET_DNS)),              // MsvAvDnsComputerName
        (7u16, timestamp.to_le_bytes().to_vec()), // MsvAvTimestamp
    ] {
        target_info.extend_from_slice(&id.to_le_bytes());
        target_info.extend_from_slice(&(value.len() as u16).to_le_bytes());
        target_info.extend_from_slice(&value);
    }
    target_info.extend_from_slice(&[0, 0, 0, 0]); // MsvAvEOL

    const FIXED: usize = 56;
    let target_name_offset = FIXED as u32;
    let target_info_offset = target_name_offset + target_name.len() as u32;

    let mut out = Vec::with_capacity(FIXED + target_name.len() + target_info.len());
    out.extend_from_slice(NTLMSSP_SIGNATURE);
    out.extend_from_slice(&NTLMSSP_CHALLENGE.to_le_bytes());
    push_fields(&mut out, target_name.len(), target_name_offset);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&server_challenge);
    out.extend_from_slice(&[0u8; 8]); // Reserved
    push_fields(&mut out, target_info.len(), target_info_offset);
    // Version: 10.0.20348, NTLMSSP_REVISION_W2K3. Sent whether or not the client asked; the
    // eight bytes are in the fixed part either way and are ignored without the flag.
    out.extend_from_slice(&[10, 0]);
    out.extend_from_slice(&20348u16.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0x0F]);
    debug_assert_eq!(out.len(), FIXED);
    out.extend_from_slice(&target_name);
    out.extend_from_slice(&target_info);
    out
}

fn push_fields(out: &mut Vec<u8>, len: usize, offset: u32) {
    out.extend_from_slice(&(len as u16).to_le_bytes()); // Len
    out.extend_from_slice(&(len as u16).to_le_bytes()); // MaxLen
    out.extend_from_slice(&offset.to_le_bytes()); // BufferOffset
}

/// What an NTLMSSP AUTHENTICATE message says about who is logging in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticate {
    pub user: String,
    pub domain: String,
    pub workstation: String,
    /// MS-NLMP 3.2.5.1.2: an anonymous login carries an empty user name and an empty NT
    /// response (and an LM response that is empty or a single zero byte).
    pub anonymous: bool,
}

/// Parse an NTLMSSP AUTHENTICATE (MS-NLMP 2.2.1.3). Every field is bounds-checked against the
/// token; `None` if any is out of range.
pub fn parse_authenticate(token: &[u8]) -> Option<Authenticate> {
    if message_type(token)? != NTLMSSP_AUTHENTICATE || token.len() < 64 {
        return None;
    }
    let flags = le32(token, 60);
    let unicode = flags & NEGOTIATE_UNICODE != 0;
    let lm = field(token, 12)?;
    let nt = field(token, 20)?;
    let text = |off: usize| -> Option<String> {
        let bytes = field(token, off)?;
        if unicode {
            from_utf16le(bytes)
        } else {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    };
    let domain = text(28)?;
    let user = text(36)?;
    let workstation = text(44)?;
    let anonymous = (flags & NEGOTIATE_ANONYMOUS != 0)
        || (user.is_empty() && nt.is_empty() && (lm.is_empty() || lm == [0]));
    Some(Authenticate {
        user,
        domain,
        workstation,
        anonymous,
    })
}

/// The bytes a `Len`/`MaxLen`/`BufferOffset` field triple at `at` points to.
fn field(token: &[u8], at: usize) -> Option<&[u8]> {
    let len = u16::from_le_bytes([*token.get(at)?, *token.get(at + 1)?]) as usize;
    let offset = le32(token.get(at + 4..at + 8)?, 0) as usize;
    if len == 0 {
        return Some(&[]);
    }
    token.get(offset..offset.checked_add(len)?)
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn utf16le(s: &str) -> Vec<u8> {
    super::wire::utf16le(s)
}

fn from_utf16le(b: &[u8]) -> Option<String> {
    super::wire::from_utf16le(b)
}
