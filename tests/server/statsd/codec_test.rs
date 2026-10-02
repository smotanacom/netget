use netget::server::statsd::codec::{
    encode_datagram, parse_datagram, Dialect, MAX_DATAGRAM_BYTES, MAX_RECORDS,
};
use serde_json::json;

#[test]
fn canonical_wire_vectors_decode_to_structured_records() {
    let wire = b"views:1|c|@0.5|#env:prod\nfuel:0.5|g\nlatency:240|ms\nusers:alice|s\nsize:8|h\npayload:4|d";
    let records = parse_datagram(wire, Dialect::Dogstatsd).unwrap();
    assert_eq!(
        serde_json::to_value(&records[0]).unwrap(),
        json!({"kind":"metric","name":"views","value":"1","metric_type":"c","sample_rate":0.5,"tags":["env:prod"]})
    );
    assert_eq!(records.len(), 6);
    assert_eq!(encode_datagram(&records, Dialect::Dogstatsd).unwrap(), wire);
    let gauges = parse_datagram(b"fuel:+2|g\nfuel:-3|g", Dialect::Statsd).unwrap();
    assert_eq!(serde_json::to_value(gauges).unwrap()[0]["value"], "+2");
}
#[test]
fn dogstatsd_event_byte_lengths_preserve_unicode_pipes_and_escaped_newlines() {
    let input = "_e{3,8}:雪|a|b\\n雪|d:123|h:host|k:group|p:low|s:app|t:error|#env:test\n_sc|Redis connection|2|d:123|h:host|#env:dev|m:line1\\nline2|with|pipes";
    let records = parse_datagram(input.as_bytes(), Dialect::Dogstatsd).unwrap();
    let events = serde_json::to_value(&records).unwrap();
    assert_eq!(events[0]["title"], "雪");
    assert_eq!(events[0]["text"], "a|b\n雪");
    assert_eq!(events[1]["message"], "line1\nline2|with|pipes");
    assert_eq!(
        encode_datagram(&records, Dialect::Dogstatsd).unwrap(),
        input.as_bytes()
    );
}
#[test]
fn dialects_and_malformed_messages_fail_atomically() {
    let invalid: &[&[u8]] = &[
        b"",
        b"\xff:1|c",
        b"a:1|c\n\n",
        b"a:1|c\r\n",
        b"a:NaN|g",
        b"a:inf|c",
        b"a:1|bad",
        b"a:1|c|@2",
        b"a:1|c|@-0.1",
        b"a:1|c|@NaN",
        b"a:1|c|@1|@1",
        b"a:1|g|@0.5",
        b"a:1|s|@1",
        b"a:1|c|#a,,b",
        b"a:1|c|#a|#b",
        b"a:1:2|c",
        b"a:1|c|c:123",
        b"a:1|c|T123",
        b"a:1|c|card:high",
        b"_e{1,2}:a|b",
        b"_e{9999999999999999999999999999,2}:a|bb",
        b"_e{1,1}:a|b|p:high",
        b"_e{1,1}:a|b|d:2|d:3",
        b"_sc|a|4",
        b"_sc|a|0|d:-1",
        b"_sc|a|0|unknown:x",
        b"a:1|c\nbad",
        "_e{1,1}:雪|x".as_bytes(),
    ];
    for wire in invalid {
        assert!(
            parse_datagram(wire, Dialect::Dogstatsd).is_err(),
            "accepted {:?}",
            String::from_utf8_lossy(wire)
        );
    }
    for wire in ["a:1|h", "a:1|d", "a:1|c|#tag", "_e{1,1}:a|b", "_sc|a|0"] {
        assert!(
            parse_datagram(wire.as_bytes(), Dialect::Statsd).is_err(),
            "{wire}"
        );
    }
    assert!(parse_datagram(b"a:1|c|@0", Dialect::Statsd).is_err());
    assert!(parse_datagram(b"a:1|c|@0", Dialect::Dogstatsd).is_ok());
}
#[test]
fn byte_and_record_limits_are_inclusive_and_apply_to_encoding() {
    let maximum = format!("x:{}|s", "a".repeat(MAX_DATAGRAM_BYTES - 4));
    let records = parse_datagram(maximum.as_bytes(), Dialect::Statsd).unwrap();
    assert_eq!(
        encode_datagram(&records, Dialect::Statsd).unwrap().len(),
        MAX_DATAGRAM_BYTES
    );
    assert!(parse_datagram(format!("{maximum}\n").as_bytes(), Dialect::Statsd).is_err());
    let batch = vec!["x:1|c"; MAX_RECORDS].join("\n");
    let records = parse_datagram(batch.as_bytes(), Dialect::Statsd).unwrap();
    assert_eq!(records.len(), MAX_RECORDS);
    assert!(parse_datagram(format!("{batch}\nx:1|c").as_bytes(), Dialect::Statsd).is_err());
    let mut extra = records.clone();
    extra.push(records[0].clone());
    assert!(encode_datagram(&extra, Dialect::Statsd).is_err());
    let too_large = vec![serde_json::from_value(json!({"kind":"metric","name":"x","value":"a".repeat(MAX_DATAGRAM_BYTES),"metric_type":"s"})).unwrap()];
    assert!(encode_datagram(&too_large, Dialect::Statsd).is_err());
}
#[test]
fn structured_fields_cannot_inject_new_records_or_unadvertised_options() {
    use netget::llm::actions::client_trait::Client;
    let protocol = netget::client::statsd::StatsdClientProtocol::new();
    for record in [
        json!({"kind":"metric","name":"x","value":"1\nevil:1|c","metric_type":"c"}),
        json!({"kind":"metric","name":"x","value":"1","metric_type":"c","timestamp":1}),
        json!({"kind":"event","title":"a","text":"b","hostname":"host|t:error"}),
    ] {
        assert!(protocol
            .execute_action(json!({"type":"send_statsd_batch","records":[record]}))
            .is_err());
    }
}

#[test]
fn model_startup_example_explicitly_opts_into_fallback() {
    use netget::llm::actions::{common::CommonAction, protocol_trait::Protocol};
    let examples = netget::server::statsd::actions::StatsdProtocol::new().get_startup_examples();
    let parsed = CommonAction::from_json(&examples.llm_mode).unwrap();
    match parsed {
        CommonAction::OpenServer {
            startup_params: Some(params),
            ..
        } => assert_eq!(params["llm_fallback"], true),
        _ => panic!("LLM startup example lost fallback parameter"),
    }
}
