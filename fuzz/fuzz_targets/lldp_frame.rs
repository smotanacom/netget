//! `netget::server::lldp::codec` — the 802.1AB TLV walker.
//!
//! LLDP is a link-local broadcast protocol with no authentication of any kind: anything on
//! the segment can send a frame and it will be walked. Every TLV carries a 7-bit type and a
//! 9-bit length packed into two bytes, and the walker advances by the length the frame
//! declared — so this is the "bound the declared size" class again, in a decoder that sees
//! attacker bytes with no handshake in front of them at all.
//!
//! Both entry points are driven: `decode_frame` includes the Ethernet header, `Lldpdu::decode`
//! is the TLV walk on its own.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::lldp::codec::{decode_frame, Lldpdu};

fuzz_target!(|data: &[u8]| {
    let _ = decode_frame(data);
    let _ = Lldpdu::decode(data);
});
