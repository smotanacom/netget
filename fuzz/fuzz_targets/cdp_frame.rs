//! `netget::server::cdp::codec` — the CDP TLV walker and its checksum.
//!
//! Same exposure as LLDP: link-local, unauthenticated, no handshake. CDP's own history in
//! this repository includes an EtherType-read-as-length defect, which is exactly what a
//! walker driven by a declared length gets wrong.
//!
//! The checksum is driven separately because it is a hand-rolled one's-complement sum with
//! an odd-length special case — CDP's checksum treats a trailing odd byte differently from
//! the standard IP checksum, and that branch is only reached on odd-length payloads.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::cdp::codec::{checksum, decode_frame, decode_payload, payload_checksum};

fuzz_target!(|data: &[u8]| {
    // The Ethernet/LLC header split, then the TLV walk over whatever it hands back.
    if let Ok((_hdr, payload)) = decode_frame(data) {
        let _ = decode_payload(payload);
        let _ = payload_checksum(payload);
    }

    // The walker on its own, without a valid frame header in front of it.
    let _ = decode_payload(data);

    // Infallible by signature, so every length — including the odd-length branch — must be
    // handled rather than refused.
    let _ = checksum(data);
});
