use netget::server::sflow::codec::*;
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::time::Instant;

use crate::helpers::sflow::{batch, golden};
fn value(bytes: &[u8]) -> Value {
    serde_json::to_value(decode(bytes).unwrap()).unwrap()
}
fn put(bytes: &mut [u8], at: usize, n: u32) {
    bytes[at..at + 4].copy_from_slice(&n.to_be_bytes());
}
fn tracked(cache: &mut SequenceCache, seq: u32, uptime: u32, now: Instant) -> Message {
    let mut bytes = golden();
    put(&mut bytes, 16, seq);
    put(&mut bytes, 20, uptime);
    cache
        .ingest("127.0.0.1:5555".parse().unwrap(), &bytes, now)
        .unwrap()
}

#[test]
fn literal_four_carrier_golden_in_both_directions() {
    let bytes = golden();
    let (encoded, count) = encode(&batch(), u32::MAX, 123456).unwrap();
    assert_eq!(encoded, bytes);
    assert_eq!(count, 4);
    let got = value(&bytes);
    assert_eq!(got["agent_address"], "192.0.2.1");
    assert_eq!(got["sub_agent_id"], 77);
    assert_eq!(got["sequence_number"], u32::MAX);
    assert_eq!(got["uptime_ms"], 123456);
    assert_eq!(got["record_count"], 4);
    for (i, expanded) in [(0, false), (1, false), (2, true), (3, true)] {
        assert_eq!(got["samples"][i]["expanded"], expanded);
        assert_eq!(got["samples"][i]["source"], json!({"class":2,"index":3}));
    }
    for i in [0, 2] {
        assert_eq!(got["samples"][i]["flow"]["sampling_rate"], 1000);
        assert_eq!(
            got["samples"][i]["flow"]["input"],
            json!({"format":1,"value":9})
        );
        assert_eq!(
            got["samples"][i]["flow"]["output"],
            json!({"format":2,"value":3})
        );
        assert_eq!(
            got["samples"][i]["records"][0],
            json!({"kind":"sampled_ipv4",
            "packet_length":64,"protocol":17,"source_ip":"192.0.2.2","destination_ip":"198.51.100.2",
            "source_port":53,"destination_port":123,"tcp_flags":0,"traffic_class":16})
        );
    }
    for i in [1, 3] {
        assert_eq!(
            got["samples"][i]["records"][0],
            json!({"kind":"vlan_counters",
            "vlan_id":42,"octets":9007199254740999u64,"unicast_packets":11,
            "multicast_packets":12,"broadcast_packets":13,"discards":14})
        );
    }
}

#[test]
fn all_truncations_bad_lengths_versions_and_byte_bound_are_rejected() {
    let bytes = golden();
    for end in 0..bytes.len() {
        assert!(
            decode(&bytes[..end]).is_err(),
            "accepted truncation at {end}"
        );
    }
    for (offset, replacement) in [(0, 4), (4, 3), (24, 33), (32, 8193), (64, 65), (72, 33)] {
        let mut malformed = bytes.clone();
        put(&mut malformed, offset, replacement);
        assert!(
            decode(&malformed).is_err(),
            "accepted invalid field at {offset}"
        );
    }
    let mut extra = bytes;
    extra.push(0);
    assert!(decode(&extra).is_err());
    assert!(decode(&vec![0; MAX_MESSAGE_BYTES + 1]).is_err());
}

#[test]
fn datagram_sequence_wrap_gap_late_and_lower_uptime_do_not_regress() {
    let now = Instant::now();
    let mut cache = SequenceCache::new(Duration::from_secs(10));
    assert_eq!(
        tracked(&mut cache, u32::MAX, u32::MAX - 10, now)
            .sequence_tracking
            .status,
        "untracked"
    );
    let wrap = tracked(&mut cache, 0, 5, now);
    assert_eq!(wrap.sequence_tracking.status, "in_order");
    assert!(!wrap.sequence_tracking.uptime_decreased);
    let gap = tracked(&mut cache, 3, 100, now);
    assert_eq!(gap.sequence_tracking.missing_datagrams, Some(2));
    let late = tracked(&mut cache, 1, 20, now);
    assert_eq!(late.sequence_tracking.status, "out_of_order_or_duplicate");
    assert!(late.sequence_tracking.uptime_decreased);
    assert_eq!(
        tracked(&mut cache, 4, 101, now).sequence_tracking.status,
        "in_order"
    );
    let forward_lower = tracked(&mut cache, 5, 1, now);
    assert_eq!(forward_lower.sequence_tracking.status, "in_order");
    assert!(forward_lower.sequence_tracking.uptime_decreased);
    cache.expire(now + Duration::from_secs(10));
    assert_eq!(cache.count(), 0);
    assert_eq!(
        tracked(&mut cache, 0, 0, now + Duration::from_secs(10))
            .sequence_tracking
            .status,
        "untracked"
    );
}

