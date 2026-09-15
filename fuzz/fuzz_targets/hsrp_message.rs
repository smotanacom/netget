//! `netget::server::hsrp::codec::decode` — HSRP v1 and v2 from one unauthenticated UDP
//! datagram. `decode` sniffs the version from the first byte and dispatches; the v2 path is
//! a TLV walker, which is the shape that has produced every walker bug in this tree.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::hsrp::codec::decode;

fuzz_target!(|data: &[u8]| {
    let first = decode(data);
    // Deterministic: no clock, no state, no interior mutability on this path.
    assert_eq!(
        first.is_ok(),
        decode(data).is_ok(),
        "hsrp::decode disagreed with itself"
    );
});
