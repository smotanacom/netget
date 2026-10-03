use netget::server::loki::codec::{self, Encoding, Entry, PushBatch, Stream};
use serde_json::json;
use std::collections::BTreeMap;
pub(crate) fn batch(encoding: &str) -> PushBatch {
    PushBatch {
        tenant_id: Some("tenant-one".into()),
        encoding: match encoding {
            "json" => Encoding::Json,
            "gzip_json" => Encoding::GzipJson,
            _ => Encoding::SnappyProtobuf,
        },
        streams: vec![Stream {
            labels: BTreeMap::from([
                ("app".into(), "sample 名".into()),
                ("host".into(), "say \"hi\" \\ \n\t".into()),
            ]),
            entries: vec![
                Entry {
                    timestamp_ns: 1700000000000000123,
                    line: "entry 名 \"quote\" \n\t".into(),
                    structured_metadata: BTreeMap::from([
                        ("trace_id".into(), "0123名".into()),
                        ("user_id".into(), "two".into()),
                    ]),
                },
                Entry {
                    timestamp_ns: 1700000000000000124,
                    line: "second line".into(),
                    structured_metadata: BTreeMap::new(),
                },
            ],
        }],
    }
}
#[test]
fn all_carriers_preserve_timestamps_unicode_labels_metadata_and_negative_time() {
    for kind in ["json", "gzip_json", "snappy_protobuf"] {
        let mut b = batch(kind);
        b.streams[0]
            .entries
            .extend([i64::MIN, -1, 0, i64::MAX].map(|timestamp_ns| Entry {
                timestamp_ns,
                line: String::new(),
                structured_metadata: BTreeMap::new(),
            }));
        let wire = codec::encode_batch(&b).unwrap();
        assert_eq!(codec::decode_batch(&wire, b.encoding).unwrap(), b.streams);
    }
}
#[test]
fn strict_json_keys_timestamp_type_shape_depth_and_duplicate_streams() {
    let good =
        json!({"streams":[{"stream":{"app":"x"},"values":[["123","line",{"trace":"one"}]]}]})
            .to_string();
    assert!(codec::decode_batch(good.as_bytes(), Encoding::Json).is_ok());
    for bad in [
        good.replace("\"123\"", "123"),
        good.replace("\"one\"", "42"),
        good.replace("\"one\"", "{}"),
        good.replace("\"123\"", "\"9223372036854775808\""),
        good.replace("\"app\":\"x\"", "\"app\":\"x\",\"app\":\"y\""),
        good.replace("\"trace\":\"one\"", "\"trace\":\"one\",\"trace\":\"two\""),
        good.replace("\"123\"", "\"+123\""),
        good.replace("\"values\"", "\"unknown\""),
        format!("{}0{}", "[".repeat(9), "]".repeat(9)),
    ] {
        assert!(
            codec::decode_batch(bad.as_bytes(), Encoding::Json).is_err(),
            "{bad}"
        );
    }
    let mut b = batch("json");
    b.streams.push(b.streams[0].clone());
    assert!(codec::encode_batch(&b).is_err());
}
#[test]
fn snappy_literal_and_all_copy_forms_validate_overlap_lengths_and_offsets() {
    // Independent wire facts: prefix9,literal'ab',copy offset2 length7.
    for copy in [vec![13, 2], vec![26, 2, 0], vec![27, 2, 0, 0, 0]] {
        let mut wire = vec![9, 4, b'a', b'b'];
        wire.extend(copy);
        assert_eq!(codec::snappy_decode(&wire).unwrap(), b"ababababa");
    }
    for n in [0, 1, 60, 61, 256, 65536, codec::MAX_BODY_BYTES] {
        let raw = vec![b'x'; n];
        assert_eq!(
            codec::snappy_decode(&codec::snappy_encode(&raw)).unwrap(),
            raw
        );
    }
    for bad in [
        vec![1, 1, 0],
        vec![1, 1, 5],
        vec![0, 0, b'a'],
        vec![1],
        vec![1, 0],
        vec![0x80; 10],
        vec![0x80, 0x80, 0x80, 0x80, 0x80, 0],
        codec::snappy_encode(&vec![0; codec::MAX_BODY_BYTES + 1]),
    ] {
        assert!(codec::snappy_decode(&bad).is_err(), "{bad:?}");
    }
}
#[test]
fn typed_name_value_tenant_entry_label_and_metadata_bounds() {
    let mut b = batch("json");
    b.streams[0].entries[0].line = "x".repeat(codec::MAX_LINE_BYTES);
    assert!(codec::encode_batch(&b).is_ok());
    b.streams[0].entries[0].line.push('x');
    assert!(codec::encode_batch(&b).is_err());
    for tenant in ["a:b", "a|b", ".", "..", "", &"x".repeat(151)] {
        assert!(codec::validate_tenant(tenant).is_err());
    }
    assert!(codec::validate_tenant(&"x".repeat(150)).is_ok());
    for token in ["", "x y", "x\n", &"x".repeat(1025)] {
        assert!(codec::validate_token(token).is_err());
    }
    assert!(codec::validate_token(&"x".repeat(1024)).is_ok());
    for count in [codec::MAX_LABELS, codec::MAX_LABELS + 1] {
        let mut b = batch("json");
        b.streams[0].labels = (0..count)
            .map(|i| (format!("k{i}"), String::new()))
            .collect();
        assert_eq!(codec::encode_batch(&b).is_ok(), count == codec::MAX_LABELS);
    }
    for count in [codec::MAX_METADATA, codec::MAX_METADATA + 1] {
        let mut b = batch("json");
        b.streams[0].entries[0].structured_metadata = (0..count)
            .map(|i| (format!("k{i}"), String::new()))
            .collect();
        assert_eq!(
            codec::encode_batch(&b).is_ok(),
            count == codec::MAX_METADATA
        );
    }
    for count in [codec::MAX_STREAMS, codec::MAX_STREAMS + 1] {
        let mut b = batch("json");
        let template = b.streams[0].clone();
        b.streams = (0..count)
            .map(|i| {
                let mut s = template.clone();
                s.labels.insert("id".into(), i.to_string());
                s
            })
            .collect();
        assert_eq!(codec::encode_batch(&b).is_ok(), count == codec::MAX_STREAMS);
    }
    for count in [codec::MAX_ENTRIES, codec::MAX_ENTRIES + 1] {
        let mut b = batch("json");
        let mut e = b.streams[0].entries[1].clone();
        e.line.clear();
        b.streams[0].entries = vec![e; count];
        assert_eq!(codec::encode_batch(&b).is_ok(), count == codec::MAX_ENTRIES);
    }
    for bad in ["1key", "__reserved", "a-b", &"k".repeat(129)] {
        let mut b = batch("json");
        b.streams[0].labels = BTreeMap::from([(bad.into(), "x".into())]);
        assert!(codec::encode_batch(&b).is_err());
    }
    let mut b = batch("json");
    b.streams[0].labels = BTreeMap::from([("k".repeat(128), "x".repeat(2048))]);
    assert!(codec::encode_batch(&b).is_ok());
    b.streams[0].labels.values_mut().next().unwrap().push('x');
    assert!(codec::encode_batch(&b).is_err());
}
#[test]
fn compressed_wire_body_and_gzip_trailing_truncation_limits() {
    let b = batch("gzip_json");
    let wire = codec::encode_batch(&b).unwrap();
    for bad in [
        wire[..wire.len() - 1].to_vec(),
        [wire.clone(), b"junk".to_vec()].concat(),
        vec![b'x'; codec::MAX_BODY_BYTES + 1],
    ] {
        assert!(codec::decode_batch(&bad, Encoding::GzipJson).is_err());
    }
    use std::io::Write;
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&vec![b'x'; codec::MAX_BODY_BYTES + 1]).unwrap();
    assert!(
        codec::decode_batch(&z.finish().unwrap(), Encoding::GzipJson)
            .unwrap_err()
            .to_string()
            .contains("limit")
    );
    let mut b = batch("json");
    b.streams[0].entries = vec![b.streams[0].entries[0].clone(); 20];
    for e in &mut b.streams[0].entries {
        e.line = "x".repeat(16 * 1024);
    }
    assert!(codec::encode_batch(&b).is_err());
}
#[test]
fn malformed_protobuf_and_go_label_escaping_are_rejected_without_panics() {
    for raw in [
        vec![0],
        vec![11],
        vec![10, 127],
        vec![10, 1, 0],
        vec![0xff; 10],
        vec![18, 4, b'o', b't', b'l', b'p'],
    ] {
        assert!(
            codec::decode_batch(&codec::snappy_encode(&raw), Encoding::SnappyProtobuf).is_err()
        );
    }
    for bytes in [
        vec![10, 2, b'{', b'}'],
        vec![
            10, 11, b'{', b'a', b'=', b'"', b'x', b'"', b',', b'a', b'=', b'"', b'x',
        ],
    ] {
        let mut wire = vec![10, bytes.len() as u8];
        wire.extend(bytes);
        assert!(
            codec::decode_batch(&codec::snappy_encode(&wire), Encoding::SnappyProtobuf).is_err()
        );
    }
}
#[test]
fn exact_body_boundary_and_protobuf_field_budget_are_atomic() {
    let mut b = batch("json");
    b.streams[0].labels = BTreeMap::from([("app".into(), "x".into())]);
    let e = Entry {
        timestamp_ns: 0,
        line: String::new(),
        structured_metadata: BTreeMap::new(),
    };
    b.streams[0].entries = vec![e.clone(); 16];
    let mut remaining = codec::MAX_BODY_BYTES - codec::encode_batch(&b).unwrap().len();
    for e in &mut b.streams[0].entries {
        let n = remaining.min(codec::MAX_LINE_BYTES);
        e.line = "x".repeat(n);
        remaining -= n;
    }
    assert_eq!(remaining, 0);
    let exact = codec::encode_batch(&b).unwrap();
    assert_eq!(exact.len(), codec::MAX_BODY_BYTES);
    assert!(codec::decode_batch(&exact, Encoding::Json).is_ok());
    b.streams[0].entries.last_mut().unwrap().line.push('x');
    assert!(codec::encode_batch(&b).is_err());
    let mut wire = exact;
    wire.push(b' ');
    assert!(codec::decode_batch(&wire, Encoding::Json).is_err());
    // Unknown protobuf scalar fields are bounded even though forward-compatible parsing skips them.
    let mut fields = vec![0x28, 0];
    fields = fields.repeat(codec::MAX_PROTO_FIELDS + 1);
    assert!(
        codec::decode_batch(&codec::snappy_encode(&fields), Encoding::SnappyProtobuf)
            .unwrap_err()
            .to_string()
            .contains("field count limit")
    );
    let mut b = batch("snappy_protobuf");
    let mut e = e;
    e.structured_metadata = (0..8).map(|i| (format!("k{i}"), String::new())).collect();
    b.streams[0].entries = vec![e; codec::MAX_ENTRIES];
    assert!(codec::encode_batch(&b)
        .unwrap_err()
        .to_string()
        .contains("field count limit"));
}
