//! `netget::server::nats::parse_frame` — the NATS control-line framer, reached on the first
//! bytes of an unauthenticated TCP connection.
//!
//! Two defects already found here by hand define what this target looks for. NATS recursed
//! once per blank line, so 8 KB of newlines in a single `read` overflowed the stack; and
//! `HPUB`'s size check was applied to `total - header`, leaving `header` unbounded, so
//! `HPUB x 4000000000 4000000000` — thirty bytes on the wire — buffered toward 4 GB and
//! overflowed the offset arithmetic on the way.
//!
//! `max_payload` is taken from the input rather than fixed, because the bound the server
//! applies is configurable and the interesting bugs are in the arithmetic *around* it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::nats::{blank_line_prefix_len, parse_frame, parse_headers, subject_matches};

fuzz_target!(|data: &[u8]| {
    // First byte chooses the payload bound; the rest is the wire input. Splitting this way
    // keeps the target deterministic while still exploring both sides of the limit.
    let (max_payload, buf) = match data.split_first() {
        Some((&b, rest)) => (u64::from(b) << 24, rest),
        None => return,
    };

    match parse_frame(buf, max_payload) {
        Ok(Some((_frame, consumed))) => {
            // A framer that reports consuming more than it was given, or zero, turns the
            // server's drain loop into an infinite one or a panicking slice.
            assert!(
                consumed > 0 && consumed <= buf.len(),
                "parse_frame consumed {} of {} bytes",
                consumed,
                buf.len()
            );
        }
        Ok(None) | Err(_) => {}
    }

    // The blank-line skipper is the one that used to recurse.
    let skipped = blank_line_prefix_len(buf);
    assert!(
        skipped <= buf.len(),
        "blank_line_prefix_len ran off the end"
    );

    // Header blocks and subject matching are parsed from the same untrusted bytes.
    let _ = parse_headers(buf);
    let text = String::from_utf8_lossy(buf);
    let _ = subject_matches(&text, "a.b.c");
    let _ = subject_matches("a.*.c", &text);
});
