//! `netget::server::nostr::wire::parse_client_message` — every text frame a Nostr client sends,
//! pre-authentication (NIP-01 has none).
//!
//! The parse is `serde_json` (whose recursion limit is the depth guard: a depth bomb must come
//! back as a refusal, not a stack overflow), then NetGet's own schema checks and, for an
//! `EVENT`, the id recomputation and the BIP-340 signature check. Asserted:
//!
//! 1. It never panics, and it is deterministic.
//! 2. A refusal is one well-formed relay message (`NOTICE`, `OK` or `CLOSED`) with a
//!    `decision=fail_closed_*` token.
//! 3. An accepted `EVENT` verifies again from its own JSON rendering — so the event the model
//!    is shown, and the one delivered to subscribers, is the one whose signature was checked.
//! 4. An accepted `REQ` is inside the declared bounds (filter count, subscription id length),
//!    and matching its filters against events — including one signed here — never panics.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::nostr::wire::{
    parse_client_message, select_events, verify_event, ClientMessage, RelayKey, MAX_FILTERS,
    MAX_SUBSCRIPTION_ID_CHARS,
};
use std::sync::LazyLock;

static KEY: LazyLock<RelayKey> = LazyLock::new(|| {
    RelayKey::from_hex("0f1e2d3c4b5a69788796a5b4c3d2e1f00112233445566778899aabbccddeeff0")
        .expect("a valid fixed key")
});

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        // WebSocket text frames are UTF-8 by the time they reach the parser; tungstenite
        // refuses anything else.
        return;
    };
    let first = parse_client_message(text);
    let second = parse_client_message(text);
    assert_eq!(
        format!("{first:?}"),
        format!("{second:?}"),
        "parse_client_message disagreed with itself"
    );

    match first {
        Err(refusal) => {
            assert!(refusal.decision.starts_with("fail_closed_"), "{refusal:?}");
            let reply: serde_json::Value =
                serde_json::from_str(&refusal.reply).expect("a refusal is JSON");
            let verb = reply[0]
                .as_str()
                .expect("a relay message starts with its verb");
            assert!(matches!(verb, "NOTICE" | "OK" | "CLOSED"), "{reply}");
            if verb == "OK" {
                assert_eq!(reply[2], false, "a refusal never accepts: {reply}");
            }
        }
        Ok(ClientMessage::Event(event)) => {
            let again = verify_event(&event.to_json()).expect("an accepted event re-verifies");
            assert_eq!(again, event);
        }
        Ok(ClientMessage::Req {
            subscription_id,
            filters,
        }) => {
            assert!(!filters.is_empty() && filters.len() <= MAX_FILTERS);
            assert!(
                !subscription_id.is_empty()
                    && subscription_id.chars().count() <= MAX_SUBSCRIPTION_ID_CHARS
            );
            let events = vec![
                KEY.sign(
                    1_700_000_000,
                    1,
                    vec![vec!["t".into(), "x".into()]],
                    "a".into(),
                ),
                KEY.sign(0, 65_535, Vec::new(), String::new()),
            ];
            for limit in [true, false] {
                let chosen = select_events(&filters, &events, limit);
                assert!(chosen.windows(2).all(|w| w[0] < w[1]) && chosen.len() <= events.len());
            }
        }
        Ok(ClientMessage::Close { subscription_id }) => {
            assert!(subscription_id.chars().count() <= MAX_SUBSCRIPTION_ID_CHARS);
        }
    }
});
