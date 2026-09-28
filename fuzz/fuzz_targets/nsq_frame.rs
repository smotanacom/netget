//! `netget::server::nsq::wire` — the command parser every byte after an NSQ client's magic goes
//! through, the frame reader the tests use for the server's side, and the IDENTIFY and MPUB
//! body decoders: the first bytes an unauthenticated peer puts in front of the NSQ server.
//!
//! Asserted, for any input (with or without the leading `"  V2"`):
//!
//! 1. `parse_command` never panics, consumes at least one byte and never more than it was
//!    given, and anything it accepts respects the bounds the session loop relies on before it
//!    allocates: a body at most `MAX_BODY_SIZE`, a message at most `MAX_MSG_SIZE`, an MPUB at
//!    most `MAX_MPUB_MESSAGES` messages, an RDY count at most `MAX_RDY_COUNT`.
//! 2. Every accepted command re-encodes to bytes that parse back to the same command.
//! 3. A refusal is always fatal: a malformed command closes the connection, as in nsqd.
//! 4. `parse_frame`, `parse_message`, `split_mpub` and `parse_identify` never panic; the
//!    `identify_depth_bomb` seed is 65,536 nested arrays, which serde_json's recursion limit
//!    must turn into an error rather than a stack overflow.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::nsq::wire::{
    encode_command, parse_command, parse_frame, parse_identify, parse_message, split_mpub, Command,
    MAGIC_V2, MAX_BODY_SIZE, MAX_FRAME_DATA, MAX_MPUB_MESSAGES, MAX_MSG_SIZE, MAX_RDY_COUNT,
};

fuzz_target!(|data: &[u8]| {
    let _ = parse_identify(data);
    let _ = split_mpub(data);
    if let Ok(Some((frame, used))) = parse_frame(data) {
        assert!(used <= data.len() && frame.data.len() <= MAX_FRAME_DATA);
        let _ = parse_message(&frame.data);
    }

    let mut rest = data.strip_prefix(&MAGIC_V2[..]).unwrap_or(data);
    loop {
        match parse_command(rest) {
            Ok(Some((command, used))) => {
                assert!(
                    used > 0 && used <= rest.len(),
                    "consumed {used} of {}",
                    rest.len()
                );
                match &command {
                    Command::Pub { body, .. } | Command::Dpub { body, .. } => {
                        assert!(!body.is_empty() && body.len() <= MAX_MSG_SIZE)
                    }
                    Command::Mpub { messages, .. } => {
                        assert!(!messages.is_empty() && messages.len() <= MAX_MPUB_MESSAGES);
                        assert!(messages
                            .iter()
                            .all(|m| !m.is_empty() && m.len() <= MAX_MSG_SIZE));
                    }
                    Command::Identify(body) | Command::Auth(body) => {
                        assert!(!body.is_empty() && body.len() <= MAX_BODY_SIZE)
                    }
                    Command::Rdy(n) => assert!(*n <= MAX_RDY_COUNT),
                    _ => {}
                }
                let again = encode_command(&command);
                let (reparsed, reused) = parse_command(&again)
                    .expect("a re-encoded command parses")
                    .expect("a re-encoded command is complete");
                assert_eq!(reparsed, command);
                assert_eq!(reused, again.len());
                rest = &rest[used..];
            }
            Ok(None) => break,
            Err(e) => {
                assert!(e.fatal, "a malformed command closes the connection");
                break;
            }
        }
    }
});
