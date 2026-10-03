use netget::server::influxdb::codec::*;
use serde_json::json;
pub(super) fn batch(precision: &str, gzip: bool) -> WriteBatch {
    serde_json::from_value(json!({"org":"org 名 &","bucket":"bucket / &","precision":precision,"gzip":gzip,"points":[{"measurement":"温 度,","tags":{"host =,":"a,b =名"},"fields":{"float":{"type":"float","value":1.25},"int":{"type":"integer","value":-42},"uint":{"type":"unsigned","value":18446744073709551615u64},"bool":{"type":"boolean","value":true},"str =,":{"type":"string","value":"say \"hi\" \\ literal\\n\t名"}},"timestamp":123}]})).unwrap()
}
#[test]
fn five_types_escaping_and_four_precisions_round_trip() {
    for p in ["ns", "us", "ms", "s"] {
        for gzip in [false, true] {
            let b = batch(p, gzip);
            let bytes = encode_batch(&b).unwrap();
            let body = decode_body(&bytes, if gzip { "gzip" } else { "identity" }).unwrap();
            let parsed = parse_batch(&body, b.precision, 456).unwrap();
            assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
            assert_eq!(parsed.points.len(), 1);
            assert_eq!(parsed.points[0].point, b.points[0]);
            assert_eq!(
                parsed.points[0].timestamp_ns,
                b.precision.to_nanoseconds(123).unwrap()
            );
        }
    }
}
#[test]
fn comments_crlf_boolean_variants_and_partial_errors_keep_source_lines() {
    let b=parse_batch(b"# comment\r\n\r\nm f=TRUE,i=-9223372036854775808i,u=18446744073709551615u\r\nbad\r\nm f=False 1\r\n",Precision::Seconds,123).unwrap();
    assert_eq!(b.points.len(), 2);
    assert_eq!(b.points[0].line, 3);
    assert_eq!(b.points[0].timestamp_ns, 123);
    assert_eq!(b.points[1].line, 5);
    assert_eq!(b.points[1].timestamp_ns, 1_000_000_000);
    assert_eq!(b.errors[0].line, 4);
}
#[test]
fn malformed_ranges_reserved_and_duplicate_keys_are_rejected() {
    for body in [
        "m f=NaN",
        "m f=1i,f=2i",
        "m,t=a,t=b f=1i",
        "m,t=a=b f=1i",
        "m f=18446744073709551616u",
        "m f=9223372036854775808i",
        "m f=1i 9223372036854775807",
        "m f=1i +1",
        "m f=\"bad\\nline\"",
        "m time=1i",
        "m,_reserved=x f=1i",
        "m f=\"unterminated",
    ] {
        let b = parse_batch(body.as_bytes(), Precision::Nanoseconds, 1).unwrap();
        assert!(b.points.is_empty(), "{body}");
        assert_eq!(b.errors.len(), 1, "{body}");
    }
    assert!(parse_batch(b"m f=1i\xff", Precision::Nanoseconds, 1).is_err());
    assert!(Precision::Seconds.to_nanoseconds(i64::MAX).is_err());
    assert!(Precision::Nanoseconds.to_nanoseconds(i64::MIN).is_err());
    assert!(Precision::Nanoseconds
        .to_nanoseconds(MAX_TIMESTAMP_NS)
        .is_ok());
}
#[test]
fn body_point_line_tag_field_and_name_limits_are_enforced() {
    let point: Point = serde_json::from_value(
        json!({"measurement":"m","fields":{"s":{"type":"string","value":"x".repeat(1017)}}}),
    )
    .unwrap();
    let mut exact = WriteBatch {
        org: "o".into(),
        bucket: "b".into(),
        precision: Precision::Nanoseconds,
        points: vec![point; MAX_POINTS],
        gzip: false,
    };
    assert_eq!(encode_batch(&exact).unwrap().len(), MAX_BODY_BYTES);
    exact
        .points
        .last_mut()
        .unwrap()
        .fields
        .insert("s".into(), FieldValue::String("x".repeat(1018)));
    assert!(encode_batch(&exact).is_err());
    assert!(parse_batch(&vec![b'x'; MAX_BODY_BYTES + 1], Precision::Nanoseconds, 1).is_err());
    assert!(parse_batch(
        "m f=1i\n".repeat(MAX_POINTS + 1).as_bytes(),
        Precision::Nanoseconds,
        1
    )
    .is_err());
    assert!(parse_batch(
        "#\n".repeat(MAX_LINES + 1).as_bytes(),
        Precision::Nanoseconds,
        1
    )
    .is_err());
    let mut b = batch("ns", false);
    b.points = vec![b.points[0].clone(); MAX_POINTS + 1];
    assert!(encode_batch(&b).is_err());
    let mut b = batch("ns", false);
    b.points[0].measurement = "m".repeat(MAX_NAME_BYTES + 1);
    assert!(encode_batch(&b).is_err());
    let mut b = batch("ns", false);
    b.points[0].tags = (0..=MAX_TAGS)
        .map(|i| (format!("t{i}"), "x".into()))
        .collect();
    assert!(encode_batch(&b).is_err());
    let mut b = batch("ns", false);
    b.points[0].fields = (0..=MAX_FIELDS)
        .map(|i| (format!("f{i}"), FieldValue::Boolean(true)))
        .collect();
    assert!(encode_batch(&b).is_err());
    let mut b = batch("ns", false);
    b.points[0].fields.insert(
        "long".into(),
        FieldValue::String("x".repeat(MAX_LINE_BYTES)),
    );
    assert!(encode_batch(&b).is_err());
}
#[test]
fn gzip_bombs_truncation_trailing_bytes_and_unknown_encoding_fail() {
    use std::io::Write;
    let mut z = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(&vec![b'x'; MAX_BODY_BYTES + 1]).unwrap();
    assert!(decode_body(&z.finish().unwrap(), "gzip").is_err());
    let bytes = encode_batch(&batch("ns", true)).unwrap();
    assert!(decode_body(&bytes[..bytes.len() - 1], "gzip").is_err());
    let mut trailing = bytes.clone();
    trailing.extend(b"garbage");
    assert!(decode_body(&trailing, "gzip").is_err());
    assert!(decode_body(&bytes, "br").is_err());
}
