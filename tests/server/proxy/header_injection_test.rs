//! A model-supplied header, path or query value with CR/LF in it must not reach the wire.
//!
//! `handle_request_modify` and `handle_response_modify` take `headers` from the model, and
//! the model reads the peer's own request, so a prompt-injected value such as
//! `"x\r\nContent-Length: 0\r\n\r\nGET /admin HTTP/1.1\r\nHost: internal"` used to be
//! written into the forwarded message verbatim: one header became a second request to the
//! upstream (inside the TLS session in MITM mode), or a second response to the client.
//! `new_path` and `query_params` sit on the space-delimited, CRLF-terminated request line,
//! so whitespace there ends the line the same way.
//!
//! The MITM response rebuild had a second desync of its own: it re-emitted the upstream's
//! `Content-Length` / `Transfer-Encoding` and then appended its own `Content-Length` over an
//! unchunked body — two lengths, or `chunked` with no chunks.

#![cfg(all(test, feature = "proxy"))]

use netget::llm::actions::protocol_trait::{ActionResult, Server};
use netget::server::proxy::actions::ProxyProtocol;
use netget::server::proxy::tls_mitm::rebuild_modified_response;
use serde_json::json;
use std::collections::HashMap;

/// The JSON the executor serialises, or `None` if it refused the action.
fn run(action: serde_json::Value) -> Option<serde_json::Value> {
    match ProxyProtocol::new().execute_action(action) {
        Ok(ActionResult::Output(bytes)) => {
            Some(serde_json::from_slice(&bytes).expect("proxy actions serialise JSON"))
        }
        Ok(other) => panic!("expected Output, got {:?}", std::mem::discriminant(&other)),
        Err(_) => None,
    }
}

const SMUGGLE: &str = "x\r\nContent-Length: 0\r\n\r\nGET /admin HTTP/1.1\r\nHost: internal\r\n";

#[test]
fn a_header_value_with_a_line_break_is_refused_on_both_modify_actions() {
    for action_type in ["handle_request_modify", "handle_response_modify"] {
        for bad in [SMUGGLE, "a\nb", "a\rb", "a\u{0}b", "a\u{1b}[31m"] {
            assert!(
                run(json!({"type": action_type, "headers": {"X-Note": bad}})).is_none(),
                "{action_type}: value {bad:?} must be refused"
            );
        }
        // A tab inside a value is legal field content.
        let ok = run(json!({"type": action_type, "headers": {"X-Note": "a\tb"}}))
            .unwrap_or_else(|| panic!("{action_type}: a tab in a value is legal"));
        assert_eq!(ok["headers"]["X-Note"], "a\tb");
    }
}

#[test]
fn a_header_name_that_is_not_a_token_is_refused() {
    for action_type in ["handle_request_modify", "handle_response_modify"] {
        for bad in ["X Note", "X:Note", "X-Note\r\nEvil", "", "Ñame"] {
            assert!(
                run(json!({"type": action_type, "headers": {bad: "v"}})).is_none(),
                "{action_type}: name {bad:?} must be refused"
            );
        }
        assert!(
            run(json!({"type": action_type, "headers": {"X-Request-Id": "abc-123"}})).is_some()
        );
    }
}

#[test]
fn framing_headers_cannot_be_added_but_can_be_removed() {
    for action_type in ["handle_request_modify", "handle_response_modify"] {
        for name in ["Content-Length", "content-length", "Transfer-Encoding"] {
            assert!(
                run(json!({"type": action_type, "headers": {name: "0"}})).is_none(),
                "{action_type}: adding {name} must be refused; the proxy frames the body"
            );
        }
        let ok = run(json!({"type": action_type, "remove_headers": ["Transfer-Encoding"]}))
            .unwrap_or_else(|| panic!("{action_type}: removing a framing header is fine"));
        assert_eq!(ok["remove_headers"][0], "Transfer-Encoding");
    }
}

