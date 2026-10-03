use netget::server::prometheus_remote_write::codec::{
    self, Sample, SampleValue, SpecialValue, WriteBatch,
};
use serde_json::json;
fn batch() -> WriteBatch {
    serde_json::from_value(crate::helpers::prometheus_remote_write::batch()).unwrap()
}
#[test]
fn literal_wire_decodes_and_encodes_both_directions() {
    let wire = include_str!("v1_float.hex")
        .split_whitespace()
        .map(|s| u8::from_str_radix(s, 16).unwrap())
        .collect::<Vec<_>>();
    let expected: WriteBatch = serde_json::from_value(
        json!({"series":[{"labels":{"__name__":"x"},"samples":[{"timestamp_ms":-1,"value":1.5}]}]}),
    )
    .unwrap();
    assert_eq!(codec::decode_batch(&wire).unwrap().series, expected.series);
    assert_eq!(codec::encode_batch(&expected).unwrap(), wire);
}
#[test]
fn signed_time_special_floats_stale_and_negative_zero_are_typed() {
    let mut b = batch();
    b.series[0].samples = [i64::MIN, -1, 0, 1, 2, 3, i64::MAX]
        .into_iter()
        .zip([
            SampleValue::Number(-0.0),
            SampleValue::Special(SpecialValue::NegativeInfinity),
            SampleValue::Special(SpecialValue::PositiveInfinity),
            SampleValue::Special(SpecialValue::Nan),
            SampleValue::Special(SpecialValue::Stale),
            SampleValue::Number(1.5),
            SampleValue::Number(f64::MAX),
        ])
        .map(|(timestamp_ms, value)| Sample {
            timestamp_ms,
            value,
        })
        .collect();
    let wire = codec::encode_batch(&b).unwrap();
    let decoded = codec::decode_batch(&wire).unwrap();
    assert_eq!(decoded.series, b.series);
    assert_eq!(
        decoded.series[0].samples[0].value.bits().unwrap(),
        0x8000000000000000
    );
    assert_eq!(
        decoded.series[0].samples[4].value.bits().unwrap(),
        0x7ff0000000000002
    );
    let model = serde_json::to_value(decoded.series).unwrap();
    assert_eq!(model[0]["samples"][3]["value"], "nan");
    assert_eq!(model[0]["samples"][4]["value"], "stale");
    assert_eq!(
        SampleValue::from_bits(0x7ff0000000000001),
        SampleValue::Special(SpecialValue::Nan)
    );
}
#[test]
fn scalar_label_sort_duplicate_shape_order_and_extension_validation() {
    let mut b = batch();
    b.series[0].samples.reverse();
    assert!(codec::encode_batch(&b).is_err());
    for name in ["", "1bad", "a-b", "名"] {
        let mut b = batch();
        b.series[0].labels.insert(name.into(), "value".into());
        assert!(codec::encode_batch(&b).is_err());
    }
    for name in ["valid:metric", "_metric", "metric0"] {
        let mut b = batch();
        b.series[0].labels.insert("__name__".into(), name.into());
        assert!(codec::encode_batch(&b).is_ok());
    }
    let mut b = batch();
    b.series.push(b.series[0].clone());
    assert!(codec::encode_batch(&b).is_err());
    let mut b = batch();
    b.series[0].labels.insert("site".into(), String::new());
    assert!(codec::encode_batch(&b).is_err());
    let mut b = batch();
    b.series[0].samples[0].value = SampleValue::Number(f64::NAN);
    assert!(codec::encode_batch(&b).is_err());
    for wire in [vec![10, 4, 26, 2, 0, 0], vec![10, 4, 34, 2, 0, 0]] {
        assert!(codec::decode_proto(&wire).is_err());
    }
    // Labels b then a are not sorted; duplicate singular fields fail closed.
    for wire in [
        vec![
            10, 20, 10, 6, 10, 1, b'b', 18, 1, b'x', 10, 6, 10, 1, b'a', 18, 1, b'y', 18, 2, 16, 0,
        ],
        vec![10, 6, 10, 4, 10, 0, 10, 0],
    ] {
        assert!(codec::decode_proto(&wire).is_err());
    }
    assert!(serde_json::from_value::<WriteBatch>(json!({"series":[],"raw":"xx"})).is_err());
    assert_eq!(codec::decode_batch(&[0]).unwrap().series.len(), 0);
}
#[test]
fn exact_series_sample_label_name_value_token_path_and_protobuf_field_bounds() {
    for n in [codec::MAX_SERIES, codec::MAX_SERIES + 1] {
        let mut b = batch();
        let mut ts = b.series.remove(0);
        ts.samples.truncate(1);
        b.series = (0..n)
            .map(|i| {
                let mut s = ts.clone();
                s.labels.insert("id".into(), i.to_string());
                s
            })
            .collect();
        assert_eq!(codec::encode_batch(&b).is_ok(), n == codec::MAX_SERIES);
    }
    for n in [codec::MAX_SAMPLES, codec::MAX_SAMPLES + 1] {
        let mut b = batch();
        b.series[0].samples = vec![b.series[0].samples[0].clone(); n];
        assert_eq!(codec::encode_batch(&b).is_ok(), n == codec::MAX_SAMPLES);
    }
    for n in [codec::MAX_LABELS, codec::MAX_LABELS + 1] {
        let mut b = batch();
        b.series[0].labels = (0..n).map(|i| (format!("k{i}"), "v".into())).collect();
        assert_eq!(codec::encode_batch(&b).is_ok(), n == codec::MAX_LABELS);
    }
    for n in [codec::MAX_NAME_BYTES, codec::MAX_NAME_BYTES + 1] {
        let mut b = batch();
        b.series[0].labels.insert("k".repeat(n), "v".into());
        assert_eq!(codec::encode_batch(&b).is_ok(), n == codec::MAX_NAME_BYTES);
    }
    for n in [codec::MAX_VALUE_BYTES, codec::MAX_VALUE_BYTES + 1] {
        let mut b = batch();
        b.series[0].labels.insert("site".into(), "v".repeat(n));
        assert_eq!(codec::encode_batch(&b).is_ok(), n == codec::MAX_VALUE_BYTES);
    }
    for n in [1024, 1025] {
        assert_eq!(codec::validate_token(&"a".repeat(n)).is_ok(), n == 1024);
        assert_eq!(
            codec::validate_path(&format!("/{}", "a".repeat(n - 1))).is_ok(),
            n == 1024
        );
    }
    for p in ["x", "//example/x", "/x?q=y", "/x#z", "/x y", "/x\\y"] {
        assert!(codec::validate_path(p).is_err(), "{p}");
    }
    for n in [codec::MAX_PROTO_FIELDS, codec::MAX_PROTO_FIELDS + 1] {
        let raw = [32, 0].repeat(n);
        let result = codec::decode_proto(&raw);
        assert_eq!(result.is_ok(), n == codec::MAX_PROTO_FIELDS);
        if let Ok(result) = result {
            assert_eq!(result.ignored_fields, n);
        }
    }
}
#[test]
fn protobuf_truncations_wire_types_and_snappy_all_copy_forms_and_bombs() {
    let wire = codec::encode_batch(&batch()).unwrap();
    for n in 0..wire.len() {
        assert!(codec::decode_batch(&wire[..n]).is_err(), "{n}");
    }
    for raw in [
        vec![0],
        vec![15],
        vec![10, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
        vec![8, 0],
        vec![0x80; 10],
    ] {
        assert!(codec::decode_proto(&raw).is_err());
    }
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
    for raw in [
        vec![1, 1, 0],
        vec![1, 1, 5],
        vec![0, 0, b'a'],
        vec![1],
        vec![1, 0],
        vec![0x80; 10],
        vec![0x80, 0x80, 0x80, 0x80, 0x80, 0],
        codec::snappy_encode(&vec![0; codec::MAX_BODY_BYTES + 1]),
    ] {
        assert!(codec::snappy_decode(&raw).is_err());
    }
    assert!(codec::decode_batch(&vec![0; codec::MAX_BODY_BYTES + 1]).is_err());
}

#[test]
fn combined_valid_scalar_limits_cannot_exceed_body_budget_and_unknown_fields_are_counted() {
    let mut b = batch();
    b.series = (0..128)
        .map(|i| {
            let mut s = b.series[0].clone();
            s.labels.insert("id".into(), i.to_string());
            s.labels
                .insert("site".into(), "x".repeat(codec::MAX_VALUE_BYTES));
            s
        })
        .collect();
    assert!(codec::encode_batch(&b).is_err());
    // A top-level reserved metadata/source field is ignored without byte exposure.
    let decoded = codec::decode_proto(&[18, 2, 0xff, 0xfe, 24, 1]).unwrap();
    assert!(decoded.series.is_empty());
    assert_eq!(decoded.ignored_fields, 2);
}
