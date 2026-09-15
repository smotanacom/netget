//! `netget::server::nfc::apdu::ApduCommand::parse` — ISO 7816-4 command APDUs.
//!
//! The APDU length encodings are the trap: a short APDU carries Lc in one byte, an extended
//! one carries a leading zero then two, and Le may be absent, one byte, two, or three. Every
//! combination is a different slice of attacker-controlled bytes, and the reader is reached
//! before anything resembling authentication.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::nfc::apdu::ApduCommand;

fuzz_target!(|data: &[u8]| {
    let _ = ApduCommand::parse(data);
});
