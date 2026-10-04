//! The DICOM dataset codec and C-FIND matching against bytes and rules written from PS3.5 and
//! PS3.4 (C.2.2.2), independent of any peer.
use netget::server::dicom::dataset::{decode, encode, matches, project, EXPLICIT_LE, IMPLICIT_LE};
use serde_json::{json, Map, Value};

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

#[test]
fn explicit_and_implicit_little_endian_bytes() {
    let ds = obj(
        json!({"00100020": {"vr": "LO", "Value": ["P1"]}, "00280010": {"vr": "US", "Value": [2]}}),
    );
    // (0010,0020) LO len 2 "P1"; (0028,0010) US len 2 = 2 — PS3.5 7.1.2 short form.
    assert_eq!(
        encode(&ds, EXPLICIT_LE).unwrap(),
        [
            &[0x10, 0, 0x20, 0, b'L', b'O', 2, 0, b'P', b'1'][..],
            &[0x28, 0, 0x10, 0, b'U', b'S', 2, 0, 2, 0]
        ]
        .concat()
    );
    // Implicit VR: tag, 4-byte length, value; VR from the dictionary on decode.
    let implicit = [
        &[0x10, 0, 0x20, 0, 2, 0, 0, 0, b'P', b'1'][..],
        &[0x28, 0, 0x10, 0, 2, 0, 0, 0, 2, 0],
    ]
    .concat();
    assert_eq!(encode(&ds, IMPLICIT_LE).unwrap(), implicit);
    assert_eq!(decode(&implicit, IMPLICIT_LE).unwrap(), ds);
}

#[test]
fn person_names_sequences_and_bulk_data_round_trip() {
    let ds = obj(json!({
        "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
        "00081110": {"vr": "SQ", "Value": [{"00081150": {"vr": "UI", "Value": ["1.2.3"]}}]},
        "00200013": {"vr": "IS", "Value": [7]},
    }));
    for ts in [EXPLICIT_LE, IMPLICIT_LE] {
        assert_eq!(decode(&encode(&ds, ts).unwrap(), ts).unwrap(), ds, "{ts}");
    }
    let bulk = obj(json!({"7FE00010": {"vr": "OB", "hex": "00010203"}}));
    let back = decode(&encode(&bulk, EXPLICIT_LE).unwrap(), EXPLICIT_LE).unwrap();
    assert_eq!(back["7FE00010"], json!({"vr": "OB", "length": 4}));
}

#[test]
fn hostile_lengths_are_refused() {
    // A declared length past the end of the buffer, and an undefined-length sequence that never ends.
    assert!(decode(
        &[0x10, 0, 0x20, 0, b'L', b'O', 0xFF, 0x7F, b'P'],
        EXPLICIT_LE
    )
    .is_err());
    assert!(decode(
        &[0x08, 0, 0x10, 0x11, b'S', b'Q', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF],
        EXPLICIT_LE
    )
    .is_err());
    // A depth bomb: nested undefined-length sequences.
    let mut bomb = Vec::new();
    for _ in 0..200 {
        bomb.extend_from_slice(&[
            0x08, 0, 0x10, 0x11, b'S', b'Q', 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ]);
        bomb.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0xFF, 0xFF, 0xFF, 0xFF]);
    }
    assert!(decode(&bomb, EXPLICIT_LE).is_err());
}

#[test]
fn c_find_matching_rules() {
    let rec = obj(json!({
        "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
        "00100020": {"vr": "LO", "Value": ["P001"]},
        "00080020": {"vr": "DA", "Value": ["20261002"]},
        "0020000D": {"vr": "UI", "Value": ["1.2.3"]},
        "00080060": {"vr": "CS", "Value": ["CT"]},
    }));
    let q = |v: Value| obj(v);
    assert!(
        matches(&q(json!({"00100010": {"vr": "PN"}})), &rec),
        "universal"
    );
    assert!(matches(
        &q(json!({"00100010": {"vr": "PN", "Value": [{"Alphabetic": "Do?^*"}]}})),
        &rec
    ));
    assert!(!matches(
        &q(json!({"00100010": {"vr": "PN", "Value": [{"Alphabetic": "Roe*"}]}})),
        &rec
    ));
    assert!(matches(
        &q(json!({"00080020": {"vr": "DA", "Value": ["20261001-20261003"]}})),
        &rec
    ));
    assert!(matches(
        &q(json!({"00080020": {"vr": "DA", "Value": ["-20261002"]}})),
        &rec
    ));
    assert!(!matches(
        &q(json!({"00080020": {"vr": "DA", "Value": ["20261003-"]}})),
        &rec
    ));
    assert!(
        matches(
            &q(json!({"0020000D": {"vr": "UI", "Value": ["9.9", "1.2.3"]}})),
            &rec
        ),
        "UID list"
    );
    assert!(!matches(
        &q(json!({"00100020": {"vr": "LO", "Value": ["P002"]}})),
        &rec
    ));
    let projected = project(
        &q(json!({"00100010": {"vr": "PN"}, "00080052": {"vr": "CS", "Value": ["STUDY"]}})),
        &rec,
    );
    assert_eq!(projected["00100010"], rec["00100010"]);
    assert!(
        !projected.contains_key("00100020"),
        "only requested keys return"
    );
}
