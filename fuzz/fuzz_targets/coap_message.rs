//! `netget::server::coap::codec::CoapMessage::decode` — RFC 7252 over an unauthenticated
//! UDP socket. Hand-rolled on purpose, so there is no third-party decoder underneath it.
//!
//! The option walker is the interesting part: CoAP option deltas and lengths use 13/14
//! extension encodings, and the accumulated delta is what turns into an option number. This
//! target also round-trips whatever decodes, because `encode` is the direction `m3ua` proved
//! can carry a bound the decoder has and the encoder does not.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::coap::codec::CoapMessage;

fuzz_target!(|data: &[u8]| {
    let Ok(msg) = CoapMessage::decode(data) else {
        return;
    };

    // Accessors the server calls on every admitted datagram, each re-walking the options.
    let _ = msg.uri_path();
    let _ = msg.path_segments();
    let _ = msg.uri_query();
    let _ = msg.is_empty_message();
    let _ = msg.is_request();

    // Re-encoding an accepted message must itself decode. A message that decodes but whose
    // encoding does not is a desync the server would emit onto the wire.
    let reencoded = msg.encode();
    assert!(
        CoapMessage::decode(&reencoded).is_ok(),
        "a decoded CoAP message re-encoded into something undecodable"
    );
});
