//! RFC 8210/6810 PDUs against literal bytes from the specifications' layouts.
use netget::server::rpki_rtr::codec::{self, Batch, Intervals, Packet, Pdu, Record};

fn record(prefix: &str, max: u8, asn: u32, announcement: bool) -> Record {
    Record {
        prefix: prefix.into(),
        max_length: max,
        asn,
        announcement,
    }
}

#[test]
fn every_pdu_matches_its_literal_layout_and_round_trips() {
    let cases: Vec<(Packet, Vec<u8>)> = vec![
        (
            Packet {
                version: 1,
                pdu: Pdu::SerialNotify {
                    session: 0x1234,
                    serial: 7,
                },
            },
            vec![1, 0, 0x12, 0x34, 0, 0, 0, 12, 0, 0, 0, 7],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::SerialQuery {
                    session: 0x1234,
                    serial: 7,
                },
            },
            vec![1, 1, 0x12, 0x34, 0, 0, 0, 12, 0, 0, 0, 7],
        ),
        (
            Packet {
                version: 0,
                pdu: Pdu::ResetQuery,
            },
            vec![0, 2, 0, 0, 0, 0, 0, 8],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::CacheResponse { session: 9 },
            },
            vec![1, 3, 0, 9, 0, 0, 0, 8],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::Prefix(record("192.0.2.0/24", 24, 64496, true)),
            },
            vec![
                1, 4, 0, 0, 0, 0, 0, 20, 1, 24, 24, 0, 192, 0, 2, 0, 0, 0, 0xfb, 0xf0,
            ],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::Prefix(record("2001:db8::/32", 48, 64497, false)),
            },
            vec![
                1, 6, 0, 0, 0, 0, 0, 32, 0, 32, 48, 0, 0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0xfb, 0xf1,
            ],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::EndOfData {
                    session: 9,
                    serial: 7,
                    intervals: Intervals {
                        refresh: 3600,
                        retry: 600,
                        expire: 7200,
                    },
                },
            },
            vec![
                1, 7, 0, 9, 0, 0, 0, 24, 0, 0, 0, 7, 0, 0, 0x0e, 0x10, 0, 0, 0x02, 0x58, 0, 0,
                0x1c, 0x20,
            ],
        ),
        (
            Packet {
                version: 0,
                pdu: Pdu::EndOfData {
                    session: 9,
                    serial: 7,
                    intervals: Intervals::default(),
                },
            },
            vec![0, 7, 0, 9, 0, 0, 0, 12, 0, 0, 0, 7],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::CacheReset,
            },
            vec![1, 8, 0, 0, 0, 0, 0, 8],
        ),
        (
            Packet {
                version: 1,
                pdu: Pdu::ErrorReport {
                    code: 2,
                    erroneous: vec![1, 2, 0, 0, 0, 0, 0, 8],
                    diagnostic: "none".into(),
                },
            },
            vec![
                1, 10, 0, 2, 0, 0, 0, 28, 0, 0, 0, 8, 1, 2, 0, 0, 0, 0, 0, 8, 0, 0, 0, 4, b'n',
                b'o', b'n', b'e',
            ],
        ),
    ];
    for (packet, bytes) in cases {
        assert_eq!(packet.encode().unwrap(), bytes, "{packet:?}");
        assert_eq!(Packet::decode(&bytes).unwrap(), packet);
    }
}

#[test]
fn prefixes_with_host_bits_bad_lengths_or_full_length_are_judged_exactly() {
    // /32 and /128 once overflowed a shift; they are valid, host bits or not, by definition.
    for (prefix, max) in [
        ("192.0.2.1/32", 32),
        ("2001:db8::1/128", 128),
        ("0.0.0.0/0", 0),
        ("::/0", 128),
    ] {
        assert!(record(prefix, max, 1, true).parsed().is_ok(), "{prefix}");
    }
    for (prefix, max) in [
        ("192.0.2.1/24", 24),
        ("192.0.2.0/24", 23),
        ("192.0.2.0/24", 33),
        ("2001:db8::1/32", 48),
        ("2001:db8::/32", 129),
        ("192.0.2.0", 24),
        ("example/24", 24),
    ] {
        assert!(
            record(prefix, max, 1, true).parsed().is_err(),
            "{prefix} max {max}"
        );
    }
    let mut prefix_pdu = Packet {
        version: 1,
        pdu: Pdu::Prefix(record("192.0.2.0/24", 24, 1, true)),
    }
    .encode()
    .unwrap();
    prefix_pdu[15] = 1; // a host bit set on the wire
    assert!(Packet::decode(&prefix_pdu).is_err());
}