#[test]
fn malformed_input_is_transactional_and_sessions_are_isolated_bounded_and_expire() {
    let now = Instant::now();
    let mut cache = SequenceCache::new(Duration::from_secs(2));
    let mut bytes = golden();
    put(&mut bytes, 16, 10);
    let peer: SocketAddr = "127.0.0.1:5555".parse().unwrap();
    cache.ingest(peer, &bytes, now).unwrap();
    let mut bad = bytes.clone();
    put(&mut bad, 16, 500);
    bad.pop();
    assert!(cache.ingest(peer, &bad, now).is_err());
    put(&mut bytes, 16, 11);
    assert_eq!(
        cache
            .ingest(peer, &bytes, now)
            .unwrap()
            .sequence_tracking
            .status,
        "in_order"
    );
    for p in 1..MAX_SESSIONS {
        cache
            .ingest(SocketAddr::from(([127, 0, 0, 1], p as u16)), &bytes, now)
            .unwrap();
    }
    assert_eq!(cache.count(), MAX_SESSIONS);
    assert!(cache
        .ingest("127.0.0.1:9000".parse().unwrap(), &bytes, now)
        .is_err());
    cache.expire(now + Duration::from_secs(2));
    cache
        .ingest(peer, &bytes, now + Duration::from_secs(2))
        .unwrap();
    put(&mut bytes, 12, 78);
    assert_eq!(
        cache
            .ingest(peer, &bytes, now + Duration::from_secs(2))
            .unwrap()
            .sequence_tracking
            .status,
        "untracked"
    );
    bytes[11] = 2;
    assert_eq!(
        cache
            .ingest(peer, &bytes, now + Duration::from_secs(2))
            .unwrap()
            .sequence_tracking
            .status,
        "untracked"
    );
    assert_eq!(cache.count(), 3);
}

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}
fn xdr(tag: u32, body: Vec<u8>) -> Vec<u8> {
    let mut framed = words(&[tag, body.len() as u32]);
    framed.extend_from_slice(&body);
    framed.resize(framed.len() + (4 - body.len() % 4) % 4, 0);
    framed
}
fn datagram(samples: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = words(&[5, 1, 0xc0000201, 77, 0, 100, samples.len() as u32]);
    for sample in samples {
        out.extend(sample);
    }
    out
}
fn flow(records: Vec<Vec<u8>>) -> Vec<u8> {
    let mut body = words(&[5, 3, 1000, 12345, 0, 3, 4, records.len() as u32]);
    for record in records {
        body.extend(record);
    }
    xdr(1, body)
}
fn raw_header(protocol: u32, header: Vec<u8>) -> Vec<u8> {
    let mut body = words(&[protocol, 512, 0, header.len() as u32]);
    body.extend_from_slice(&header);
    body.resize(body.len() + (4 - header.len() % 4) % 4, 0);
    xdr(1, body)
}

#[test]
fn exact_message_sample_per_sample_total_record_and_unknown_byte_limits() {
    let full = datagram(vec![xdr(4095, vec![42; 4096]), xdr(4095, vec![43; 4052])]);
    assert_eq!(full.len(), MAX_MESSAGE_BYTES);
    assert!(decode(&full).is_ok());
    let over = datagram(vec![xdr(4095, vec![42; 4096]), xdr(4095, vec![43; 4053])]);
    assert!(decode(&over).is_err());
    assert!(decode(&datagram(vec![xdr(4095, vec![]); MAX_SAMPLES])).is_ok());
    assert!(decode(&datagram(vec![xdr(4095, vec![]); MAX_SAMPLES + 1])).is_err());
    assert!(decode(&datagram(vec![xdr(4095, vec![0; MAX_OPAQUE_BYTES + 1])])).is_err());
    let record = xdr(1001, words(&[42, 3, 43, 4]));
    assert_eq!(
        decode(&datagram(vec![flow(vec![
            record.clone();
            MAX_RECORDS_PER_SAMPLE
        ])]))
        .unwrap()
        .record_count,
        64
    );
    assert!(decode(&datagram(vec![flow(vec![
        record.clone();
        MAX_RECORDS_PER_SAMPLE + 1
    ])]))
    .is_err());
    let mut samples = vec![flow(vec![record.clone(); 64]); 4];
    assert_eq!(
        decode(&datagram(samples.clone())).unwrap().record_count,
        MAX_RECORDS
    );
    samples.push(flow(vec![record]));
    assert!(decode(&datagram(samples)).is_err());
    let unknown = value(&datagram(vec![flow(vec![xdr(
        (424242 << 12) | 99,
        b"secret\0payload".to_vec(),
    )])]));
    assert_eq!(
        unknown["samples"][0]["records"][0],
        json!({"kind":"unknown","enterprise":424242,"format":99,"byte_count":14})
    );
    assert!(!unknown.to_string().contains("secret"));
    let padded = datagram(vec![xdr((424242 << 12) | 98, vec![1, 2, 3])]);
    assert!(decode(&padded).is_ok());
    let mut nonzero = padded;
    *nonzero.last_mut().unwrap() = 255;
    assert!(decode(&nonzero).is_ok());
}

