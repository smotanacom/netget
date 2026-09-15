//! `netget::utils::bencode::check_bencode_structure` — the shared iterative guard that
//! stands between an unauthenticated UDP datagram and `serde_bencode`, which has no depth
//! limit anywhere.
//!
//! Three properties are asserted, and the third is the one the guard exists for:
//!
//! 1. The guard itself never panics and never recurses, whatever bytes it is given.
//! 2. It is deterministic — the same bytes produce the same verdict twice.
//! 3. **Anything it accepts, `serde_bencode` survives.** That is the contract: the guard's
//!    whole job is to be the thing that decides, so accepting input the real decoder then
//!    dies on would make it worse than useless. Handing accepted input to `serde_bencode`
//!    here is what turns this target into a test of the *pair*, and it is why removing
//!    `MAX_BENCODE_DEPTH` makes this target abort with `stack overflow` in seconds.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::utils::bencode::check_bencode_structure;

fuzz_target!(|data: &[u8]| {
    let verdict = check_bencode_structure(data);

    // Deterministic: no interior mutability, no clock, no allocator dependence.
    assert_eq!(
        verdict.is_ok(),
        check_bencode_structure(data).is_ok(),
        "check_bencode_structure disagreed with itself"
    );

    if verdict.is_ok() {
        // The guard said yes. The decoder it guards must therefore survive these bytes.
        // A stack overflow here is a SIGSEGV, not a panic, so libFuzzer reports it as a
        // crash rather than as a caught assertion — which is exactly the class no other
        // test in this repository can observe.
        let _ = serde_bencode::from_bytes::<serde_bencode::value::Value>(data);
    }
});
