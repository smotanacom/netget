//! `netget::server::modbus::codec` — MBAP header framing plus PDU request parsing.
//!
//! Modbus is Beta, and `CLAUDE.md` names its accumulator in the unbounded-body class. The
//! server's real loop is `try_parse_adu` in a drain loop feeding each `adu.pdu` to
//! `parse_request`, so that is what this target does: a framer that mis-reports `consumed`
//! shows up here as an assertion rather than as a silent desync in production.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::modbus::codec::{parse_request, try_parse_adu};

fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    // Bounded so a `consumed == 0` bug is an assertion, not a libFuzzer timeout.
    for _ in 0..64 {
        match try_parse_adu(rest) {
            Ok(Some((adu, consumed))) => {
                assert!(
                    consumed > 0 && consumed <= rest.len(),
                    "try_parse_adu consumed {} of {} bytes",
                    consumed,
                    rest.len()
                );
                let _ = parse_request(&adu.pdu);
                rest = &rest[consumed..];
            }
            Ok(None) | Err(_) => break,
        }
    }

    // And the PDU parser on its own, so it is reachable without a well-formed MBAP header.
    let _ = parse_request(data);
});
