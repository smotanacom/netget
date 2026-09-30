//! `netget::server::smb::wire` — the compound walker and every request parser the SMB2
//! server reads a peer's bytes through.
//!
//! The session loop reads one Direct TCP frame whole (at most `MAX_MESSAGE_BYTES`) and walks it
//! with `next_in_chain`, one request per `NextCommand`; each handler then locates its fields
//! with one `parse_*` function. Every one of those is pure, so this target is the whole of the
//! peer-controlled parsing without a socket, a session or a model. It does what the loop does
//! — walk the chain — and hands **every** request to **every** parser, not only the one its
//! command names, because a parser that panics on another command's body panics on a lying
//! peer's body too.
//!
//! Invariants asserted, each one something the server relies on without checking at runtime:
//!
//! * **The chain walk terminates inside the frame.** Every located request is at least a header
//!   long, starts where the previous one's `NextCommand` pointed, and a `NextCommand` is
//!   8-aligned and never past the end, so the walk cannot run longer than `len / 64 + 1` steps.
//! * **A located slice is inside its message.** A WRITE's data is exactly `Length` bytes, a
//!   NEGOTIATE's dialect list is exactly `DialectCount` entries, a SESSION_SETUP's security
//!   buffer is inside the message.
//! * **A CREATE path is rooted and `/`-separated**, which is what the event the model sees
//!   promises.
//! * **Every reply header correlates.** `ResponseHeader::for_request` + `encode`, the only code
//!   that lays a header out, round-trips through `RequestHeader::parse` with the request's
//!   MessageId, TreeId, SessionId and command.
//!
//! SMB2 has no nesting: a compound chain is linear. Its equivalent of a depth bomb is a chain
//! of thousands of minimal headers, which `seed_corpus.py` seeds; the security buffer inside a
//! SESSION_SETUP is ASN.1 and gets its own target, `ntlmssp_token`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::smb::wire::{
    self, ChainError, RequestHeader, ResponseHeader, HEADER_LEN, PROTOCOL_ID,
};
use netget::server::smb::MAX_MESSAGE_BYTES;

fn check_request(req: &RequestHeader, message: &[u8]) {
    let body = wire::body(message);
    assert_eq!(body.len(), message.len() - HEADER_LEN);

    if let Some(dialects) = wire::parse_negotiate(body) {
        assert_eq!(dialects.len(), wire::le16(body, 2) as usize);
    }
    if let Some(blob) = wire::parse_session_setup(message) {
        assert!(blob.len() <= message.len());
        if !blob.is_empty() {
            let start = blob.as_ptr() as usize - message.as_ptr() as usize;
            assert!(start + blob.len() <= message.len());
        }
    }
    let _ = wire::parse_tree_connect(message);
    if let Some(create) = wire::parse_create(message) {
        assert!(create.path.starts_with('/'), "{:?}", create.path);
        assert!(!create.path.contains('\\'), "{:?}", create.path);
        assert!(create.path == "/" || !create.path.ends_with('/'));
    }
    let _ = wire::parse_close(body);
    let _ = wire::parse_flush(body);
    let _ = wire::parse_read(body);
    if let Some(write) = wire::parse_write(message) {
        if let Some(data) = write.data {
            assert_eq!(data.len(), write.length as usize);
        }
    }
    let _ = wire::parse_query_info(body);
    if let Some(query) = wire::parse_query_directory(message) {
        assert!(!query.pattern.is_empty() || wire::le16(body, 26) != 0);
    }

    let encoded = ResponseHeader::for_request(req, 0).encode();
    let back = RequestHeader::parse(&encoded).expect("a header this server encodes parses");
    assert_eq!(
        (back.command, back.message_id, back.tree_id, back.session_id),
        (req.command, req.message_id, req.tree_id, req.session_id),
        "a reply must carry the MessageId, TreeId and SessionId of its request"
    );
}

fuzz_target!(|data: &[u8]| {
    // The session loop never reads a frame longer than this.
    if data.len() > MAX_MESSAGE_BYTES {
        return;
    }
    let mut offset = 0usize;
    for _ in 0..=data.len() / HEADER_LEN {
        let rest = &data[offset..];
        match wire::next_in_chain(rest) {
            Ok(located) => {
                assert!(located.message.len() >= HEADER_LEN);
                assert!(located.message.len() <= rest.len());
                assert_eq!(located.message.as_ptr(), rest.as_ptr());
                assert_eq!(&located.message[..4], &PROTOCOL_ID);
                check_request(&located.header, located.message);
                match located.next {
                    Some(next) => {
                        assert!(next >= HEADER_LEN && next % 8 == 0 && next <= rest.len());
                        assert_eq!(next, located.message.len());
                        offset += next;
                    }
                    None => return,
                }
            }
            Err(ChainError::NotSmb2) => return,
            Err(ChainError::BadNextCommand(header)) => {
                // Answered with an error correlated to that request, and the walk stops.
                let reply = wire::error_response(&ResponseHeader::for_request(&header, 0));
                assert!(reply.len() > HEADER_LEN);
                return;
            }
        }
    }
    panic!("the chain walk ran longer than the frame could hold");
});
