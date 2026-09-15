//! `netget::server::stomp::frame::parse_frame` — STOMP framing, pre-authentication.
//!
//! STOMP is one of the six protocols whose decoder has already produced a crash by hand:
//! `content-length: 18446744073709551615` panicked in every test build and wrapped silently
//! in the shipped one, because `Cargo.toml` has no `[profile.dev]` and so `overflow-checks`
//! is on in debug and off in release. That is backwards from where you want to find it, and
//! it is precisely what a fuzzer built with debug assertions on does find.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::stomp::frame::{parse_frame, unescape_header, ParseOutcome};

fuzz_target!(|data: &[u8]| {
    match parse_frame(data) {
        Ok(ParseOutcome::Frame { frame, consumed }) => {
            assert!(
                consumed > 0 && consumed <= data.len(),
                "parse_frame consumed {} of {} bytes",
                consumed,
                data.len()
            );
            // Every header the parser hands back must survive being unescaped again; the
            // escape codec is applied to attacker-controlled text on the way out.
            for (k, v) in &frame.headers {
                let _ = unescape_header(k);
                let _ = unescape_header(v);
            }
        }
        Ok(ParseOutcome::Heartbeat { consumed }) => {
            assert!(
                consumed > 0 && consumed <= data.len(),
                "heartbeat consumed {} of {} bytes",
                consumed,
                data.len()
            );
        }
        Ok(ParseOutcome::Incomplete) | Err(_) => {}
    }

    let text = String::from_utf8_lossy(data);
    let _ = unescape_header(&text);
});
