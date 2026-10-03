use super::common::{signed, SECRET};
use netget::{
    client::nostr::{actions::NostrClientProtocol, api},
    llm::actions::{client_trait::Client, protocol_trait::Protocol},
    server::nostr::wire::RelayKey,
};
use serde_json::{json, Value};
#[test]
fn signed_events_preserve_unicode_controls_and_verify_native_id_and_signature() {
    let key = RelayKey::from_hex(SECRET).unwrap();
    let action = json!({"type":"nostr_publish","kind":65535,"created_at":1700000000,"content":"film ☃ \u{8}\u{1}\r\n","tags":[["t","film"],["p","anything","marker"]]});
    let api::Action::Publish {
        kind,
        created_at,
        content,
        tags,
    } = api::action(&action).unwrap()
    else {
        panic!()
    };
    let event = key.sign(created_at, kind, tags, content);
    let text = api::frame_text(json!(["EVENT", "sub", event.to_json()])).unwrap();
    let api::RelayMessage::Event {
        event: received, ..
    } = api::relay(&text).unwrap()
    else {
        panic!()
    };
    assert_eq!(received, event);
    let mut bad = event.to_json();
    bad["content"] = json!("forged");
    assert!(api::relay(&json!(["EVENT", "sub", bad]).to_string()).is_err());
    let mut bad = event.to_json();
    bad["sig"] = json!("0".repeat(128));
    assert!(api::relay(&json!(["EVENT", "sub", bad]).to_string()).is_err());
    assert!(!format!("{key:?}").contains(SECRET));
}
#[test]
fn selected_filters_have_native_and_or_and_live_limit_zero_semantics() {
    let event = netget::server::nostr::wire::verify_event(&signed("film")).unwrap();
    let filter=api::filter(&json!({"authors":[event.pubkey],"kinds":[1],"#t":["film"],"since":event.created_at,"until":event.created_at,"limit":0})).unwrap();
    assert!(filter.matches(&event), "limit controls initial query only");
    assert!(!api::filter(&json!({"kinds":[7],"#t":["film"]}))
        .unwrap()
        .matches(&event));
    for value in [
        json!({"ids":[]}),
        json!({"authors":["abcd"]}),
        json!({"#e":["abcd"]}),
        json!({"#p":["A".repeat(64)]}),
        json!({"#t":[]}),
        json!({"kinds":[65536]}),
        json!({"limit":501}),
        json!({"search":"film"}),
        json!({"since":-1}),
        json!({"until":1.5}),
        json!({"#ab":["film"]}),
    ] {
        assert!(api::filter(&value).is_err(), "{value}");
    }
    let api::Action::Subscribe{id,filters}=api::action(&json!({"type":"nostr_subscribe","subscription_id":"☃".repeat(64),"filters":[{"kinds":[1]},{"kinds":[7]}]})).unwrap() else {panic!()};
    assert_eq!(id.chars().count(), 64);
    assert_eq!(filters.len(), 2);
}
#[test]
fn native_results_require_typed_correlation_and_keep_unknown_reason_prefixes() {
    let id = "a".repeat(64);
    let api::RelayMessage::Ok {
        accepted, message, ..
    } = api::relay(&json!(["OK", id, false, "auth-required: credentials required"]).to_string())
        .unwrap()
    else {
        panic!()
    };
    assert!(!accepted);
    assert_eq!(api::reason_prefix(&message), Some("auth-required"));
    for frame in [
        json!(["OK", "a", true, ""]),
        json!(["OK", id, "true", ""]),
        json!(["OK", id, false, "unprefixed"]),
        json!(["OK", id, true]),
        json!(["CLOSED", "sub", "unprefixed"]),
        json!(["EOSE", "sub", "extra"]),
        json!(["NOTICE", false]),
        json!(["EVENT", "sub", {}]),
    ] {
        assert!(api::relay(&frame.to_string()).is_err(), "{frame}");
    }
    assert!(matches!(
        api::relay("[\"AUTH\",\"private-challenge\"]").unwrap(),
        api::RelayMessage::Unsupported("AUTH")
    ));
    assert!(matches!(
        api::relay("[\"COUNT\",\"sub\",{\"count\":2}]").unwrap(),
        api::RelayMessage::Unsupported("COUNT")
    ));
}
#[test]
fn nip11_keeps_absent_capabilities_and_distinguishes_relay_and_contact_keys() {
    assert_eq!(api::relay_info(&json!({"unknown":42})).unwrap(), json!({}));
    let contact = "a".repeat(64);
    let relay = "b".repeat(64);
    let value=api::relay_info(&json!({"pubkey":contact,"self":relay,"supported_nips":[1,11,42,9999],"limitation":{"auth_required":false,"max_subscriptions":0,"unknown":2},"unknown":true})).unwrap();
    assert_eq!(value["pubkey"], contact);
    assert_eq!(value["self"], relay);
    assert_eq!(value["supported_nips"], json!([1, 11, 42, 9999]));
    assert_eq!(
        value["limitation"],
        json!({"auth_required":false,"max_subscriptions":0})
    );
    for input in [
        json!([]),
        json!({"supported_nips":["1"]}),
        json!({"pubkey":"bad"}),
        json!({"name":1}),
        json!({"limitation":{"auth_required":"false"}}),
    ] {
        assert!(api::relay_info(&input).is_err());
    }
}
#[test]
fn private_keys_raw_frames_and_unbounded_constructed_values_refuse_before_copying() {
    let protocol = NostrClientProtocol::new();
    for action in [
        json!({"type":"nostr_publish","kind":1,"content":"film","secret_key":SECRET}),
        json!({"type":"nostr_publish","kind":1,"content":"film","sig":"a"}),
        json!({"type":"send_data","data":"AUTH"}),
        json!({"type":"nostr_auth","challenge":"x"}),
        json!({"type":"nostr_count","filters":[{}]}),
        json!({"type":"nostr_subscribe","subscription_id":"","filters":[{}]}),
        json!({"type":"nostr_subscribe","subscription_id":"s","filters":vec![json!({});11]}),
        json!({"type":"nostr_publish","kind":1,"content":"x","tags":[[]]}),
        json!({"type":"nostr_publish","kind":1,"content":"x".repeat(65537)}),
    ] {
        let error = protocol.execute_action(action).err().unwrap().to_string();
        assert!(!error.contains(SECRET));
    }
    let mut value = Value::String(SECRET.into());
    for _ in 0..10000 {
        value = Value::Array(vec![value]);
    }
    let error = protocol.execute_action(value).err().unwrap().to_string();
    assert!(error.contains("depth/node/retained-content"));
    assert!(!error.contains(SECRET));
    let text = format!("{}0{}", "[".repeat(10000), "]".repeat(10000));
    assert!(api::json(text.as_bytes()).is_err());
    assert!(api::json(&vec![b' '; 131073]).is_err());
    let state = super::common::state();
    assert!(protocol
        .get_async_actions(&state)
        .iter()
        .flat_map(|a| &a.parameters)
        .all(|p| !p.name.contains("key") && !p.name.contains("sig")));
}
#[test]
fn selected_collection_text_and_json_budgets_have_direct_boundary_evidence() {
    let key = RelayKey::from_hex(SECRET).unwrap();
    for length in [api::MAX_TEXT, api::MAX_TEXT + 1] {
        let value = json!({"type":"nostr_publish","kind":1,"content":"x".repeat(length)});
        assert_eq!(api::action(&value).is_ok(), length == api::MAX_TEXT);
    }
    for count in [2000, 2001] {
        assert_eq!(api::action(&json!({"type":"nostr_publish","kind":1,"content":"","tags":vec![json!(["t"]);count]})).is_ok(),count==2000);
    }
    for count in [32, 33] {
        assert_eq!(
            api::action(
                &json!({"type":"nostr_publish","kind":1,"content":"","tags":[vec!["t";count]]})
            )
            .is_ok(),
            count == 32
        );
    }
    for count in [500, 501] {
        assert_eq!(
            api::filter(&json!({"kinds":vec![json!(1);count]})).is_ok(),
            count == 500
        );
    }
    for count in [10, 11] {
        assert_eq!(api::action(&json!({"type":"nostr_subscribe","subscription_id":"s","filters":vec![json!({});count]})).is_ok(),count==10);
    }
    for count in [64, 65] {
        assert_eq!(api::subid(&json!("☃".repeat(count))).is_ok(), count == 64);
    }
    let large = key.sign(1700000000, 1, vec![], "\u{1}".repeat(api::MAX_TEXT));
    assert!(
        api::frame_text(json!(["EVENT", large.to_json()])).is_err(),
        "UTF8 content cap does not bypass the encoded message cap"
    );
    let empty_tag = key.sign(1700000000, 1, vec![vec![]], "invalid tag".into());
    assert!(api::relay(&json!(["EVENT", "sub", empty_tag.to_json()]).to_string()).is_err());
    for depth in [api::MAX_DEPTH, api::MAX_DEPTH + 1] {
        let mut nested = json!(true);
        for _ in 0..depth {
            nested = Value::Array(vec![nested]);
        }
        assert_eq!(
            api::json(serde_json::to_string(&nested).unwrap().as_bytes()).is_ok(),
            depth == api::MAX_DEPTH
        );
    }
    for nodes in [api::MAX_NODES, api::MAX_NODES + 1] {
        let value = Value::Array(vec![Value::Null; nodes - 1]);
        assert_eq!(
            api::json(serde_json::to_string(&value).unwrap().as_bytes()).is_ok(),
            nodes == api::MAX_NODES
        );
    }
    for extra in [0, 1] {
        let value = Value::String(
            "x".repeat(api::MAX_RETAINED_BYTES - std::mem::size_of::<Value>() + extra),
        );
        assert_eq!(api::within_budget(&value), extra == 0);
        let error = NostrClientProtocol::new()
            .execute_action(value)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error.contains("depth/node/retained-content"), extra != 0);
    }
}
