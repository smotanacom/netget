//! The memcached client's reply reader, byte by byte, and every bound it declares.
//!
//! No server and no model: `wire::parse_reply` is a pure function over the bytes a server
//! sent and the request they answer. Each bound here was checked by removing it from
//! `src/client/memcached/wire.rs` and watching its test fail.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features memcached --test client -- memcached::wire_test --test-threads=100

use netget::client::memcached::actions::request_from_action;
use netget::client::memcached::wire::{
    parse_reply, Expect, Reply, Request, StoreVerb, WireError, MAX_REPLY_LINE, MAX_RESPONSE_BYTES,
    MAX_STATS_ENTRIES,
};
use serde_json::json;

fn get(keys: &[&str], with_cas: bool) -> Expect {
    Expect::Values {
        keys: keys.iter().map(|k| k.to_string()).collect(),
        with_cas,
    }
}

#[test]
fn a_get_reply_is_one_response_with_hits_and_misses() {
    let buf = b"VALUE a 5 12\r\nhello\r\nworld\r\nEND\r\n";
    let (reply, used) = parse_reply(buf, &get(&["a", "b"], false))
        .expect("parses")
        .expect("complete");
    assert_eq!(used, buf.len());
    let Reply::Values { items, misses } = reply else {
        panic!("expected values, got {reply:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key, "a");
    assert_eq!(items[0].flags, 5);
    // The data block is read by its declared length, so a CRLF inside it is data.
    assert_eq!(items[0].data, b"hello\r\nworld");
    assert_eq!(misses, vec!["b".to_string()]);
}

#[test]
fn a_partial_reply_consumes_nothing() {
    let full = b"VALUE a 0 5 99\r\nhello\r\nEND\r\n";
    for cut in 0..full.len() {
        assert_eq!(
            parse_reply(&full[..cut], &get(&["a"], true)),
            Ok(None),
            "cut at {cut}"
        );
    }
    let (reply, _) = parse_reply(full, &get(&["a"], true)).unwrap().unwrap();
    let Reply::Values { items, .. } = reply else {
        panic!()
    };
    assert_eq!(items[0].cas, Some(99));
}

#[test]
fn the_declared_value_size_is_refused_before_the_block_is_waited_for() {
    // Only the header has arrived. A reader that waited for 4 GB would return Ok(None) here.
    let buf = b"VALUE a 0 4000000000\r\n";
    assert!(matches!(
        parse_reply(buf, &get(&["a"], false)),
        Err(WireError::ValueTooLarge { .. })
    ));
}

#[test]
fn a_line_without_crlf_is_bounded() {
    let mut buf = vec![b'S'; MAX_REPLY_LINE + 1];
    assert_eq!(
        parse_reply(
            &buf,
            &Expect::Status {
                command: "set",
                key: None
            }
        ),
        Err(WireError::LineTooLong)
    );
    // Exactly at the bound is still waiting for its CRLF, not refused.
    buf.truncate(MAX_REPLY_LINE);
    assert_eq!(
        parse_reply(
            &buf,
            &Expect::Status {
                command: "set",
                key: None
            }
        ),
        Ok(None)
    );
}

#[test]
fn a_value_for_a_key_nobody_asked_for_is_refused() {
    let buf = b"VALUE other 0 1\r\nx\r\nEND\r\n";
    assert_eq!(
        parse_reply(buf, &get(&["a"], false)),
        Err(WireError::UnrequestedKey("other".to_string()))
    );
    // The same key twice is the same refusal.
    let buf = b"VALUE a 0 1\r\nx\r\nVALUE a 0 1\r\ny\r\nEND\r\n";
    assert!(matches!(
        parse_reply(buf, &get(&["a"], false)),
        Err(WireError::UnrequestedKey(_))
    ));
}

#[test]
fn stats_are_bounded_in_lines_and_in_bytes() {
    let mut buf = Vec::new();
    for i in 0..=MAX_STATS_ENTRIES {
        buf.extend_from_slice(format!("STAT s{i} 1\r\n").as_bytes());
    }
    buf.extend_from_slice(b"END\r\n");
    assert_eq!(
        parse_reply(&buf, &Expect::Stats { group: None }),
        Err(WireError::TooManyStats)
    );

    let (reply, _) = parse_reply(
        b"STAT pid 1\r\nSTAT version 1.6.45\r\nEND\r\n",
        &Expect::Stats { group: None },
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        reply,
        Reply::Stats {
            entries: vec![
                ("pid".to_string(), "1".to_string()),
                ("version".to_string(), "1.6.45".to_string())
            ]
        }
    );
}

#[test]
fn one_response_is_bounded_even_when_every_value_is_legal() {
    // Five 1 MiB values, each under the per-item limit, together over the response bound.
    let keys: Vec<String> = (0..5).map(|i| format!("k{i}")).collect();
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let block = vec![b'v'; 1024 * 1024];
    let mut buf = Vec::new();
    for k in &keys {
        buf.extend_from_slice(format!("VALUE {k} 0 {}\r\n", block.len()).as_bytes());
        buf.extend_from_slice(&block);
        buf.extend_from_slice(b"\r\n");
    }
    assert!(buf.len() > MAX_RESPONSE_BYTES);
    assert_eq!(
        parse_reply(&buf, &get(&key_refs, false)),
        Err(WireError::ResponseTooLarge)
    );
}

#[test]
fn an_error_line_answers_any_request() {
    let (reply, used) = parse_reply(b"SERVER_ERROR out of memory\r\n", &get(&["a"], false))
        .unwrap()
        .unwrap();
    assert_eq!(used, 28);
    assert_eq!(
        reply,
        Reply::Error {
            kind: "server_error",
            message: "out of memory".to_string()
        }
    );
}

#[test]
fn requests_encode_as_the_text_protocol_and_never_noreply() {
    let set = Request::Store {
        verb: StoreVerb::Set,
        key: "greeting".to_string(),
        flags: 42,
        exptime: 0,
        value: "two\r\nlines".to_string(),
        cas_unique: None,
    };
    assert_eq!(
        set.encode(),
        b"set greeting 42 0 10\r\ntwo\r\nlines\r\n".to_vec()
    );
    let cas = Request::Store {
        verb: StoreVerb::Cas,
        key: "k".to_string(),
        flags: 0,
        exptime: 60,
        value: "v".to_string(),
        cas_unique: Some(7),
    };
    assert_eq!(cas.encode(), b"cas k 0 60 1 7\r\nv\r\n".to_vec());
    assert_eq!(
        Request::Get {
            keys: vec!["a".into(), "b".into()],
            with_cas: true
        }
        .encode(),
        b"gets a b\r\n".to_vec()
    );
}

#[test]
fn the_model_cannot_smuggle_a_second_command_through_a_key() {
    for bad in ["a b", "a\r\nflush_all", "", &"k".repeat(251)] {
        let err = request_from_action(&json!({"type": "memcached_get", "keys": [bad]}));
        assert!(err.is_err(), "key {bad:?} must be refused");
    }
    assert!(
        request_from_action(&json!({"type": "memcached_flush_all"})).is_err(),
        "flush_all without confirm must be refused"
    );
    assert!(
        request_from_action(&json!({"type": "memcached_flush_all", "confirm": false})).is_err()
    );
    assert!(matches!(
        request_from_action(&json!({"type": "memcached_flush_all", "confirm": true})),
        Ok(Some(Request::FlushAll { delay: None }))
    ));
    assert!(
        request_from_action(
            &json!({"type": "memcached_set", "key": "k", "value": "v", "flags": 4294967296u64})
        )
        .is_err(),
        "flags past 32 bits must be refused, not wrapped"
    );
}
