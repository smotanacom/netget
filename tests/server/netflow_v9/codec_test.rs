use netget::server::netflow_v9::codec::*;
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::time::Instant;
pub fn peer() -> SocketAddr {
    "127.0.0.1:20550".parse().unwrap()
}
pub fn cache() -> TemplateCache {
    TemplateCache::new(Duration::from_secs(10), Duration::from_secs(30))
}
pub fn sample() -> Batch {
    serde_json::from_value(netget::client::netflow_v9::actions::example_batch()).unwrap()
}
pub fn packet(source: u32, seq: u32, sets: &[(u16, Vec<u8>)]) -> Vec<u8> {
    packet_count(source, seq, sets.len() as u16, sets)
}
pub fn packet_count(source: u32, seq: u32, count: u16, sets: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut b = vec![0, 9];
    b.extend(count.to_be_bytes());
    for n in [1000, 1700000000, seq, source] {
        b.extend(n.to_be_bytes());
    }
    for (id, body) in sets {
        b.extend(id.to_be_bytes());
        b.extend(((body.len() + 4) as u16).to_be_bytes());
        b.extend(body);
    }
    b
}
pub fn template(id: u16, fields: &[(u16, u16)]) -> Vec<u8> {
    let mut b = id.to_be_bytes().to_vec();
    b.extend((fields.len() as u16).to_be_bytes());
    for (i, n) in fields {
        b.extend(i.to_be_bytes());
        b.extend(n.to_be_bytes());
    }
    b
}
fn scalar(element: Element, length: Option<u16>, value: FieldValue) -> Batch {
    serde_json::from_value(json!({"source_id":42,"templates":[{"id":256,"fields":[{"element":element,"length":length},{"element":"source_ipv4"}]}],"data_sets":[{"template_id":256,"records":[[value,{"kind":"ipv4","value":"192.0.2.1"}]]}]})).unwrap()
}
#[test]
fn literal_rfc_v9_header_templates_count_and_both_directions() {
    let b = include_str!("v9_ipv4.hex")
        .split_whitespace()
        .map(|v| u8::from_str_radix(v, 16).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        encode(&sample(), u32::MAX, 1700000000, 123456).unwrap().0,
        b
    );
    let m = cache().ingest(peer(), &b, Instant::now()).unwrap();
    assert_eq!(
        (m.header_count, m.record_count, m.known_total_record_count),
        (2, 1, 2)
    );
    assert_eq!(m.sequence_number, u32::MAX);
    assert_eq!(m.sys_uptime_ms, 123456);
    assert_eq!(m.source_id, 42);
    assert_eq!(m.count_status, "validated");
    assert_eq!(
        m.data_sets[0].records[0],
        vec![
            Some(FieldValue::Ipv4("192.0.2.1".parse().unwrap())),
            Some(FieldValue::Ipv4("198.51.100.2".parse().unwrap())),
            Some(FieldValue::Unsigned(5))
        ]
    );
}
#[test]
fn counters_all_widths_fixed_flags_ipv6_and_uptime_are_typed() {
    for n in 1..=8 {
        let max = if n == 8 {
            u64::MAX
        } else {
            (1u64 << (n * 8)) - 1
        };
        let b = scalar(Element::InBytes, Some(n), FieldValue::Unsigned(max));
        let w = encode(&b, 0, 0, 0).unwrap().0;
        let m = cache().ingest(peer(), &w, Instant::now()).unwrap();
        assert_eq!(
            m.data_sets[0].records[0][0],
            Some(FieldValue::Unsigned(max))
        );
        if n < 8 {
            assert!(encode(
                &scalar(Element::InBytes, Some(n), FieldValue::Unsigned(max + 1)),
                0,
                0,
                0
            )
            .is_err());
        }
    }
    for (e, v) in [
        (Element::TcpFlags, FieldValue::Unsigned(255)),
        (
            Element::FirstSwitched,
            FieldValue::UptimeMilliseconds(u32::MAX),
        ),
        (
            Element::SourceIpv6,
            FieldValue::Ipv6("2001:db8::1".parse().unwrap()),
        ),
    ] {
        let w = encode(&scalar(e, None, v.clone()), 0, 0, 0).unwrap().0;
        assert_eq!(
            cache()
                .ingest(peer(), &w, Instant::now())
                .unwrap()
                .data_sets[0]
                .records[0][0],
            Some(v)
        );
    }
    assert!(encode(
        &scalar(Element::TcpFlags, Some(2), FieldValue::Unsigned(256)),
        0,
        0,
        0
    )
    .is_err());
    assert!(encode(
        &scalar(
            Element::SourceIpv4,
            Some(3),
            FieldValue::Ipv4("192.0.2.1".parse().unwrap())
        ),
        0,
        0,
        0
    )
    .is_err());
}
#[test]
fn options_byte_lengths_scope_namespace_and_padding_are_distinct_from_ipfix() {
    let b:Batch=serde_json::from_value(json!({"source_id":9,"templates":[{"id":258,"scope_count":1,"fields":[{"scope":"interface"},{"element":"sampling_interval"},{"element":"sampling_algorithm"}]}],"data_sets":[{"template_id":258,"records":[[{"kind":"unsigned","value":7},{"kind":"unsigned","value":100},{"kind":"unsigned","value":1}]]}]})).unwrap();
    let mut w = encode(&b, 1, 1, 1).unwrap().0;
    assert_eq!(&w[20..30], &[0, 1, 0, 24, 1, 2, 0, 4, 0, 8]);
    assert_eq!(&w[30..34], &[0, 2, 0, 4]);
    assert_eq!(w.len(), 60);
    w[42] = 0xa5;
    w[43] = 0xff;
    for v in &mut w[57..60] {
        *v = 0x7f;
    }
    let m = cache().ingest(peer(), &w, Instant::now()).unwrap();
    let t = &m.data_sets[0].template;
    assert_eq!(t.scope_count, 1);
    assert_eq!(t.fields[0].scope, Some(Scope::Interface));
    assert_eq!(t.fields[0].element, None);
    assert_eq!(m.data_sets[0].records[0][0], Some(FieldValue::Unsigned(7)));
    assert_eq!(m.header_count, 2);
    let mut malformed = w.clone();
    malformed[27] = 1;
    assert!(cache().ingest(peer(), &malformed, Instant::now()).is_err());
}
#[test]
fn full_u16_unknown_fields_have_no_enterprise_bit_or_raw_value() {
    let t = template(300, &[(0x8001, 4), (8, 4)]);
    let w = packet(
        42,
        0,
        &[(0, t), (300, vec![0xde, 0xad, 0xbe, 0xef, 192, 0, 2, 1])],
    );
    let m = cache().ingest(peer(), &w, Instant::now()).unwrap();
    assert_eq!(m.data_sets[0].template.fields[0].field_type, 0x8001);
    assert_eq!(m.data_sets[0].records[0][0], None);
    assert_eq!(
        m.data_sets[0].records[0][1],
        Some(FieldValue::Ipv4("192.0.2.1".parse().unwrap()))
    );
    assert!(!serde_json::to_string(&m).unwrap().contains("deadbeef"));
    let bad = packet(42, 0, &[(0, template(300, &[(1000, 65535)]))]);
    assert!(cache().ingest(peer(), &bad, Instant::now()).is_err());
}
#[test]
fn packet_sequence_wrap_late_gap_and_unknown_templates_keep_tracking() {
    let now = Instant::now();
    let mut c = cache();
    let t = packet(42, u32::MAX, &[(0, template(300, &[(8, 4)]))]);
    c.ingest(peer(), &t, now).unwrap();
    let m = c
        .ingest(peer(), &packet(42, 0, &[(300, vec![192, 0, 2, 1])]), now)
        .unwrap();
    assert_eq!(m.sequence_tracking.status, "in_order");
    let m = c
        .ingest(
            peer(),
            &packet_count(42, 3, 2, &[(300, vec![192, 0, 2, 1, 192, 0, 2, 2])]),
            now,
        )
        .unwrap();
    assert_eq!(m.record_count, 2);
    assert_eq!(m.sequence_tracking.missing_packets, Some(2));
    let m = c
        .ingest(peer(), &packet(42, 2, &[(300, vec![192, 0, 2, 3])]), now)
        .unwrap();
    assert_eq!(m.sequence_tracking.status, "out_of_order_or_duplicate");
    let m = c
        .ingest(peer(), &packet(42, 4, &[(999, vec![0; 8])]), now)
        .unwrap();
    assert_eq!(m.count_status, "unverifiable_unknown_template");
    assert_eq!(m.sequence_tracking.status, "in_order");
    let m = c
        .ingest(peer(), &packet(42, 5, &[(300, vec![192, 0, 2, 1])]), now)
        .unwrap();
    assert_eq!(m.sequence_tracking.expected, Some(5));
    assert_eq!(m.sequence_tracking.status, "in_order");
}
#[test]
fn source_ip_and_source_id_isolate_templates_but_udp_port_does_not() {
    let now = Instant::now();
    let mut c = cache();
    c.ingest(
        peer(),
        &packet(42, 0, &[(0, template(300, &[(8, 4)]))]),
        now,
    )
    .unwrap();
    let d = packet(42, 1, &[(300, vec![192, 0, 2, 1])]);
    assert_eq!(
        c.ingest("127.0.0.1:9999".parse().unwrap(), &d, now)
            .unwrap()
            .record_count,
        1
    );
    assert_eq!(
        c.ingest("127.0.0.2:9999".parse().unwrap(), &d, now)
            .unwrap()
            .unknown_data_sets
            .len(),
        1
    );
    assert_eq!(
        c.ingest(peer(), &packet(43, 0, &[(300, vec![192, 0, 2, 1])]), now)
            .unwrap()
            .unknown_data_sets
            .len(),
        1
    );
    assert_eq!(c.counts(), (3, 1));
}
#[test]
fn malformed_count_truncation_reserved_ids_and_redefinitions_are_transactional() {
    let now = Instant::now();
    let mut c = cache();
    let good = packet(42, 0, &[(0, template(300, &[(8, 4)]))]);
    c.ingest(peer(), &good, now).unwrap();
    let redefine = packet(
        42,
        1,
        &[(0, template(300, &[(12, 4)])), (300, vec![198, 51, 100, 2])],
    );
    for mut b in [
        redefine.clone(),
        packet(42, 1, &[(2, vec![0; 4])]),
        packet(42, 1, &[(0, template(300, &[]))]),
        packet(42, 1, &[(0, template(300, &[(8, 3)]))]),
    ] {
        if b == redefine {
            b[3] = 1;
        }
        assert!(c.ingest(peer(), &b, now).is_err());
    }
    for len in 0..redefine.len() {
        assert!(c.ingest(peer(), &redefine[..len], now).is_err());
    }
    let m = c
        .ingest(peer(), &packet(42, 1, &[(300, vec![192, 0, 2, 1])]), now)
        .unwrap();
    assert_eq!(m.sequence_tracking.status, "in_order");
    assert_eq!(
        m.data_sets[0].template.fields[0].element,
        Some(Element::SourceIpv4)
    );
    let m = c.ingest(peer(), &redefine, now).unwrap();
    assert_eq!(m.template_changes[0].change, "replaced");
    assert_eq!(
        m.data_sets[0].template.fields[0].element,
        Some(Element::DestinationIpv4)
    );
}
#[test]
fn refresh_expiry_clock_regression_and_ambiguous_uptime_decrease_are_explicit() {
    let now = Instant::now();
    let mut c = cache();
    let t = packet(42, 0, &[(0, template(300, &[(8, 4)]))]);
    c.ingest(peer(), &t, now).unwrap();
    let m = c
        .ingest(
            peer(),
            &packet(42, 1, &[(0, template(300, &[(8, 4)]))]),
            now + Duration::from_secs(9),
        )
        .unwrap();
    assert!(m.template_changes.is_empty());
    c.expire(now + Duration::from_secs(11));
    assert_eq!(c.counts(), (1, 1));
    c.expire(now + Duration::from_secs(20));
    assert_eq!(c.counts(), (1, 0));
    c.expire(now + Duration::from_secs(40));
    assert_eq!(c.counts(), (0, 0));
    let mut c = cache();
    c.ingest(peer(), &t, now).unwrap();
    let mut d = packet(42, 1, &[(300, vec![192, 0, 2, 1])]);
    d[4..8].copy_from_slice(&999u32.to_be_bytes());
    let m = c.ingest(peer(), &d, now).unwrap();
    assert!(m.sequence_tracking.uptime_decreased);
    assert_eq!(m.record_count, 1);
    assert!(m.sequence_tracking.cache_reset.is_none());
    d[12..16].copy_from_slice(&2u32.to_be_bytes());
    d[8..12].copy_from_slice(&1699999999u32.to_be_bytes());
    let m = c.ingest(peer(), &d, now).unwrap();
    assert_eq!(m.sequence_tracking.cache_reset, Some("clock_regression"));
    assert_eq!(m.unknown_data_sets.len(), 1);
    assert_eq!(c.counts(), (1, 0));
}
#[test]
fn template_session_global_and_message_bounds_fail_without_partial_state() {
    let now = Instant::now();
    let mut c = cache();
    let mut b = sample();
    b.data_sets.clear();
    b.templates = (256..288)
        .map(|id| {
            let mut t = b.templates[0].clone();
            t.id = id;
            t
        })
        .collect();
    for source in 0..32 {
        b.source_id = source;
        let w = encode(&b, 0, 0, 0).unwrap().0;
        c.ingest(peer(), &w, now).unwrap();
    }
    assert_eq!(c.counts(), (32, 1024));
    let mut extra = sample();
    extra.source_id = 32;
    assert!(c
        .ingest(peer(), &encode(&extra, 0, 0, 0).unwrap().0, now)
        .is_err());
    assert_eq!(c.counts(), (32, 1024));
    let mut c = cache();
    for source in 0..128 {
        let w = packet(source, 0, &[(999, vec![0; 4])]);
        c.ingest(peer(), &w, now).unwrap();
    }
    assert!(c
        .ingest(peer(), &packet(128, 0, &[(999, vec![0; 4])]), now)
        .is_err());
    assert_eq!(c.counts(), (128, 0));
    let mut b = sample();
    b.data_sets[0].records = vec![b.data_sets[0].records[0].clone(); 257];
    assert!(encode(&b, 0, 0, 0).is_err());
    b = sample();
    b.data_sets = vec![b.data_sets[0].clone(); 64];
    assert!(encode(&b, 0, 0, 0).is_err());
    b = sample();
    b.templates[0].fields = vec![b.templates[0].fields[0].clone(); 33];
    assert!(encode(&b, 0, 0, 0).is_err());
    assert!(cache().ingest(peer(), &vec![0; 8193], now).is_err());
    let mut c = cache();
    let bad = packet(0, 0, &[(0, template(300, &[(1000, 1025)]))]);
    assert!(c.ingest(peer(), &bad, now).is_err());
    assert_eq!(c.counts(), (0, 0));
}
#[test]
fn selected_short_record_ambiguity_and_wrong_action_schema_are_rejected() {
    let b: Batch = serde_json::from_value(
        json!({"source_id":0,"templates":[{"id":256,"fields":[{"element":"protocol"}]}]}),
    )
    .unwrap();
    assert!(encode(&b, 0, 0, 0).is_err());
    for field in [
        json!({"element":"source_ipv4","scope":"interface"}),
        json!({"scope":"interface"}),
        json!({"element":"source_ipv4","length":65535}),
    ] {
        let mut b = netget::client::netflow_v9::actions::example_batch();
        b["templates"][0]["fields"][0] = field;
        let b: Batch = serde_json::from_value(b).unwrap();
        assert!(encode(&b, 0, 0, 0).is_err());
    }
    let mut b = netget::client::netflow_v9::actions::example_batch();
    b["enterprise_number"] = json!(9);
    assert!(serde_json::from_value::<Batch>(b).is_err());
}

