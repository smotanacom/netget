//! `netget::server::nfs::guard::RecordScreen` — the guard that decides every RPC record from
//! the length the peer *announced*, before anything is read or allocated for it.
//!
//! `nfsserve` 0.10.2 resizes buffers from a wire-supplied 31/32-bit length with no cap, so a
//! ~40-byte unauthenticated `MOUNTPROC3_MNT` carrying a `dirpath` length of `0xFFFFFFFF` asks
//! for gigabytes. `NFSTcpListener` owns its own `accept()` loop, so there is no seam inside
//! the crate — NetGet binds the public listener itself and runs this screen in front.
//!
//! The screen is **stateful across markers**: it accumulates `bytes_so_far` and
//! `fragments_so_far` and resets them when the last-fragment bit is set. A single marker
//! therefore proves almost nothing; the interesting input is a *sequence*, which is what this
//! target feeds it. The accumulator is where a reset that fires on the wrong condition, or
//! an addition that overflows, would let a record past the bound.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::nfs::guard::{garbage_args_reply, RecordScreen};

fuzz_target!(|data: &[u8]| {
    let mut screen = RecordScreen::new();
    let mut admitted_total: usize = 0;

    for chunk in data.chunks_exact(4) {
        let marker = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        match screen.admit_fragment(marker) {
            Ok(frag) => {
                // Every admitted fragment must be within the per-fragment bound, and the
                // running total within the per-record bound. `checked_add` rather than `+`
                // because an overflow here is the bug, not a panic we want to cause.
                admitted_total = admitted_total
                    .checked_add(frag.length)
                    .expect("admitted fragment lengths overflowed usize");
                assert!(
                    admitted_total <= 2 * 1024 * 1024,
                    "the screen admitted {} bytes into one record",
                    admitted_total
                );
                if frag.is_last {
                    // A finished record resets the accumulator; the next marker starts over.
                    assert!(screen.at_record_start(), "last fragment left mid-record");
                    admitted_total = 0;
                }
            }
            Err(_) => {
                // A refusal is answered in NFS's own vocabulary, built from the call's xid.
                let _ = garbage_args_reply(marker);
                break;
            }
        }
    }
});
