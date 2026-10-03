use netget::server::gelf::codec::*;
use serde_json::json;
use std::{io::Write, net::SocketAddr, time::Duration};
use tokio::time::Instant;
fn message() -> Message {
    serde_json::from_value(json!({"host":"demo","short_message":"温度","full_message":"line\nnext","timestamp":1700000000.25,"level":6,"additional_fields":{"service":"api","count":42}})).unwrap()
}
fn peer() -> SocketAddr {
    "127.0.0.1:12345".parse().unwrap()
}
fn chunk(id: u64, seq: u8, count: u8, data: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x1e, 0x0f];
    bytes.extend_from_slice(&id.to_be_bytes());
    bytes.extend_from_slice(&[seq, count]);
    bytes.extend_from_slice(data);
    bytes
}
#[test]
fn structured_json_compression_and_tcp_fragment_boundaries() {
    let m = message();
    let json = encode_json(&m).unwrap();
    assert_eq!(parse_json(&json).unwrap(), m);
    for compression in [Compression::None, Compression::Gzip, Compression::Zlib] {
        let chunks = encode_udp(&m, compression, 64, [1; 8]).unwrap();
        let mut r = Reassembler::default();
        let now = Instant::now();
        let mut found = None;
        for chunk in chunks.iter().rev() {
            if let Some(m) = r.push(peer(), chunk, now).unwrap() {
                found = Some(m);
            }
        }
        assert_eq!(found, Some(m.clone()));
        assert_eq!(r.pending_bytes(), 0);
    }
    let mut wire = json.clone();
    wire.push(0);
    wire.extend_from_slice(&json);
    wire.push(0);
    for split in 0..wire.len() {
        let mut d = TcpDecoder::default();
        let mut messages = Vec::new();
        d.feed(&wire[..split]).unwrap();
        while let Some(m) = d.next_message().unwrap() {
            messages.push(m);
        }
        d.feed(&wire[split..]).unwrap();
        while let Some(m) = d.next_message().unwrap() {
            messages.push(m);
        }
        assert_eq!(messages, vec![m.clone(), m.clone()]);
        d.finish().unwrap();
    }
}
#[test]
fn missing_required_invalid_unknown_and_typed_fields_fail() {
    for bytes in [
        b"{}".as_slice(),
        br#"{"version":"1.0","host":"x","short_message":"x"}"#,
        br#"{"version":"1.1","host":"x","short_message":"x","_id":1}"#,
        br#"{"version":"1.1","host":"x","short_message":"x","_array":[]}"#,
        br#"{"version":"1.1","host":"x","short_message":"x","unknown":1}"#,
        br#"{"version":"1.1","host":"x","short_message":"x","level":8}"#,
        br#"{"version":"1.1","host":"x","short_message":"x","timestamp":-1}"#,
        br#"{"version":"1.1","host":"","short_message":"x"}"#,
        b"\xff",
    ] {
        assert!(parse_json(bytes).is_err(), "{:?}", bytes);
    }
    let mut m = message();
    m.additional_fields.insert("bad!".into(), json!("x"));
    assert!(encode_json(&m).is_err());
    m = message();
    m.timestamp = Some(f64::NAN);
    assert!(encode_json(&m).is_err());
    assert!(Transport::parse("unix").is_err());
    assert!(Compression::parse("gzip", Transport::Tcp).is_err());
    assert!(Compression::parse("zlib", Transport::Tcp).is_err());
    assert!(Compression::parse("other", Transport::Udp).is_err());
}
#[test]
fn chunk_source_duplicate_conflict_late_and_replay_controls() {
    let now = Instant::now();
    let mut r = Reassembler::default();
    let bytes = encode_json(&message()).unwrap();
    let mid = bytes.len() / 2;
    let a = chunk(1, 0, 2, &bytes[..mid]);
    let b = chunk(1, 1, 2, &bytes[mid..]);
    assert!(r.push(peer(), &a, now).unwrap().is_none());
    let used = r.pending_bytes();
    assert!(r.push(peer(), &a, now).unwrap().is_none());
    assert_eq!(r.pending_bytes(), used);
    assert!(r
        .push("127.0.0.1:54321".parse().unwrap(), &b, now)
        .unwrap()
        .is_none());
    assert_eq!(r.pending_count(), 2);
    assert!(r.push(peer(), &b, now).unwrap().is_some());
    assert!(r.push(peer(), &a, now).unwrap().is_none());
    assert_eq!(r.pending_count(), 1);
    let a = chunk(2, 0, 2, &bytes[..mid]);
    assert!(r.push(peer(), &a, now).unwrap().is_none());
    assert!(r.push(peer(), &chunk(2, 0, 2, b"conflict"), now).is_err());
    assert!(r
        .push(peer(), &chunk(2, 1, 2, &bytes[mid..]), now)
        .unwrap()
        .is_none());
    r.expire(now + CHUNK_TIMEOUT);
    assert_eq!(r.pending_count(), 0);
    assert_eq!(r.pending_bytes(), 0);
    assert!(r
        .push("127.0.0.1:54321".parse().unwrap(), &a, now + CHUNK_TIMEOUT)
        .unwrap()
        .is_none());
    let mut late = Reassembler::default();
    late.push(peer(), &chunk(9, 0, 2, &bytes[..mid]), now)
        .unwrap();
    assert!(late
        .push(peer(), &chunk(9, 1, 2, &bytes[mid..]), now + CHUNK_TIMEOUT)
        .unwrap()
        .is_none());
    assert_eq!(late.pending_count(), 0);
}
#[test]
fn every_declared_byte_count_and_memory_bound_is_enforced() {
    let mut m = message();
    m.full_message = Some("x".repeat(MAX_MESSAGE_BYTES));
    assert!(encode_json(&m).is_err());
    assert!(parse_json(&vec![b' '; MAX_MESSAGE_BYTES + 1]).is_err());
    let mut d = TcpDecoder::default();
    d.feed(&vec![b' '; MAX_MESSAGE_BYTES + 1]).unwrap();
    assert!(d.next_message().is_err());
    let mut d = TcpDecoder::default();
    assert!(d.feed(&vec![b' '; MAX_MESSAGE_BYTES + 8193]).is_err());
    let mut d = TcpDecoder::default();
    d.feed(b"partial").unwrap();
    assert!(d.finish().is_err());
    let now = Instant::now();
    let mut r = Reassembler::default();
    for bytes in [
        vec![],
        vec![0; MAX_DATAGRAM_BYTES + 1],
        vec![0x1e, 0x0f],
        chunk(1, 0, 0, b"x"),
        chunk(1, 0, 129, b"x"),
        chunk(1, 2, 2, b"x"),
    ] {
        assert!(r.push(peer(), &bytes, now).is_err());
    }
    assert!(encode_udp(&message(), Compression::None, 12, [0; 8]).is_err());
    assert!(encode_udp(&message(), Compression::None, 8193, [0; 8]).is_err());
    assert!(encode_udp(&message(), Compression::None, 13, [0; 8]).is_err());
    let mut r = Reassembler::default();
    for id in 0..MAX_PENDING_MESSAGES {
        r.push(peer(), &chunk(id as u64, 0, 2, b"x"), now).unwrap();
    }
    assert!(r.push(peer(), &chunk(999, 0, 2, b"x"), now).is_err());
    assert_eq!(r.pending_count(), MAX_PENDING_MESSAGES);
    r.expire(now + Duration::from_secs(5));
    assert_eq!(r.pending_count(), 0);
    let mut r = Reassembler::default();
    let payload = vec![b'x'; MAX_DATAGRAM_BYTES - 12];
    for seq in 0..32 {
        r.push(peer(), &chunk(1, seq, 128, &payload), now).unwrap();
    }
    assert!(r.push(peer(), &chunk(1, 32, 128, &payload), now).is_err());
    assert_eq!(r.pending_bytes(), 0);
    let mut r = Reassembler::default();
    let mut exceeded = false;
    'outer: for id in 0..128 {
        for seq in 0..31 {
            if r.push(peer(), &chunk(id, seq, 128, &payload), now).is_err() {
                exceeded = true;
                break 'outer;
            }
        }
    }
    assert!(exceeded);
    assert!(r.pending_bytes() <= MAX_PENDING_BYTES);
}
#[test]
fn gzip_zlib_bombs_truncation_and_trailing_bytes_fail() {
    for gzip in [true, false] {
        let bomb = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        let compressed = if gzip {
            let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            w.write_all(&bomb).unwrap();
            w.finish().unwrap()
        } else {
            let mut w = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            w.write_all(&bomb).unwrap();
            w.finish().unwrap()
        };
        assert!(decompress(&compressed).is_err(), "gzip={gzip}");
        let mut compressed = encode_udp(
            &message(),
            if gzip {
                Compression::Gzip
            } else {
                Compression::Zlib
            },
            8192,
            [0; 8],
        )
        .unwrap()
        .remove(0);
        assert_eq!(decompress(&compressed).unwrap(), message());
        compressed.pop();
        assert!(decompress(&compressed).is_err(), "gzip={gzip}");
        let mut compressed = encode_udp(
            &message(),
            if gzip {
                Compression::Gzip
            } else {
                Compression::Zlib
            },
            8192,
            [0; 8],
        )
        .unwrap()
        .remove(0);
        compressed.extend_from_slice(b"junk");
        assert!(decompress(&compressed).is_err(), "gzip={gzip}");
    }
}

#[test]
fn remembered_chunk_ids_are_bounded_and_expire() {
    let now = Instant::now();
    let json = encode_json(&message()).unwrap();
    let mut r = Reassembler::default();
    for id in 0..MAX_RECENT_IDS + 64 {
        assert!(r
            .push(peer(), &chunk(id as u64, 0, 1, &json), now)
            .unwrap()
            .is_some());
    }
    assert_eq!(r.recent_count(), MAX_RECENT_IDS);
    r.expire(now + CHUNK_TIMEOUT);
    assert_eq!(r.recent_count(), 0);
}
