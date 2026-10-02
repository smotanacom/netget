use netget::server::fluent_forward::codec::*;
use serde_json::json;
use std::io::Write;
pub fn batch(mode: &str) -> Batch {
    serde_json::from_value(json!({"tag":"demo.logs","entries":[{"timestamp":{"seconds":1700000000,"nanoseconds":250000000},"record":{"message":"温度\nnext","n":42,"ok":true,"nil":null,"nested":{"list":[1,-2,0.5]}}}],"mode":mode,"require_ack":true})).unwrap()
}
#[test]
fn four_modes_eventtime_typed_records_and_every_split_roundtrip() {
    for mode in ["message", "forward", "packed", "compressed_packed"] {
        let b = batch(mode);
        let wire = encode_batch(&b, Some("opaque-correlation")).unwrap();
        let (node, n) = parse_one(&wire).unwrap().unwrap();
        assert_eq!(n, wire.len());
        let got = parse_batch(node).unwrap().unwrap();
        assert_eq!(got.tag, b.tag);
        assert_eq!(got.entries, b.entries);
        assert_eq!(got.mode, mode);
        assert_eq!(got.chunk.as_deref(), Some("opaque-correlation"));
        for split in 0..wire.len() {
            let mut d = Decoder::default();
            d.feed(&wire[..split]).unwrap();
            let first = d.next_node().unwrap();
            d.feed(&wire[split..]).unwrap();
            let got = first.or_else(|| d.next_node().unwrap()).unwrap();
            assert_eq!(parse_batch(got).unwrap().unwrap().entries, b.entries);
            d.finish().unwrap();
        }
    }
}
#[test]
fn packed_legacy_string_text_option_coalescing_and_heartbeat() {
    let mut b = batch("packed");
    b.entries[0].timestamp.nanoseconds = None;
    let wire = encode_batch(&b, None).unwrap();
    let (Node::Array(mut a), _) = parse_one(&wire).unwrap().unwrap() else {
        panic!()
    };
    let Node::Bin(bytes) = &a[1] else { panic!() };
    a[1] = Node::Str(bytes.clone());
    let Node::Map(opts) = a.last_mut().unwrap() else {
        panic!()
    };
    opts.push((
        Node::Str(b"compressed".to_vec()),
        Node::Str(b"text".to_vec()),
    ));
    assert_eq!(
        parse_batch(Node::Array(a)).unwrap().unwrap().entries,
        b.entries
    );
    let mut d = Decoder::default();
    d.feed(&[vec![0xc0], wire.clone(), wire].concat()).unwrap();
    assert_eq!(parse_batch(d.next_node().unwrap().unwrap()).unwrap(), None);
    for _ in 0..2 {
        assert!(parse_batch(d.next_node().unwrap().unwrap())
            .unwrap()
            .is_some());
    }
    d.finish().unwrap();
}
#[test]
fn exact_ack_wire_and_integer_carrier() {
    assert_eq!(encode_ack("abc").unwrap(), b"\x81\xa3ack\xa3abc");
    assert_eq!(
        parse_ack(parse_one(b"\x81\xa3ack\xa3abc").unwrap().unwrap().0).unwrap(),
        "abc"
    );
    let mut b = batch("forward");
    b.tag = "t".into();
    b.require_ack = false;
    b.entries[0] =
        serde_json::from_value(json!({"timestamp":{"seconds":1},"record":{"m":"x"}})).unwrap();
    assert_eq!(
        encode_batch(&b, None).unwrap(),
        b"\x93\xa1t\x91\x92\x01\x81\xa1m\xa1x\x81\xa4size\x01"
    );
}
#[test]
fn malformed_declared_lengths_depth_elements_record_time_and_eof_bounds() {
    for bytes in [
        vec![0xc1],
        vec![0xdb, 0, 4, 0, 1],
        vec![0xdd, 0, 0, 0x40, 1],
        [vec![0x91; MAX_DEPTH + 1], vec![0xc0]].concat(),
        vec![0xdf, 0, 0, 0x20, 1],
    ] {
        assert!(parse_one(&bytes).is_err());
    }
    let mut d = Decoder::default();
    assert!(d.feed(&vec![0; MAX_FRAME_BYTES + 8193]).is_err());
    d.feed(&[0xdb, 0, 4, 0, 0]).unwrap();
    assert!(d.finish().is_err());
    assert!(parse_one(&[vec![0xc6, 0, 4, 0, 0], vec![0; MAX_FRAME_BYTES]].concat()).is_err());
    for value in [
        json!({"tag":"","entries":[]}),
        json!({"tag":"x".repeat(1025),"entries":batch("forward").entries}),
        json!({"tag":"x","mode":"message","entries":vec![batch("forward").entries[0].clone();2]}),
        json!({"tag":"x","mode":"unknown","entries":batch("forward").entries}),
        json!({"tag":"x","entries":vec![batch("forward").entries[0].clone();MAX_RECORDS+1]}),
    ] {
        let b: Batch = serde_json::from_value(value).unwrap();
        assert!(encode_batch(&b, None).is_err());
    }
    let mut b = batch("forward");
    b.entries[0].timestamp.nanoseconds = Some(1_000_000_000);
    assert!(encode_batch(&b, None).is_err());
    b.entries[0].timestamp.nanoseconds = Some(1);
    b.entries[0].timestamp.seconds = u32::MAX as u64 + 1;
    assert!(encode_batch(&b, None).is_err());
    let mut b = batch("packed");
    b.entries[0]
        .record
        .insert("big".into(), json!("x".repeat(MAX_FRAME_BYTES)));
    assert!(encode_batch(&b, None).is_err());
    let bad = Node::Array(vec![
        Node::Str(b"t".to_vec()),
        Node::Uint(1),
        Node::Map(vec![(Node::Str(b"v".to_vec()), Node::Bin(vec![1]))]),
    ]);
    assert!(parse_batch(bad).is_err());
    assert!(parse_ack(Node::Nil).is_err());
    assert!(parse_ack(Node::Map(vec![])).is_err());
}
#[test]
fn gzip_packed_bombs_trailing_truncation_and_size_mismatch() {
    let b = batch("compressed_packed");
    let (Node::Array(mut a), _) = parse_one(&encode_batch(&b, None).unwrap())
        .unwrap()
        .unwrap()
    else {
        panic!()
    };
    let Node::Bin(bytes) = &a[1] else { panic!() };
    let mut bytes = bytes.clone();
    bytes.pop();
    a[1] = Node::Bin(bytes);
    assert!(parse_batch(Node::Array(a.clone())).is_err());
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&vec![0; MAX_FRAME_BYTES + 1]).unwrap();
    a[1] = Node::Bin(encoder.finish().unwrap());
    assert!(parse_batch(Node::Array(a)).is_err());
    let (Node::Array(mut a), _) = parse_one(&encode_batch(&batch("forward"), None).unwrap())
        .unwrap()
        .unwrap()
    else {
        panic!()
    };
    let Node::Map(opts) = a.last_mut().unwrap() else {
        panic!()
    };
    opts[0].1 = Node::Uint(2);
    assert!(parse_batch(Node::Array(a)).is_err());
}

