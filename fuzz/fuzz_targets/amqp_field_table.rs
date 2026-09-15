//! `netget::server::amqp::codec` — AMQP field tables, the decoder whose depth bound was
//! verified by removing it and watching the test binary abort with `stack overflow`.
//!
//! Field tables nest, and the cost to the peer is **five bytes per level**, so one 128 KiB
//! frame bought roughly 26 000 levels before `MAX_FIELD_TABLE_DEPTH = 32` was added. The
//! table is read out of the connection-open method, which arrives before any authentication
//! has happened.
//!
//! `BasicProperties::decode` is driven from the same bytes: it is the other structure a peer
//! controls on the same connection, and it shares the `Decoder`'s cursor arithmetic.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::amqp::codec::{BasicProperties, Decoder};

fuzz_target!(|data: &[u8]| {
    // The field table walker, with its depth bound.
    let mut d = Decoder::new(data);
    let _ = d.field_table();

    // Its remaining-length accounting must never claim more than it was handed.
    assert!(
        d.remaining() <= data.len(),
        "Decoder::remaining reported {} of {} bytes",
        d.remaining(),
        data.len()
    );

    // The property block, from the same untrusted bytes and the same cursor arithmetic.
    let mut p = Decoder::new(data);
    let _ = BasicProperties::decode(&mut p);

    // And the string readers, which take a length from the wire and slice on it.
    let mut s = Decoder::new(data);
    let _ = s.short_string();
    let _ = s.long_string();
    let _ = s.long_bytes();
});
