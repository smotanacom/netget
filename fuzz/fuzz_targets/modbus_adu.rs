//! `netget::server::modbus::codec` — MBAP header framing, PDU request parsing, and the
//! encoders that answer what was parsed.
//!
//! The server's real loop is `try_parse_adu` in a drain loop feeding each `adu.pdu` to
//! `parse_request`, so that is what this target does: a framer that mis-reports `consumed`
//! shows up here as an assertion rather than as a silent desync in production.
//!
//! Three more invariants are asserted, because the server relies on each of them without
//! checking it at runtime:
//!
//! * **Framing round-trips.** `encode_adu` over a parsed ADU's own fields reproduces the bytes
//!   it was parsed from. The server frames every reply with `encode_adu`; a disagreement
//!   between the two directions is a reply the client's framer reads differently.
//! * **Every accepted read can be answered.** `pdu_from_results` calls the bounded encoders
//!   and treats their refusal as unreachable, on the grounds that `parse_request` caps a read
//!   at 2000 bits or 125 registers. Here a read of any quantity `parse_request` accepts must
//!   encode, and fit an ADU.
//! * **A write's acknowledgement is a legal frame.** `encode_write_ack` is built from the
//!   parsed request; for FC 5/6 it must parse back as the same request (the echo), and every
//!   ack must fit an ADU.
//!
//! Modbus has no nesting, so this corpus has no depth bomb; its equivalent is a seed on every
//! length a peer declares (see `seed_corpus.py`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::modbus::codec::{
    encode_adu, encode_bits_response, encode_registers_response, encode_write_ack, parse_request,
    try_parse_adu, ModbusRequest, MAX_ADU_LEN,
};

fn check_request(request: &ModbusRequest) {
    let fc = request.function_code();
    if request.is_bit_read() {
        let values = vec![true; request.quantity() as usize];
        let pdu = encode_bits_response(fc, &values).unwrap_or_else(|e| {
            panic!("a bit read parse_request accepted cannot be answered: {e}")
        });
        let adu = encode_adu(0, 0, &pdu).expect("bit response must frame");
        assert!(adu.len() <= MAX_ADU_LEN);
    } else if request.is_register_read() {
        let values = vec![0xFFFFu16; request.quantity() as usize];
        let pdu = encode_registers_response(fc, &values).unwrap_or_else(|e| {
            panic!("a register read parse_request accepted cannot be answered: {e}")
        });
        let adu = encode_adu(0, 0, &pdu).expect("register response must frame");
        assert!(adu.len() <= MAX_ADU_LEN);
    } else {
        let ack = encode_write_ack(request);
        let adu = encode_adu(0, 0, &ack).expect("write ack must frame");
        assert!(adu.len() <= MAX_ADU_LEN);
        if matches!(
            request,
            ModbusRequest::WriteSingleCoil { .. } | ModbusRequest::WriteSingleRegister { .. }
        ) {
            assert_eq!(
                parse_request(&ack).as_ref(),
                Ok(request),
                "an FC 5/6 acknowledgement is the request echoed"
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    // Bounded so a `consumed == 0` bug is an assertion, not a libFuzzer timeout.
    for _ in 0..64 {
        match try_parse_adu(rest) {
            Ok(Some((adu, consumed))) => {
                assert!(
                    consumed > 0 && consumed <= rest.len() && consumed <= MAX_ADU_LEN,
                    "try_parse_adu consumed {} of {} bytes",
                    consumed,
                    rest.len()
                );
                let reencoded = encode_adu(adu.transaction_id, adu.unit_id, &adu.pdu)
                    .expect("a parsed ADU must re-encode");
                assert_eq!(
                    reencoded.as_slice(),
                    &rest[..consumed],
                    "framing does not round-trip"
                );
                if let Ok(request) = parse_request(&adu.pdu) {
                    check_request(&request);
                }
                rest = &rest[consumed..];
            }
            Ok(None) | Err(_) => break,
        }
    }

    // And the PDU parser on its own, so it is reachable without a well-formed MBAP header.
    if let Ok(request) = parse_request(data) {
        check_request(&request);
    }
});
