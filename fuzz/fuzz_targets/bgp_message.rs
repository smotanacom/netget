//! `netget::server::bgp::wire` — the BGP header check and the `netgauze` decoder behind it.
//!
//! Another guard/decoder pair, and a pre-authentication one: BGP's OPEN exchange happens
//! before anything resembling a session, so `parse_header` sees bytes from any host that can
//! complete a TCP handshake. The header carries a 16-byte marker and a declared length, and
//! the declared length is the number the rest of the decode trusts — the "bound the declared
//! size, not the remainder" class that NATS's `HPUB` limit got wrong.
//!
//! `asn4` is taken from the input because the four-byte-ASN capability changes how path
//! attributes are parsed, and a decoder that reads a two-byte field as four is exactly the
//! kind of length confusion worth reaching.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::bgp::wire::{decode, parse_header, BGP_HEADER_LEN};

fuzz_target!(|data: &[u8]| {
    let (asn4, body) = match data.split_first() {
        Some((&b, rest)) => (b & 1 == 1, rest),
        None => return,
    };

    if let Ok(header) =
        <&[u8; BGP_HEADER_LEN]>::try_from(body.get(..BGP_HEADER_LEN).unwrap_or_default())
    {
        if let Ok((declared_len, _msg_type)) = parse_header(header) {
            // The header check is the guard: a length it accepts is one the decoder will act
            // on, so it must be inside BGP's own 4096-byte frame limit and at least a header.
            assert!(
                (BGP_HEADER_LEN..=4096).contains(&declared_len),
                "parse_header admitted a declared length of {}",
                declared_len
            );
        }
    }

    // The decoder the guard guards, driven from the same bytes.
    let _ = decode(body, asn4);
});
