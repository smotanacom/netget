//! `netget::server::m3ua::codec` — M3UA common header plus the parameter TLV walk.
//!
//! `CLAUDE.md` records m3ua as the protocol that had `MAX_MESSAGE_LEN` on decode and nothing
//! on encode. The walker advances by
//! `offset = (end + padding_for(declared)).min(body.len())`, which is exactly the shape
//! where an off-by-one either spins forever or overshoots — so this target drives the header
//! and the parameter walk from the same bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::m3ua::codec::{parse_header, parse_parameters, peek_class_type, Message};

fuzz_target!(|data: &[u8]| {
    let _ = parse_header(data);
    let _ = peek_class_type(data);
    let _ = parse_parameters(data);
    let _ = Message::parse(data);
});
