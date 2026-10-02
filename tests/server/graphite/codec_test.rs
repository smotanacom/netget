use netget::server::graphite::codec::*;

#[test]
fn published_plaintext_vectors_preserve_unicode_tags_and_fractional_numbers() {
    let metrics = vec![
        Metric {
            path: "servers.demo.load".into(),
            value: 0.5,
            timestamp: 1700000000.25,
        },
        Metric {
            path: "温度;location=lab".into(),
            value: -1.25,
            timestamp: -1.0,
        },
    ];
    assert_eq!(
        encode_batch(&metrics).unwrap(),
        "servers.demo.load 0.5 1700000000.25\n温度;location=lab -1.25 -1\n".as_bytes()
    );
    assert_eq!(
        parse_line(b"  cpu.load\t1.5\t1700000000\r").unwrap().value,
        1.5
    );
}
#[test]
fn every_fragment_boundary_including_utf8_is_supported() {
    let wire = "温度 1.5 1700000000\nother -2 -1\n".as_bytes();
    for split in 0..=wire.len() {
        let mut decoder = Decoder::default();
        decoder.feed(&wire[..split]).unwrap();
        let mut got = decoder.next_batch().unwrap();
        decoder.feed(&wire[split..]).unwrap();
        got.extend(decoder.next_batch().unwrap());
        assert_eq!(got.len(), 2, "split {split}");
        assert_eq!(got[0].path, "温度");
        assert_eq!(got[1].timestamp, -1.0);
        decoder.finish().unwrap();
    }
}
#[test]
fn malformed_nonfinite_and_injected_fields_are_rejected() {
    for line in [
        b"".as_slice(),
        b"a 1",
        b"a 1 2 extra",
        b"a NaN 2",
        b"a inf 2",
        b"a 1 NaN",
        b"a 1 -2",
        b"a 1 -0.5",
        b"\xff 1 2",
    ] {
        assert!(parse_line(line).is_err(), "{line:?}");
    }
    for path in ["", "x\ny", "x y", "x\0y"] {
        assert!(encode_batch(&[Metric {
            path: path.into(),
            value: 1.0,
            timestamp: 1.0
        }])
        .is_err());
    }
    assert!(encode_batch(&[Metric {
        path: "x".into(),
        value: f64::INFINITY,
        timestamp: 1.0
    }])
    .is_err());
    assert!(serde_json::from_value::<Metric>(
        serde_json::json!({"path":"x","value":1,"timestamp":2,"extra":1})
    )
    .is_err());
}
#[test]
fn line_batch_and_pending_buffer_limits_are_enforced() {
    let exact = format!("{} 1 2", "a".repeat(MAX_LINE_BYTES - 4));
    assert!(parse_line(exact.as_bytes()).is_ok());
    assert!(parse_line(format!("a{exact}").as_bytes()).is_err());
    let metric = Metric {
        path: "a".into(),
        value: 1.0,
        timestamp: 2.0,
    };
    assert!(encode_batch(&vec![metric.clone(); MAX_BATCH_RECORDS]).is_ok());
    assert!(encode_batch(&vec![metric; MAX_BATCH_RECORDS + 1]).is_err());
    assert!(encode_batch(&[]).is_err());
    let long = parse_line(exact.as_bytes()).unwrap();
    assert!(encode_batch(&vec![long.clone(); 15]).is_ok());
    assert!(encode_batch(&vec![long; 16]).is_err());
    let mut decoder = Decoder::default();
    decoder.feed(&vec![b'a'; MAX_LINE_BYTES + 1]).unwrap();
    assert!(decoder.next_batch().is_err());
    assert!(decoder.finish().is_err());
    assert!(Decoder::default()
        .feed(&vec![b'a'; MAX_LINE_BYTES + READ_BYTES + 1])
        .is_err());
}
#[test]
fn coalesced_lines_are_drained_in_bounded_batches_and_eof_requires_lf() {
    let mut decoder = Decoder::default();
    decoder.feed("a 1 2\n".repeat(600).as_bytes()).unwrap();
    assert_eq!(decoder.next_batch().unwrap().len(), 256);
    assert_eq!(decoder.next_batch().unwrap().len(), 256);
    assert_eq!(decoder.next_batch().unwrap().len(), 88);
    assert!(decoder.next_batch().unwrap().is_empty());
    decoder.finish().unwrap();
    decoder.feed(b"last 1 2").unwrap();
    assert!(decoder.next_batch().unwrap().is_empty());
    assert!(decoder.finish().is_err());
}
