//! `netget::server::ldap` — LDAPMessage framing, the envelope, and the SearchRequest decoder
//! whose filter renderer is the only recursive code in any LDAP request.
//!
//! A search needs no bind, so a filter reaches `render_filter` from the first message of an
//! unauthenticated connection. The filter grammar is recursive (`&`, `|`, `!` each hold
//! filters) and `render_filter` recurses once per level, so the whole defence is its
//! `MAX_FILTER_DEPTH` check. The corpus carries `depth_bomb` — a 20,000-deep `(&(&(&…` inside a
//! real SearchRequest, well past the limit — and `at_depth_limit`, exactly 32 deep.
//!
//! The path is the server's own: `ldap_message_len` frames the stream (it is what refuses a
//! message over `MAX_LDAP_MESSAGE`), `decode_ldap_message` splits the envelope, and a
//! SearchRequest goes to `parse_search_request`. A framer that reports a length past the
//! buffer is an assertion here rather than a slice panic in the session.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::ldap::{decode_ldap_message, ldap_message_len, parse_search_request};

/// `[APPLICATION 3]`, constructed.
const OP_SEARCH_REQUEST: u8 = 0x63;

fn search(op_value: &[u8]) {
    if let Ok(fields) = parse_search_request(op_value) {
        // The rendered filter is bounded by the input: a renderer that manufactured text out of
        // nothing would be a different bug, but it is the one this catches.
        assert!(fields.filter.len() <= 16 + op_value.len() * 16);
        // Deterministic: the same bytes render the same filter.
        assert_eq!(parse_search_request(op_value).ok(), Some(fields));
    }
}

fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    for _ in 0..16 {
        match ldap_message_len(rest) {
            Ok(Some(len)) => {
                assert!(
                    len > 0 && len <= rest.len(),
                    "ldap_message_len returned {len} for {} buffered bytes",
                    rest.len()
                );
                if let Ok((_id, tag, op_value)) = decode_ldap_message(&rest[..len]) {
                    if tag == OP_SEARCH_REQUEST {
                        search(op_value);
                    }
                }
                rest = &rest[len..];
            }
            Ok(None) | Err(_) => break,
        }
    }

    // And the SearchRequest decoder on its own, so the filter renderer is reachable without a
    // well-formed envelope around it.
    search(data);
});