#[test]
fn captured_header_bound_and_truncation_status_do_not_expose_bytes() {
    let max = datagram(vec![flow(vec![raw_header(
        999,
        vec![42; MAX_HEADER_BYTES],
    )])]);
    let got = value(&max);
    let record = &got["samples"][0]["records"][0];
    assert_eq!(record["header_length"], 256);
    assert_eq!(record["decode_status"], "unsupported_header_protocol");
    assert_eq!(record["packet"], Value::Null);
    assert!(decode(&datagram(vec![flow(vec![raw_header(
        11,
        vec![0; MAX_HEADER_BYTES + 1]
    )])]))
    .is_err());
    let truncated = value(&datagram(vec![flow(vec![raw_header(11, vec![0x45])])]));
    assert_eq!(
        truncated["samples"][0]["records"][0]["decode_status"],
        "truncated"
    );
    assert!(!record.to_string().contains("header_data"));
}

#[test]
fn ipv6_extension_and_ethernet_vlan_depth_and_fragment_ports_are_bounded() {
    let mut ip6 = vec![0; 40];
    ip6[0] = 0x60;
    ip6[4..6].copy_from_slice(&100u16.to_be_bytes());
    ip6[7] = 64;
    ip6[8..24].copy_from_slice(
        &"2001:db8::1"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets(),
    );
    ip6[24..40].copy_from_slice(
        &"2001:db8::2"
            .parse::<std::net::Ipv6Addr>()
            .unwrap()
            .octets(),
    );
    let mut at_limit = ip6.clone();
    for i in 0..8 {
        at_limit.extend_from_slice(&[if i == 7 { 17 } else { 0 }, 0, 0, 0, 0, 0, 0, 0]);
    }
    at_limit.extend_from_slice(&[0, 53, 0, 123, 0, 8, 0, 0]);
    let got = value(&datagram(vec![flow(vec![raw_header(12, at_limit)])]));
    assert_eq!(got["samples"][0]["records"][0]["packet"]["source_port"], 53);
    let mut too_deep = ip6.clone();
    too_deep.extend_from_slice(&[0; 9 * 8]);
    let got = value(&datagram(vec![flow(vec![raw_header(12, too_deep)])]));
    assert_eq!(
        got["samples"][0]["records"][0]["decode_status"],
        "extension_depth_limit"
    );
    let mut fragmented = ip6;
    fragmented[6] = 44;
    fragmented.extend_from_slice(&[17, 0, 0, 8, 0, 0, 0, 1, 0, 53, 0, 123]);
    let got = value(&datagram(vec![flow(vec![raw_header(12, fragmented)])]));
    assert_eq!(
        got["samples"][0]["records"][0]["decode_status"],
        "non_initial_fragment"
    );
    assert_eq!(got["samples"][0]["records"][0]["packet"]["source_port"], 0);
    let mut ethernet = vec![0; 12];
    ethernet.extend_from_slice(&[0x81, 0, 0, 42, 0x88, 0xa8, 0, 43, 8, 0]);
    ethernet.extend_from_slice(&[
        0x45, 0, 0, 64, 0, 0, 0, 0, 64, 17, 0, 0, 192, 0, 2, 1, 198, 51, 100, 2, 0, 53, 0, 123,
    ]);
    let got = value(&datagram(vec![flow(vec![raw_header(1, ethernet.clone())])]));
    assert_eq!(
        got["samples"][0]["records"][0]["link_layer"]["vlans"],
        json!([42, 43])
    );
    ethernet[20] = 0x81;
    ethernet[21] = 0;
    let got = value(&datagram(vec![flow(vec![raw_header(1, ethernet)])]));
    assert_eq!(
        got["samples"][0]["records"][0]["decode_status"],
        "vlan_depth_limit"
    );
}

