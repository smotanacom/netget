//! `netget::server::bolt::packstream` — Bolt's chunking and PackStream, pre-authentication.
//!
//! Everything after the 20-byte handshake is chunked PackStream, and the first message (HELLO)
//! is decoded before anyone has logged in. PackStream nests — one marker byte opens a list, map
//! or structure — so the decoder recurses, and `MAX_PACKSTREAM_DEPTH` is what keeps ~100 KB of
//! `0x91` from taking the whole process down with a stack overflow. Its other guard,
//! `check_declared`, refuses a count or length larger than the input could hold before
//! `Vec::with_capacity` is asked for it.
//!
//! Each input is driven two ways, because a byte string that is a good test of one is noise to
//! the other:
//!
//! 1. **As one message body** straight into `decode`, which is where the depth bomb and the
//!    oversized declared lengths in the corpus land. Whatever decodes must re-encode to bytes
//!    that decode to the same encoding (`encode ∘ decode` is idempotent), survive
//!    `parse_request` and the JSON conversion the event uses, and be dropped.
//! 2. **As a chunked stream** through `Dechunker` with the server's own 1 MiB cap, every
//!    complete message then taking path 1. A dechunker that returns a message longer than the
//!    cap, or more bytes than it was given, fails the assertion.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::bolt::messages::parse_request;
use netget::server::bolt::packstream::{decode, to_bytes, Dechunker, MAX_MESSAGE_BYTES};
use netget::server::bolt::values::value_to_json;

fn exercise(body: &[u8]) {
    let Ok(value) = decode(body) else {
        return;
    };
    let encoded = to_bytes(&value);
    let again = decode(&encoded).expect("NetGet's encoding of a decoded value must decode");
    assert_eq!(
        to_bytes(&again),
        encoded,
        "encode(decode(encode(v))) differs from encode(v)"
    );
    let _ = value_to_json(&value);
    let _ = parse_request(value);
    drop(again);
}

fuzz_target!(|data: &[u8]| {
    exercise(data);

    let mut dechunker = Dechunker::new(MAX_MESSAGE_BYTES);
    dechunker.push(data);
    let mut total = 0usize;
    // Bounded: a dechunker that returned a message without consuming input would loop.
    for _ in 0..1024 {
        match dechunker.next_message() {
            Ok(Some(message)) => {
                assert!(message.len() <= MAX_MESSAGE_BYTES, "message past the cap");
                total += message.len();
                assert!(
                    total <= data.len(),
                    "dechunker produced {total} bytes from a {}-byte input",
                    data.len()
                );
                exercise(&message);
            }
            Ok(None) | Err(_) => break,
        }
    }
});
