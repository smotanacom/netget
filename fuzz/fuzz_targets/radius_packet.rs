//! `netget::server::radius::packet` — RADIUS packet and attribute decoding, from one
//! unauthenticated UDP datagram.
//!
//! RADIUS attributes are length-prefixed and self-describing, and `decode_attributes` walks
//! them with its own offset arithmetic. `attribute_value_json` is the interesting second
//! half: it is infallible by signature, so every malformed value has to be handled rather
//! than refused, and it is called on attacker bytes for every attribute in the packet.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::radius::packet::{
    attribute_value_json, decode_attributes, decode_user_password, RadiusPacket,
};

fuzz_target!(|data: &[u8]| {
    if let Ok(pkt) = RadiusPacket::decode(data) {
        for attr in &pkt.attributes {
            // Infallible by signature: it must handle every shape, not refuse any.
            let _ = attribute_value_json(attr.attr_type, &attr.value);
        }
        // The User-Password unpadding walks 16-byte blocks of attacker input.
        if let Some(pw) = pkt.first(2) {
            let _ = decode_user_password(pw, &pkt.authenticator, b"secret");
        }
    }

    // The attribute walker on its own, without a valid 20-byte header in front of it.
    if let Ok(attrs) = decode_attributes(data) {
        for attr in &attrs {
            let _ = attribute_value_json(attr.attr_type, &attr.value);
        }
    }
});