#[test]
fn all_options_scope_names_and_unsigned_widths_have_literal_type_values() {
    for (scope, id) in [
        (Scope::System, 1u16),
        (Scope::Interface, 2),
        (Scope::LineCard, 3),
        (Scope::Cache, 4),
        (Scope::Template, 5),
    ] {
        for width in 1..=8 {
            let max = if width == 8 {
                u64::MAX
            } else {
                (1u64 << (width * 8)) - 1
            };
            let batch: Batch=serde_json::from_value(json!({"source_id":7,"templates":[{"id":258,"scope_count":1,"fields":[{"scope":scope,"length":width},{"element":"sampling_interval"}]}],"data_sets":[{"template_id":258,"records":[[{"kind":"unsigned","value":max},{"kind":"unsigned","value":100}]]}]})).unwrap();
            let wire = encode(&batch, 0, 0, 0).unwrap().0;
            assert_eq!(&wire[30..32], &id.to_be_bytes());
            assert_eq!(&wire[32..34], &(width as u16).to_be_bytes());
            let m = cache().ingest(peer(), &wire, Instant::now()).unwrap();
            assert_eq!(m.data_sets[0].template.fields[0].scope, Some(scope));
            assert_eq!(
                m.data_sets[0].records[0][0],
                Some(FieldValue::Unsigned(max))
            );
        }
    }
}

#[test]
fn count_must_cover_each_unknown_data_flowset_without_partial_template_commit() {
    let now = Instant::now();
    let mut c = cache();
    let bad = packet_count(
        42,
        0,
        2,
        &[
            (0, template(300, &[(8, 4)])),
            (400, vec![0; 4]),
            (401, vec![0; 4]),
        ],
    );
    assert!(c.ingest(peer(), &bad, now).is_err());
    assert_eq!(c.counts(), (0, 0));
    let valid = packet_count(
        42,
        0,
        3,
        &[
            (0, template(300, &[(8, 4)])),
            (400, vec![0; 4]),
            (401, vec![0; 4]),
        ],
    );
    let m = c.ingest(peer(), &valid, now).unwrap();
    assert_eq!(m.count_status, "unverifiable_unknown_template");
    assert_eq!(m.known_total_record_count, 1);
    assert_eq!(m.unknown_data_sets.len(), 2);
    assert_eq!(c.counts(), (1, 1));
}
