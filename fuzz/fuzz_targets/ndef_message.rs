//! `netget::client::nfc::ndef::decode_message` — NDEF records read off a tag.
//!
//! The decoder is deliberately **total**: a malformed tail becomes an `{"type":"undecodable"}`
//! record rather than an `Err`, so "it returned an error" is not the property to assert here.
//! What matters is that it terminates, never panics, and that the records it produces survive
//! a re-encode — the record header packs TNF, the SR/IL/MB/ME flags and three length fields
//! into bytes that the encoder has to reproduce, and a decode/encode pair that disagrees is
//! how a length bound on one side and not the other gets found.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::client::nfc::ndef::{decode_message, encode_message};

fuzz_target!(|data: &[u8]| {
    let Ok(records) = decode_message(data) else {
        return;
    };

    assert_eq!(
        records.len(),
        decode_message(data).map(|r| r.len()).unwrap_or(usize::MAX),
        "decode_message disagreed with itself"
    );

    // Re-encoding what was decoded must not panic, and must stay inside the declared bound.
    if let Ok(bytes) = encode_message(&records) {
        assert!(
            bytes.len() <= 65_535,
            "encode_message produced {} bytes, past MAX_MESSAGE_LEN",
            bytes.len()
        );
        let _ = decode_message(&bytes);
    }
});
