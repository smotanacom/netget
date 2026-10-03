use netget::server::ipfix::codec::*;
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::time::Instant;
pub fn peer() -> SocketAddr {
    "127.0.0.1:47390".parse().unwrap()
}
pub fn cache() -> TemplateCache {
    TemplateCache::new(Duration::from_secs(10), Duration::from_secs(30))
}
pub fn sample() -> Batch {
    serde_json::from_value(netget::client::ipfix::actions::example_batch()).unwrap()
}
pub fn packet(domain: u32, sequence: u32, sets: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut b = vec![0, 10, 0, 0];
    b.extend_from_slice(&1700000000u32.to_be_bytes());
    b.extend_from_slice(&sequence.to_be_bytes());
    b.extend_from_slice(&domain.to_be_bytes());
    for (id, body) in sets {
        b.extend_from_slice(&id.to_be_bytes());
        b.extend_from_slice(&((body.len() + 4) as u16).to_be_bytes());
        b.extend_from_slice(body);
    }
    let n = b.len() as u16;
    b[2..4].copy_from_slice(&n.to_be_bytes());
    b
}
pub fn template(id: u16, fields: &[(u16, u16)]) -> Vec<u8> {
    let mut b = id.to_be_bytes().to_vec();
    b.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    for (i, n) in fields {
        b.extend_from_slice(&i.to_be_bytes());
        b.extend_from_slice(&n.to_be_bytes());
    }
    b
}
fn one(element: Element, length: Option<u16>, value: FieldValue) -> Batch {
    Batch {
        observation_domain_id: 42,
        export_time: Some(1700000000),
        templates: vec![Template {
            id: 256,
            scope_count: 0,
            fields: vec![Field { element, length }],
        }],
        data_sets: vec![DataSet {
            template_id: 256,
            records: vec![vec![value]],
        }],
    }
}
#[test]
fn independent_literal_golden_asserts_both_directions() {
    let golden = vec![
        0, 10, 0, 56, 0x65, 0x53, 0xf1, 0, 0, 0, 0, 0, 0, 0, 0, 42, 0, 2, 0, 20, 1, 0, 0, 3, 0, 8,
        0, 4, 0, 12, 0, 4, 0, 2, 0, 8, 1, 0, 0, 20, 192, 0, 2, 1, 198, 51, 100, 2, 0, 0, 0, 0, 0,
        0, 0, 5,
    ];
    assert_eq!(encode(&sample(), 0, 1700000000).unwrap().0, golden);
    let m = cache().ingest(peer(), &golden, Instant::now()).unwrap();
    assert_eq!(m.record_count, 1);
    assert_eq!(m.observation_domain_id, 42);
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
fn unsigned_reduction_all_widths_and_timestamp_address_extremes() {
    for n in 1..=8 {
        let max = if n == 8 {
            u64::MAX
        } else {
            (1u64 << (n * 8)) - 1
        };
        let b = one(Element::OctetDeltaCount, Some(n), FieldValue::Unsigned(max));
        let wire = encode(&b, 0, 0).unwrap().0;
        assert_eq!(
            cache()
                .ingest(peer(), &wire, Instant::now())
                .unwrap()
                .data_sets[0]
                .records[0][0],
            Some(FieldValue::Unsigned(max))
        );
        if n < 8 {
            assert!(encode(
                &one(
                    Element::OctetDeltaCount,
                    Some(n),
                    FieldValue::Unsigned(max + 1)
                ),
                0,
                0
            )
            .is_err());
        }
    }
    for (e, v) in [
        (
            Element::FlowStartSeconds,
            FieldValue::TimestampSeconds(u32::MAX),
        ),
        (
            Element::FlowEndMilliseconds,
            FieldValue::TimestampMilliseconds(u64::MAX),
        ),
        (
            Element::SourceIpv6Address,
            FieldValue::Ipv6("2001:db8::abcd".parse().unwrap()),
        ),
    ] {
        let b = one(e, None, v.clone());
        let w = encode(&b, 0, 0).unwrap().0;
        assert_eq!(
            cache()
                .ingest(peer(), &w, Instant::now())
                .unwrap()
                .data_sets[0]
                .records[0][0],
            Some(v)
        );
        assert!(encode(&one(e, Some(1), FieldValue::Unsigned(1)), 0, 0).is_err());
    }
    let b = one(
        Element::TcpControlBits,
        Some(2),
        FieldValue::Unsigned(0x1ff),
    );
    let mut w = encode(&b, 0, 0).unwrap().0;
    let n = w.len();
    w[n - 2] = 0xf1;
    assert_eq!(
        cache()
            .ingest(peer(), &w, Instant::now())
            .unwrap()
            .data_sets[0]
            .records[0][0],
        Some(FieldValue::Unsigned(0x1ff))
    );
    assert!(encode(
        &one(Element::TcpControlBits, None, FieldValue::Unsigned(0xf001)),
        0,
        0
    )
    .is_err());
}
#[test]
fn variable_prefix_unicode_and_fixed_strings_preserve_nuls() {
    for len in [0, 254, 255, 1024] {
        let s = "x".repeat(len);
        let w = encode(
            &one(Element::InterfaceName, None, FieldValue::String(s.clone())),
            0,
            0,
        )
        .unwrap()
        .0;
        assert_eq!(w[32], if len < 255 { len as u8 } else { 255 });
        assert_eq!(
            cache()
                .ingest(peer(), &w, Instant::now())
                .unwrap()
                .data_sets[0]
                .records[0][0],
            Some(FieldValue::String(s))
        );
    }
    for length in [None, Some(4)] {
        let s = "名\0".to_string();
        let w = encode(
            &one(
                Element::InterfaceName,
                length,
                FieldValue::String(s.clone()),
            ),
            0,
            0,
        )
        .unwrap()
        .0;
        assert_eq!(
            cache()
                .ingest(peer(), &w, Instant::now())
                .unwrap()
                .data_sets[0]
                .records[0][0],
            Some(FieldValue::String(s))
        );
    }
    assert!(encode(
        &one(
            Element::InterfaceName,
            None,
            FieldValue::String("x".repeat(1025))
        ),
        0,
        0
    )
    .is_err());
    assert!(encode(
        &one(
            Element::InterfaceName,
            Some(5),
            FieldValue::String("abc".into())
        ),
        0,
        0
    )
    .is_err());
    let mut w = encode(
        &one(Element::InterfaceName, None, FieldValue::String("x".into())),
        0,
        0,
    )
    .unwrap()
    .0;
    *w.last_mut().unwrap() = 255;
    assert!(cache().ingest(peer(), &w, Instant::now()).is_err());
}
#[test]
fn options_scope_duplicate_elements_and_nonzero_padding() {
    let mut b = sample();
    b.templates[0].scope_count = 1;
    b.templates[0].fields[1].element = Element::SourceIpv4Address;
    let w = encode(&b, 0, 0).unwrap().0;
    let m = cache().ingest(peer(), &w, Instant::now()).unwrap();
    assert_eq!(m.data_sets[0].template.scope_count, 1);
    assert_eq!(
        m.data_sets[0].template.fields[0].element,
        m.data_sets[0].template.fields[1].element
    );
    assert_ne!(m.data_sets[0].records[0][0], m.data_sets[0].records[0][1]);
    let w = packet(
        1,
        0,
        &[
            (2, template(300, &[(8, 4)])),
            (300, vec![192, 0, 2, 1, 0xaa, 0xbb, 0xcc]),
        ],
    );
    assert_eq!(
        cache()
            .ingest(peer(), &w, Instant::now())
            .unwrap()
            .record_count,
        1
    );
    let mut t = template(300, &[(8, 4)]);
    t.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
    assert_eq!(
        cache()
            .ingest(peer(), &packet(1, 0, &[(2, t)]), Instant::now())
            .unwrap()
            .template_changes
            .len(),
        1
    );
}
#[test]
fn unknown_enterprise_and_standard_values_are_descriptors_without_raw_bytes() {
    let mut t = template(300, &[(8, 4)]);
    t[2..4].copy_from_slice(&3u16.to_be_bytes());
    t.extend_from_slice(&[0x83, 0xe7, 0, 2, 0, 0, 0, 123]);
    t.extend_from_slice(&[3, 0xe8, 0xff, 0xff]);
    let w = packet(
        42,
        0,
        &[
            (2, t),
            (300, vec![192, 0, 2, 1, 255, 255, 3, 254, 253, 252]),
        ],
    );
    let m = cache().ingest(peer(), &w, Instant::now()).unwrap();
    assert_eq!(m.data_sets[0].template.fields[1].enterprise, Some(123));
    assert_eq!(
        m.data_sets[0].records[0],
        vec![
            Some(FieldValue::Ipv4("192.0.2.1".parse().unwrap())),
            None,
            None
        ]
    );
    let j = serde_json::to_value(m).unwrap();
    assert!(!j.to_string().contains("base64"));
    assert_eq!(j["data_sets"][0]["records"][0][2], json!(null));
}
#[test]
fn sequence_wrap_gaps_late_messages_and_unknown_templates() {
    let now = Instant::now();
    let mut c = cache();
    let w = encode(&sample(), u32::MAX, 0).unwrap().0;
    c.ingest(peer(), &w, now).unwrap();
    let m = c
        .ingest(peer(), &encode(&sample(), 0, 0).unwrap().0, now)
        .unwrap();
    assert_eq!(m.sequence_tracking.expected, Some(0));
    assert_eq!(m.sequence_tracking.status, "in_order");
    let m = c
        .ingest(peer(), &encode(&sample(), 20, 0).unwrap().0, now)
        .unwrap();
    assert_eq!(m.sequence_tracking.missing_records, Some(19));
    let m = c
        .ingest(peer(), &encode(&sample(), 1, 0).unwrap().0, now)
        .unwrap();
    assert_eq!(m.sequence_tracking.status, "out_of_order_or_duplicate");
    let m = c
        .ingest(peer(), &encode(&sample(), 21, 0).unwrap().0, now)
        .unwrap();
    assert_eq!(m.sequence_tracking.status, "in_order");
    let m = c
        .ingest(peer(), &packet(42, 22, &[(500, vec![1, 2, 3])]), now)
        .unwrap();
    assert_eq!(m.unknown_data_sets.len(), 1);
    let m = c
        .ingest(peer(), &encode(&sample(), 100, 0).unwrap().0, now)
        .unwrap();
    assert_eq!(m.sequence_tracking.expected, None);
}
#[test]
fn separate_messages_refresh_replace_withdrawals_and_exact_expiry() {
    let now = Instant::now();
    let mut c = cache();
    let tpl = packet(1, 0, &[(2, template(300, &[(8, 4)]))]);
    c.ingest(peer(), &tpl, now).unwrap();
    let d = packet(1, 0, &[(300, vec![192, 0, 2, 1])]);
    assert_eq!(
        c.ingest(peer(), &d, now + Duration::from_secs(8))
            .unwrap()
            .record_count,
        1
    );
    let m = c
        .ingest(peer(), &tpl, now + Duration::from_secs(9))
        .unwrap();
    assert!(m.template_changes.is_empty());
    let withdrawal = packet(1, 1, &[(2, vec![1, 44, 0, 0]), (300, vec![192, 0, 2, 2])]);
    let m = c
        .ingest(peer(), &withdrawal, now + Duration::from_secs(10))
        .unwrap();
    assert_eq!(m.ignored_withdrawals, vec![300]);
    assert_eq!(m.record_count, 1);
    assert_eq!(
        c.ingest(peer(), &d, now + Duration::from_secs(18))
            .unwrap()
            .record_count,
        1
    );
    assert_eq!(
        c.ingest(peer(), &d, now + Duration::from_secs(19))
            .unwrap()
            .unknown_data_sets
            .len(),
        1
    );
    let replace = packet(1, 2, &[(2, template(300, &[(7, 2)])), (300, vec![1, 187])]);
    let m = c
        .ingest(peer(), &replace, now + Duration::from_secs(20))
        .unwrap();
    assert_eq!(
        m.data_sets[0].records[0][0],
        Some(FieldValue::Unsigned(443))
    );
    let m = c
        .ingest(
            peer(),
            &packet(1, 3, &[(2, template(300, &[(8, 4)]))]),
            now + Duration::from_secs(21),
        )
        .unwrap();
    assert_eq!(m.template_changes[0].change, "replaced");
    c.expire(now + Duration::from_secs(51));
    assert_eq!(c.counts(), (0, 0));
}
#[test]
fn peer_ports_domains_and_malformed_atomic_state_are_isolated() {
    let now = Instant::now();
    let mut c = cache();
    c.ingest(peer(), &packet(1, 0, &[(2, template(300, &[(8, 4)]))]), now)
        .unwrap();
    let mut bad = packet(1, 0, &[(2, template(300, &[(7, 2)]))]);
    bad.extend_from_slice(&[1, 44, 0, 9, 0]);
    let n = bad.len() as u16;
    bad[2..4].copy_from_slice(&n.to_be_bytes());
    assert!(c.ingest(peer(), &bad, now).is_err());
    let m = c
        .ingest(peer(), &packet(1, 0, &[(300, vec![192, 0, 2, 1])]), now)
        .unwrap();
    assert_eq!(
        m.data_sets[0].records[0][0],
        Some(FieldValue::Ipv4("192.0.2.1".parse().unwrap()))
    );
    assert_eq!(m.sequence_tracking.status, "in_order");
    assert_eq!(
        c.ingest(peer(), &packet(2, 0, &[(300, vec![192, 0, 2, 1])]), now)
            .unwrap()
            .unknown_data_sets
            .len(),
        1
    );
    assert_eq!(
        c.ingest(
            "127.0.0.1:47391".parse().unwrap(),
            &packet(1, 0, &[(300, vec![192, 0, 2, 1])]),
            now
        )
        .unwrap()
        .unknown_data_sets
        .len(),
        1
    );
}
#[test]
fn malformed_header_sets_templates_variable_lengths_and_kinds_fail() {
    let w = encode(&sample(), 0, 0).unwrap().0;
    for n in 0..w.len() {
        let mut b = w[..n].to_vec();
        if n >= 4 {
            b[2..4].copy_from_slice(&(n as u16).to_be_bytes());
        }
        if [16, 36].contains(&n) {
            continue;
        }
        assert!(
            cache().ingest(peer(), &b, Instant::now()).is_err(),
            "truncation{n}"
        );
    }
    let mut b = w.clone();
    b[1] = 9;
    assert!(cache().ingest(peer(), &b, Instant::now()).is_err());
    let mut b = w.clone();
    b[3] -= 1;
    assert!(cache().ingest(peer(), &b, Instant::now()).is_err());
    for sets in [
        vec![(4, vec![])],
        vec![(2, template(255, &[(8, 4)]))],
        vec![(2, template(300, &[(8, 3)]))],
        vec![(3, vec![1, 44, 0, 1, 0, 0, 0, 8, 0, 4])],
        vec![
            (2, template(300, &[(82, 65535)])),
            (300, vec![255, 4, 1, 2]),
        ],
    ] {
        assert!(cache()
            .ingest(peer(), &packet(1, 0, &sets), Instant::now())
            .is_err());
    }
    let mut b = sample();
    b.data_sets[0].records[0][0] = FieldValue::Unsigned(123);
    assert!(encode(&b, 0, 0).is_err());
    b = sample();
    b.templates.push(b.templates[0].clone());
    assert!(encode(&b, 0, 0).is_err());
    b = sample();
    b.data_sets[0].template_id = 300;
    assert!(encode(&b, 0, 0).is_err());
}
#[test]
fn exact_message_set_record_field_and_scalar_bounds() {
    let mut b = one(
        Element::InterfaceName,
        None,
        FieldValue::String("x".repeat(1020)),
    );
    b.data_sets[0].records = vec![vec![FieldValue::String("x".repeat(1020))]; 7];
    b.data_sets[0]
        .records
        .push(vec![FieldValue::String("x".repeat(996))]);
    let w = encode(&b, 0, 0).unwrap().0;
    assert_eq!(w.len(), 8192);
    assert_eq!(
        cache()
            .ingest(peer(), &w, Instant::now())
            .unwrap()
            .record_count,
        8
    );
    b.data_sets[0].records.last_mut().unwrap()[0] = FieldValue::String("x".repeat(997));
    assert!(encode(&b, 0, 0).is_err());
    let mut w2 = w.clone();
    w2.push(0);
    assert!(cache().ingest(peer(), &w2, Instant::now()).is_err());
    let mut b = one(Element::ProtocolIdentifier, None, FieldValue::Unsigned(6));
    b.data_sets[0].records = vec![vec![FieldValue::Unsigned(6)]; 256];
    assert!(encode(&b, 0, 0).is_ok());
    b.data_sets[0].records.push(vec![FieldValue::Unsigned(6)]);
    assert!(encode(&b, 0, 0).is_err());
    b = one(Element::ProtocolIdentifier, None, FieldValue::Unsigned(6));
    b.data_sets = vec![b.data_sets[0].clone(); 63];
    assert!(encode(&b, 0, 0).is_ok());
    b.data_sets.push(b.data_sets[0].clone());
    assert!(encode(&b, 0, 0).is_err());
    b = one(Element::ProtocolIdentifier, None, FieldValue::Unsigned(6));
    b.templates[0].fields = vec![b.templates[0].fields[0].clone(); 32];
    b.data_sets[0].records[0] = vec![FieldValue::Unsigned(6); 32];
    assert!(encode(&b, 0, 0).is_ok());
    let extra_field = b.templates[0].fields[0].clone();
    b.templates[0].fields.push(extra_field);
    assert!(encode(&b, 0, 0).is_err());
}
#[test]
fn session_per_session_template_and_global_template_caps_recover_on_expiry() {
    let now = Instant::now();
    let mut c = cache();
    for domain in 0..128 {
        c.ingest(peer(), &packet(domain, 0, &[]), now).unwrap();
    }
    assert_eq!(c.counts(), (128, 0));
    assert!(c.ingest(peer(), &packet(128, 0, &[]), now).is_err());
    c.expire(now + Duration::from_secs(30));
    assert_eq!(c.counts(), (0, 0));
    let mut b = sample();
    b.data_sets.clear();
    b.templates = (256..288)
        .map(|id| Template {
            id,
            scope_count: 0,
            fields: vec![Field {
                element: Element::SourceIpv4Address,
                length: None,
            }],
        })
        .collect();
    for domain in 0..32 {
        b.observation_domain_id = domain;
        c.ingest(peer(), &encode(&b, 0, 0).unwrap().0, now).unwrap();
    }
    assert_eq!(c.counts(), (32, 1024));
    let mut extra = sample();
    extra.observation_domain_id = 32;
    assert!(c
        .ingest(peer(), &encode(&extra, 0, 0).unwrap().0, now)
        .is_err());
    assert_eq!(c.counts(), (32, 1024));
    extra.observation_domain_id = 0;
    extra.templates[0].id = 300;
    extra.data_sets[0].template_id = 300;
    assert!(c
        .ingest(peer(), &encode(&extra, 0, 0).unwrap().0, now)
        .is_err());
    c.expire(now + Duration::from_secs(10));
    assert_eq!(c.counts(), (32, 0));
    assert!(c
        .ingest(
            peer(),
            &encode(&extra, 0, 0).unwrap().0,
            now + Duration::from_secs(10)
        )
        .is_ok());
}
