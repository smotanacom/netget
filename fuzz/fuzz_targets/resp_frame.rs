//! `netget::utils::resp::scan_resp2_frame` — the iterative guard in front of
//! `redis_protocol::resp2::decode`, which recurses once per nested array with no depth limit.
//! It stands between the first bytes of an unauthenticated Redis connection and that decoder.
//!
//! Four properties are asserted, and the third is the one the guard exists for:
//!
//! 1. The guard never panics and never recurses, whatever bytes it is given.
//! 2. It is deterministic — the same bytes produce the same verdict twice.
//! 3. **Anything it reports `Complete`, `decode` survives**, and ends the frame at the same
//!    byte. Handing accepted input to the real decoder is what makes this a test of the
//!    *pair*: removing `MAX_RESP_DEPTH` turns the `depth_bomb` seed into a stack overflow.
//! 4. Where the guard says `Incomplete` or `Malformed`, `decode` agrees — `Ok(None)` or `Err`.
//!    The guard's module doc claims it follows the decoder's grammar exactly; this is the
//!    check on that claim. Both verdicts are only reached with nesting inside the bound, so
//!    decoding them here is safe.
//!
//! `TooDeep` and `TooLong` are never decoded: those are the inputs the guard is there to keep
//! away from the decoder.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::utils::resp::{scan_resp2_frame, RespLimits, RespScan};
use redis_protocol::resp2::decode::decode;

/// The Redis server's own limits (`MAX_PENDING_FRAME_BYTES` is 64 MiB).
const LIMITS: RespLimits = RespLimits::with_max_bulk_len(64 * 1024 * 1024);

fuzz_target!(|data: &[u8]| {
    let verdict = scan_resp2_frame(data, &LIMITS);

    assert_eq!(
        verdict,
        scan_resp2_frame(data, &LIMITS),
        "scan_resp2_frame disagreed with itself"
    );

    match verdict {
        RespScan::Complete { consumed } => {
            assert!(consumed <= data.len(), "consumed past the end of the input");
            // A stack overflow here is a SIGSEGV, not a panic, so libFuzzer reports it as a
            // crash — the class no ordinary test can observe.
            match decode(data) {
                Ok(Some((_, n))) => assert_eq!(
                    n, consumed,
                    "guard and decoder disagree about where the frame ends"
                ),
                Ok(None) => panic!("guard said Complete, decoder said Incomplete"),
                // `build_owned_frame` rejects a simple error that is not UTF-8, which the
                // guard does not check because it cannot affect nesting.
                Err(_) => {}
            }
        }
        RespScan::Incomplete => {
            assert!(
                matches!(decode(data), Ok(None)),
                "guard said Incomplete, decoder did not"
            );
        }
        RespScan::Malformed => {
            assert!(
                decode(data).is_err(),
                "guard said Malformed, decoder accepted or waited"
            );
        }
        RespScan::TooDeep { .. } | RespScan::TooLong(_) => {}
    }
});
