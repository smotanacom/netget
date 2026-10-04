use netget::server::diameter::codec::*;
use serde_json::{json, Value};
fn bytes(v: &str) -> Vec<u8> {
    v.as_bytes()
        .chunks_exact(2)
        .map(|x| u8::from_str_radix(std::str::from_utf8(x).unwrap(), 16).unwrap())
        .collect()
}
#[test]
fn captured_unmodified_python_wire_matches_literal_fields_and_native_codec() {
    let rows: Vec<Value> = serde_json::from_str(include_str!("peer_wire.json")).unwrap();
    assert_eq!(rows.len(), 12);
    for row in rows {
        let data = bytes(row["hex"].as_str().unwrap());
        assert_eq!(data[0], 1);
        assert_eq!(
            u32::from_be_bytes([0, data[1], data[2], data[3]]) as usize,
            data.len()
        );
        let p = Packet::decode(&data).unwrap();
        assert_eq!(p.encode().unwrap(), data);
        assert_eq!(p.command as u64, row["command"].as_u64().unwrap());
        assert_eq!(p.application as u64, row["application"].as_u64().unwrap());
        assert_eq!(p.is_request(), row["request"] == true);
        if p.command == AA {
            assert_eq!(data[4], if p.is_request() { 0xc0 } else { 0x40 });
            assert_eq!(p.num(AUTH_APP).unwrap(), 1);
            assert_eq!(p.num(AUTH_TYPE).unwrap(), 3);
            assert_eq!(p.num(AUTH_STATE).unwrap(), 1);
            assert_eq!(p.text(SESSION, 512).unwrap(), "client.example;test;1");
            if p.is_request() {
                assert_eq!(p.text(USER, 255).unwrap(), "alice");
                assert!(["Correct", "wrong"]
                    .contains(&p.text(PASSWORD, MAX_PASSWORD_BYTES).unwrap().as_str()));
            } else {
                assert!([2001, 4001].contains(&p.num(RESULT).unwrap()));
            }
        } else {
            assert!([CER, DWR, DPR].contains(&p.command));
            assert_eq!(p.application, 0);
        }
    }
}
#[test]
fn header_avp_bounds_reject_malformed_lengths_before_copy() {
    let mut p = Packet::request(DWR, 0).unwrap();
    p.origin(&Identity {
        host: "node.example".into(),
        realm: "example".into(),
    });
    let good = p.encode().unwrap();
    for offset in [0, 1, 2, 3, 25, 26, 27] {
        let mut bad = good.clone();
        bad[offset] = 0xff;
        assert!(Packet::decode(&bad).is_err(), "offset{offset}");
    }
    let mut too_many = p.clone();
    too_many.avps = vec![Avp::number(RESULT, 2001); MAX_AVPS + 1];
    assert!(too_many.encode().is_err());
    let mut huge = p;
    huge.avps = vec![Avp::bytes(1, vec![0; MAX_FRAME_BYTES])];
    assert!(huge.encode().is_err());
    huge.avps = vec![Avp::bytes(1, vec![0; MAX_FRAME_BYTES / 2]); 2];
    assert!(huge
        .encode()
        .unwrap_err()
        .to_string()
        .contains("aggregate message size before copy"));
    assert!(Packet::decode(&vec![0; MAX_FRAME_BYTES + 4]).is_err());
}
#[test]
fn response_requires_stateless_agreement_and_all_correlation_fields() {
    let id = Identity {
        host: "server.example".into(),
        realm: "example".into(),
    };
    let client = Identity {
        host: "client.example".into(),
        realm: "example".into(),
    };
    let r: Request =
        serde_json::from_value(json!({"username":"alice","password":"Correct"})).unwrap();
    let p = r.packet(&client, &id, "client.example;one").unwrap();
    let a = Reply {
        verdict: Verdict::Accept,
        ..Default::default()
    }
    .packet(&p, &id)
    .unwrap();
    assert!(Response::from_packet(&a, &p, &id).unwrap().accepted);
    for mutation in 0..11 {
        let mut bad = a.clone();
        match mutation {
            0 => bad.hop ^= 1,
            1 => bad.end ^= 1,
            2 => bad.application = 0,
            3 => bad.command = DWR,
            4 => bad.flags |= 0x80,
            5 => bad.avps.retain(|v| v.code != AUTH_STATE),
            6 => {
                bad.avps
                    .iter_mut()
                    .find(|v| v.code == AUTH_STATE)
                    .unwrap()
                    .data = 0u32.to_be_bytes().to_vec()
            }
            7 => bad.avps.push(Avp::number(RESULT, 2001)),
            8 => bad.avps.push(Avp::number(999999, 1)),
            9 => bad.avps.swap(0, 1),
            _ => bad.avps.push(Avp::bytes(FAILED_AVP, [])),
        };
        assert!(
            Response::from_packet(&bad, &p, &id).is_err(),
            "mutation{mutation}"
        );
    }
}
#[test]
fn nasreq_session_is_first_in_native_request_reply_and_error_wire() {
    let client = Identity {
        host: "client.example".into(),
        realm: "example".into(),
    };
    let server = Identity {
        host: "server.example".into(),
        realm: "example".into(),
    };
    let request: Request =
        serde_json::from_value(json!({"username":"alice","password":"Correct"})).unwrap();
    let p = request
        .packet(&client, &server, "client.example;one")
        .unwrap();
    let success = Reply {
        verdict: Verdict::Accept,
        ..Default::default()
    }
    .packet(&p, &server)
    .unwrap();
    for packet in [
        p.clone(),
        success,
        error_answer(&p, &server, 3004, None).unwrap(),
        error_answer(&p, &server, 5012, None).unwrap(),
    ] {
        let wire = packet.encode().unwrap();
        assert_eq!(&wire[20..24], &263u32.to_be_bytes());
    }
    let mut bad = p;
    bad.avps.swap(0, 1);
    assert!(Request::from_packet(&bad, &server, &client).is_err());
}
#[test]
fn selected_pap_limit_is_128_utf8_bytes_and_base_optional_flags_are_clear() {
    let mut request: Request =
        serde_json::from_value(json!({"username":"alice","password":"é".repeat(64)})).unwrap();
    request.validate().unwrap();
    let client = Identity {
        host: "client.example".into(),
        realm: "example".into(),
    };
    let server = Identity {
        host: "server.example".into(),
        realm: "example".into(),
    };
    let mut packet = request
        .packet(&client, &server, "client.example;one")
        .unwrap();
    Request::from_packet(&packet, &server, &client).unwrap();
    request.password.as_mut().unwrap().push('x');
    assert!(request.validate().is_err());
    packet
        .avps
        .iter_mut()
        .find(|a| a.code == PASSWORD)
        .unwrap()
        .data
        .push(b'x');
    assert!(Request::from_packet(&packet, &server, &client).is_err());
    for code in [PRODUCT, FIRMWARE, ERROR_MESSAGE] {
        assert_eq!(Avp::bytes(code, []).flags, 0);
    }
    let mut answer = packet.answer(5012);
    answer.flags |= 0x10;
    assert!(answer.encode().is_err());
}
#[test]
fn capability_advertisements_allow_flat_typed_ids_without_enabling_vendor_aaa() {
    fn grouped(avps: Vec<Avp>) -> Vec<u8> {
        let mut p = Packet::request(DWR, 0).unwrap();
        p.avps = avps;
        p.encode().unwrap()[20..].to_vec()
    }
    let mut p = Packet::request(CER, 0).unwrap();
    capability_fields(
        &mut p,
        &Identity {
            host: "peer.example".into(),
            realm: "example".into(),
        },
        "127.0.0.1".parse().unwrap(),
    );
    let good = grouped(vec![
        Avp::number(VENDOR, 10415),
        Avp::number(AUTH_APP, 16777216),
    ]);
    p.avps.push(Avp::bytes(VENDOR_APP, good.as_slice()));
    capabilities(&p).unwrap();
    let mut only_vendor = p.clone();
    only_vendor.avps.retain(|a| a.code != AUTH_APP);
    assert!(capabilities(&only_vendor).is_err());
    for bad in [
        grouped(vec![
            Avp::number(VENDOR, 10415),
            Avp::bytes(VENDOR_APP, good.as_slice()),
        ]),
        grouped(vec![
            Avp::number(VENDOR, 10415),
            Avp::number(AUTH_APP, 1),
            Avp::number(AUTH_APP, 1),
        ]),
        grouped(vec![
            Avp::number(VENDOR, 10415),
            Avp::number(AUTH_APP, 1),
            Avp::number(ACCT_APP, 1),
        ]),
        grouped(vec![Avp::number(AUTH_APP, 1)]),
        grouped(vec![Avp::number(VENDOR, 10415); 4]),
        vec![0; MAX_FRAME_BYTES],
    ] {
        let mut bad_packet = p.clone();
        bad_packet.avps.last_mut().unwrap().data = bad;
        assert!(capabilities(&bad_packet).is_err());
    }
}
#[test]
fn optional_vendor_avps_padding_and_reserved_bits_are_ignored() {
    let mut p = Packet::request(DWR, 0).unwrap();
    p.avps.push(Avp {
        code: 999999,
        flags: 0x80,
        vendor: Some(123),
        data: vec![1],
    });
    let mut bytes = p.encode().unwrap();
    bytes[4] |= 0xf;
    bytes[24] |= 0x1f;
    *bytes.last_mut().unwrap() = 77;
    let got = Packet::decode(&bytes).unwrap();
    assert_eq!(got.flags, 0x80);
    assert!(got.unsupported_mandatory(&[]).is_none());
    assert_eq!(got.avps[0].data, vec![1]);
}
#[test]
fn constructed_action_and_handler_json_budgets_refuse_without_recursive_drop() {
    use netget::llm::actions::{client_trait::Client, protocol_trait::Server};
    let mut deep = Value::Null;
    for _ in 0..10000 {
        deep = Value::Array(vec![deep]);
    }
    assert!(netget::server::diameter::actions::DiameterProtocol
        .execute_action(deep)
        .is_err());
    let mut deep = Value::Null;
    for _ in 0..10000 {
        deep = Value::Array(vec![deep]);
    }
    assert!(netget::client::diameter::actions::DiameterClientProtocol
        .execute_action(deep)
        .is_err());
}