#[test]
fn typed_export_compact_and_expanded_indices_scalar_validation_and_exact_limits() {
    let mut b = batch();
    if let Sample::Flow {
        source,
        input,
        records,
        ..
    } = &mut b.samples[0]
    {
        source.index = (1 << 24) - 1;
        input.value = (1 << 30) - 1;
        records.clear();
        records.resize(
            MAX_RECORDS_PER_SAMPLE,
            FlowRecord::ExtendedSwitch {
                source_vlan: 42,
                source_priority: 3,
                destination_vlan: 43,
                destination_priority: 4,
            },
        );
    }
    assert!(encode(&b, 0, 0).is_ok());
    if let Sample::Flow { source, .. } = &mut b.samples[0] {
        source.index = 1 << 24;
    }
    assert!(encode(&b, 0, 0).is_err());
    if let Sample::Flow {
        source,
        expanded,
        input,
        ..
    } = &mut b.samples[0]
    {
        *expanded = true;
        source.index = u32::MAX;
        input.value = u32::MAX;
    }
    assert!(encode(&b, 0, 0).is_ok());
    b.samples.resize(MAX_SAMPLES, b.samples[1].clone());
    assert!(encode(&b, 0, 0).is_ok());
    b.samples.push(b.samples[1].clone());
    assert!(encode(&b, 0, 0).is_err());
    let mut bad = serde_json::to_value(batch()).unwrap();
    bad["samples"][0]["records"][0]["source_ip"] = json!("2001:db8::1");
    let bad: Batch = serde_json::from_value(bad).unwrap();
    assert!(encode(&bad, 0, 0).is_err());
    let mut unknown = serde_json::to_value(batch()).unwrap();
    unknown["raw_bytes"] = json!("secret");
    assert!(serde_json::from_value::<Batch>(unknown).is_err());
    assert!(netget::server::sflow::duration(0).is_err());
    assert!(netget::server::sflow::duration(86401).is_err());
    assert!(netget::server::sflow::duration(1).is_ok());
    assert!(netget::server::sflow::duration(86400).is_ok());
}

#[test]
fn literal_ethernet_and_ipv6_summary_records_in_both_directions() {
    let hex = "00000064 0011223344550000 aabbccddeeff0000 000086dd"
        .split_whitespace()
        .collect::<String>();
    let ethernet = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let hex="00000080 00000006 20010db8000000000000000000000001 20010db8000000000000000000000002 000004d2 000001bb 00000102 000000b0".split_whitespace().collect::<String>();
    let ip6 = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect::<Vec<_>>();
    let oracle = datagram(vec![flow(vec![xdr(2, ethernet), xdr(4, ip6)])]);
    let typed:Batch=serde_json::from_value(json!({"agent_address":"192.0.2.1","sub_agent_id":77,"uptime_ms":100,
        "samples":[{"kind":"flow","sequence_number":5,"source":{"class":0,"index":3},"sampling_rate":1000,"sample_pool":12345,"drops":0,
            "input":{"format":0,"value":3},"output":{"format":0,"value":4},"records":[
                {"kind":"ethernet","frame_length":100,"source_mac":"00:11:22:33:44:55","destination_mac":"aa:bb:cc:dd:ee:ff","ether_type":34525},
                {"kind":"sampled_ipv6","packet_length":128,"protocol":6,"source_ip":"2001:db8::1","destination_ip":"2001:db8::2","source_port":1234,"destination_port":443,"tcp_flags":258,"traffic_class":176}
            ]}]})).unwrap();
    assert_eq!(encode(&typed, 0, 100).unwrap().0, oracle);
    let decoded = value(&oracle);
    assert_eq!(
        decoded["samples"][0]["records"][0],
        json!({"kind":"ethernet","frame_length":100,
        "source_mac":"00:11:22:33:44:55","destination_mac":"aa:bb:cc:dd:ee:ff","ether_type":34525})
    );
    assert_eq!(
        decoded["samples"][0]["records"][1],
        json!({"kind":"sampled_ipv6","packet_length":128,"protocol":6,
        "source_ip":"2001:db8::1","destination_ip":"2001:db8::2","source_port":1234,"destination_port":443,"tcp_flags":258,"traffic_class":176})
    );
    let mut bad = typed;
    if let Sample::Flow { records, .. } = &mut bad.samples[0] {
        if let FlowRecord::Ethernet { source_mac, .. } = &mut records[0] {
            *source_mac = "00:11:22:33:44:gg".into();
        }
    }
    assert!(encode(&bad, 0, 100).is_err());
}
