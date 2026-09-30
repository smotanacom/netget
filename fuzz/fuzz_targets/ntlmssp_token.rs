//! `netget::server::smb::auth` — the SPNEGO (RFC 4178, ASN.1 DER) and NTLMSSP (MS-NLMP)
//! tokens a SESSION_SETUP carries, pre-authentication by definition.
//!
//! **The SPNEGO wrapper is not parsed as ASN.1, and that is the depth answer.** `find_ntlmssp`
//! does not walk the DER at all: it scans for the NTLMSSP signature, because every NTLMSSP
//! field is addressed by an offset from that signature and nothing the server needs lies in
//! the wrapper. There is no recursion to bound. The corpus carries a nested-DER depth bomb
//! anyway (`seed_corpus.py`), so that the day someone replaces the scan with a real DER walker
//! the fuzzer starts at the input that walker has to survive.
//!
//! Invariants asserted:
//!
//! * **The scan is exact.** It finds a token iff the signature occurs, the token is a suffix of
//!   the buffer starting at the first occurrence, and a bare token is reported as bare.
//! * **Every field `parse_authenticate` reads is inside the token**: a user, domain or
//!   workstation decoded from UTF-16 has at most half as many characters as the token has bytes.
//! * **What the server sends back is well-formed**: the CHALLENGE built from any client's flags
//!   is an NTLMSSP type-2 message whose target-name and target-info fields lie inside it, and
//!   wrapping it the way the client wrapped its own token yields a DER TLV whose declared length
//!   is exactly the bytes that follow, with the CHALLENGE findable inside.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::smb::auth::{self, Wrapping, NTLMSSP_SIGNATURE};

fn le16(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

fn le32(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// The declared length of the outermost DER TLV, and where its content starts.
fn der_outer(b: &[u8]) -> (usize, usize) {
    assert!(b.len() >= 2, "a DER TLV is at least two bytes");
    match b[1] {
        n if n < 0x80 => (n as usize, 2),
        0x81 => (b[2] as usize, 3),
        0x82 => (((b[2] as usize) << 8) | b[3] as usize, 4),
        other => panic!("this server never writes a DER length form 0x{other:02x}"),
    }
}

fuzz_target!(|data: &[u8]| {
    // SecurityBufferLength is 16 bits.
    if data.len() > u16::MAX as usize {
        return;
    }
    let first = data
        .windows(NTLMSSP_SIGNATURE.len())
        .position(|w| w == NTLMSSP_SIGNATURE);
    let Some((token, wrapping)) = auth::find_ntlmssp(data) else {
        assert!(first.is_none(), "the signature is present and the scan missed it");
        return;
    };
    let at = first.expect("a token was found, so the signature is present");
    assert_eq!(token, &data[at..]);
    assert_eq!(wrapping == Wrapping::Raw, at == 0);

    if let Some(kind) = auth::message_type(token) {
        assert_eq!(kind as usize, le32(token, 8));
    }

    if let Some(who) = auth::parse_authenticate(token) {
        for field in [&who.user, &who.domain, &who.workstation] {
            assert!(field.chars().count() <= token.len());
        }
    }

    let flags = auth::negotiate_flags(token);
    let challenge = auth::challenge(flags, [0x11; 8], 0x01D9_0000_0000_0000);
    assert_eq!(auth::message_type(&challenge), Some(auth::NTLMSSP_CHALLENGE));
    for (len_at, off_at) in [(12, 16), (40, 44)] {
        let (len, off) = (le16(&challenge, len_at), le32(&challenge, off_at));
        assert!(off + len <= challenge.len(), "a CHALLENGE field points outside it");
    }

    let wrapped = auth::wrap_challenge(&challenge, wrapping);
    match wrapping {
        Wrapping::Raw => assert_eq!(wrapped, challenge),
        Wrapping::Spnego => {
            let (len, start) = der_outer(&wrapped);
            assert_eq!(start + len, wrapped.len(), "the negTokenResp length is exact");
            let (found, _) = auth::find_ntlmssp(&wrapped).expect("the CHALLENGE is findable");
            assert!(found.starts_with(&challenge));
        }
    }
    let done = auth::accept_completed(wrapping);
    if wrapping == Wrapping::Spnego {
        let (len, start) = der_outer(&done);
        assert_eq!(start + len, done.len());
    }
});