#[test]
fn lengths_versions_and_types_are_checked_before_trusting_a_pdu() {
    for bad in [
        vec![2, 2, 0, 0, 0, 0, 0, 8],              // version 2
        vec![1, 2, 0, 0, 0, 0, 0, 9, 0],           // reset query too long
        vec![1, 7, 0, 9, 0, 0, 0, 12, 0, 0, 0, 7], // version-1 End of Data without timers
        vec![
            0, 7, 0, 9, 0, 0, 0, 24, 0, 0, 0, 7, 0, 0, 0x0e, 0x10, 0, 0, 0x02, 0x58, 0, 0, 0x1c,
            0x20,
        ], // version 0 with timers
        vec![1, 11, 0, 0, 0, 0, 0, 8],             // unknown type
        vec![1, 2, 0, 0, 0, 0, 0, 7],              // declared length disagrees
        vec![1, 10, 0, 9, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0], // error code 9
        vec![1, 10, 0, 1, 0, 0, 0, 16, 0, 0, 0, 100, 0, 0, 0, 0], // encapsulated length past the PDU
    ] {
        assert!(Packet::decode(&bad).is_err(), "{bad:?}");
    }
    let timers = Intervals {
        refresh: 7200,
        retry: 600,
        expire: 7200,
    };
    assert!(timers.validate().is_err(), "expire must exceed refresh");
}

#[test]
fn serial_arithmetic_follows_rfc_1982_across_the_wrap() {
    assert!(codec::serial_newer(1, 0));
    assert!(codec::serial_newer(0, u32::MAX));
    assert!(codec::serial_newer(5, u32::MAX - 5));
    assert!(!codec::serial_newer(7, 7));
    assert!(!codec::serial_newer(6, 7));
    assert!(
        !codec::serial_newer(0x8000_0000, 0),
        "the half-space is ambiguous and never newer"
    );
}

#[test]
fn batches_refuse_reset_withdrawals_duplicates_and_oversize() {
    let ok = Batch {
        serial: 1,
        records: vec![record("192.0.2.0/24", 24, 1, true)],
    };
    assert!(ok.validate(true).is_ok());
    assert!(Batch {
        serial: 1,
        records: vec![record("192.0.2.0/24", 24, 1, false)]
    }
    .validate(true)
    .is_err());
    assert!(Batch {
        serial: 1,
        records: vec![
            record("192.0.2.0/24", 24, 1, true),
            record("192.0.2.0/24", 24, 1, true)
        ]
    }
    .validate(false)
    .is_err());
    let many = (0..=codec::MAX_RECORDS)
        .map(|i| record(&format!("10.{}.{}.0/24", i / 256, i % 256), 24, 1, true))
        .collect();
    assert!(Batch {
        serial: 1,
        records: many
    }
    .validate(false)
    .is_err());
}

#[tokio::test]
async fn a_frame_header_over_the_bound_is_refused_before_allocation() {
    let mut over: &[u8] = &[1, 2, 0, 0, 0, 0, 0x10, 0x01];
    assert!(
        codec::read_frame(&mut over, std::time::Duration::from_secs(1))
            .await
            .is_err()
    );
    let mut under: &[u8] = &[1, 2, 0, 0, 0, 0, 0, 7];
    assert!(
        codec::read_frame(&mut under, std::time::Duration::from_secs(1))
            .await
            .is_err()
    );
    let mut fragmented = tokio_test_reader(vec![vec![1, 2], vec![0, 0, 0], vec![0, 0, 8]]);
    assert_eq!(
        codec::read_frame(&mut fragmented, std::time::Duration::from_secs(1))
            .await
            .unwrap(),
        vec![1, 2, 0, 0, 0, 0, 0, 8]
    );
}

/// A reader that yields the PDU in pieces, as one TCP read per piece would.
fn tokio_test_reader(pieces: Vec<Vec<u8>>) -> impl tokio::io::AsyncRead + Unpin {
    let (mut a, b) = tokio::io::duplex(64);
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        for p in pieces {
            a.write_all(&p).await.unwrap();
            a.flush().await.unwrap();
            tokio::task::yield_now().await;
        }
    });
    b
}