#[test]
fn packed_value_budget_is_shared_across_entries_and_gzip_members_have_exact_trailers() {
    let make_entry = || {
        Node::Array(vec![
            Node::Uint(1),
            Node::Map(vec![(
                Node::Str(b"many".to_vec()),
                Node::Array(vec![Node::Nil; MAX_VALUES / 2]),
            )]),
        ])
    };
    let packed = [
        encode_node(&make_entry()).unwrap(),
        encode_node(&make_entry()).unwrap(),
    ]
    .concat();
    assert!(parse_batch(Node::Array(vec![
        Node::Str(b"t".to_vec()),
        Node::Bin(packed)
    ]))
    .is_err());
    let valid = Node::Array(vec![
        Node::Uint(1),
        Node::Map(vec![(
            Node::Str(b"message".to_vec()),
            Node::Str(b"ok".to_vec()),
        )]),
    ]);
    let gzip = |bytes: &[u8]| {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    };
    let member = gzip(&encode_node(&valid).unwrap());
    let frame = |bytes: Vec<u8>| {
        Node::Array(vec![
            Node::Str(b"t".to_vec()),
            Node::Bin(bytes),
            Node::Map(vec![(
                Node::Str(b"compressed".to_vec()),
                Node::Str(b"gzip".to_vec()),
            )]),
        ])
    };
    assert_eq!(
        parse_batch(frame([member.clone(), member.clone()].concat()))
            .unwrap()
            .unwrap()
            .entries
            .len(),
        2
    );
    assert!(parse_batch(frame([member.clone(), vec![0]].concat())).is_err());
    assert!(parse_batch(frame(member[..member.len() - 4].to_vec())).is_err());
    let mut corrupt = member;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    assert!(parse_batch(frame(corrupt)).is_err());
}

#[test]
fn outbound_packed_depth_rejects_inner_stream_before_emission() {
    let mut nested = json!(null);
    for _ in 0..MAX_DEPTH - 1 {
        nested = json!([nested]);
    }
    for mode in ["message", "forward", "packed", "compressed_packed"] {
        let mut b = batch(mode);
        b.entries[0].record = serde_json::from_value(json!({"nested":nested})).unwrap();
        assert!(
            encode_batch(&b, None).is_err(),
            "{mode} must reject nested inner frame"
        );
    }
}
