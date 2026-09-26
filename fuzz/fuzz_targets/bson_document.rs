//! `netget::utils::bson_depth::scan_bson_document` — the iterative guard in front of
//! `bson::Document::from_reader`, which recurses once per embedded document with no depth limit.
//! It stands between the first `OP_MSG` of an unauthenticated MongoDB connection and that
//! decoder.
//!
//! Four properties are asserted, and the third is the one the guard exists for:
//!
//! 1. The guard never panics and never recurses, whatever bytes it is given.
//! 2. It is deterministic — the same bytes produce the same verdict twice.
//! 3. **Anything it reports `Complete`, `bson` survives**, decoding exactly the bytes the scan
//!    measured, as the server does. Handing accepted input to the real decoder is what makes
//!    this a test of the *pair*: removing `MAX_BSON_DEPTH` turns the `depth_bomb` seed into a
//!    stack overflow.
//! 4. **It never refuses a document `bson` accepts.** Where the guard says `Malformed`, `bson`
//!    must fail too. That is only asked of inputs too short to nest past the limit (the top
//!    document is at least 5 bytes and every further level at least 7), so decoding them
//!    unguarded here cannot overflow.
//!
//! `TooDeep` is never decoded: that is the input the guard is there to keep away.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::utils::bson_depth::{scan_bson_document, BsonScan, MAX_BSON_DEPTH};

/// No input this short can hold a document nested past [`MAX_BSON_DEPTH`].
const TOO_SHORT_TO_BE_DEEP: usize = 5 + 7 * MAX_BSON_DEPTH;

fuzz_target!(|data: &[u8]| {
    let verdict = scan_bson_document(data);

    assert_eq!(
        verdict,
        scan_bson_document(data),
        "scan_bson_document disagreed with itself"
    );

    match verdict {
        BsonScan::Complete { consumed } => {
            assert!(consumed <= data.len(), "consumed past the end of the input");
            // A stack overflow here is a SIGSEGV, not a panic, so libFuzzer reports it as a
            // crash. An `Err` is fine: the scan does not check UTF-8, boolean bytes or the old
            // binary subtype's inner length, none of which can open a level.
            let _ = bson::Document::from_reader(&data[..consumed]);
        }
        BsonScan::Malformed => {
            if data.len() < TOO_SHORT_TO_BE_DEEP {
                assert!(
                    bson::Document::from_reader(data).is_err(),
                    "the guard refused a document bson accepts"
                );
            }
        }
        BsonScan::TooDeep { .. } => {}
    }
});