#[test]
fn a_request_target_with_whitespace_or_a_line_break_is_refused() {
    for bad in ["/a b", "/a\r\nHost: x", "/a\tb", "/a\u{0}"] {
        assert!(
            run(json!({"type": "handle_request_modify", "new_path": bad})).is_none(),
            "new_path {bad:?} must be refused"
        );
        assert!(
            run(json!({"type": "handle_request_modify", "query_params": {"q": bad}})).is_none(),
            "query value {bad:?} must be refused"
        );
        assert!(
            run(json!({"type": "handle_request_modify", "query_params": {bad: "v"}})).is_none(),
            "query name {bad:?} must be refused"
        );
    }
    let ok = run(json!({"type": "handle_request_modify", "new_path": "/v2/items?x=1", "query_params": {"page": "2"}}))
        .expect("an ordinary path and query are accepted");
    assert_eq!(ok["new_path"], "/v2/items?x=1");
    assert_eq!(ok["query_params"]["page"], "2");
}

fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn header_lines(response: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(response);
    let (head, _) = text.split_once("\r\n\r\n").expect("a head/body separator");
    head.lines().skip(1).map(str::to_owned).collect()
}

#[test]
fn the_mitm_response_rebuild_emits_exactly_one_content_length_and_no_chunked_framing() {
    let upstream = headers(&[
        ("Content-Type", "text/plain"),
        ("Transfer-Encoding", "chunked"),
        ("Content-Length", "999"),
        ("Set-Cookie", "a=b"),
    ]);
    let out = rebuild_modified_response(200, &upstream, None, &[], b"hello");
    let lines = header_lines(&out);
    let lengths: Vec<&String> = lines
        .iter()
        .filter(|l| l.to_lowercase().starts_with("content-length:"))
        .collect();
    assert_eq!(lengths, vec!["Content-Length: 5"], "{lines:?}");
    assert!(
        !lines
            .iter()
            .any(|l| l.to_lowercase().starts_with("transfer-encoding:")),
        "the upstream's chunked framing must not describe an unchunked body: {lines:?}"
    );
    assert!(lines.contains(&"Content-Type: text/plain".to_string()));
    assert!(lines.contains(&"Set-Cookie: a=b".to_string()));
    assert!(out.ends_with(b"\r\n\r\nhello"));
    assert!(out.starts_with(b"HTTP/1.1 200 OK\r\n"));
}

#[test]
fn the_mitm_response_rebuild_drops_a_model_header_that_would_split_the_response() {
    let upstream = headers(&[("Content-Type", "text/plain")]);
    let added = headers(&[
        ("X-Injected", SMUGGLE),
        ("Content-Length", "0"),
        ("X-Fine", "yes"),
        ("content-type", "text/html"),
    ]);
    let out = rebuild_modified_response(404, &upstream, Some(&added), &[], b"nope");
    let text = String::from_utf8_lossy(&out);
    assert!(
        !text.contains("/admin"),
        "smuggled request reached the wire: {text:?}"
    );
    let lines = header_lines(&out);
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.to_lowercase().starts_with("content-length:"))
            .count(),
        1,
        "{lines:?}"
    );
    assert!(lines.contains(&"X-Fine: yes".to_string()));
    // A model header replaces the upstream's, case-insensitively.
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.to_lowercase().starts_with("content-type:"))
            .count(),
        1,
        "{lines:?}"
    );
    assert!(lines.contains(&"content-type: text/html".to_string()));
    assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
}

#[test]
fn the_mitm_response_rebuild_honours_remove_headers() {
    let upstream = headers(&[("Set-Cookie", "a=b"), ("Server", "x")]);
    let out = rebuild_modified_response(200, &upstream, None, &["set-cookie".to_string()], b"");
    let lines = header_lines(&out);
    assert!(
        !lines.iter().any(|l| l.starts_with("Set-Cookie")),
        "{lines:?}"
    );
    assert!(lines.contains(&"Server: x".to_string()));
    assert!(lines.contains(&"Content-Length: 0".to_string()));
}
