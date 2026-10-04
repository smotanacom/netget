//! MLLP framing, ER7 parsing and building, injection refusals, and the NetGet pair.
use crate::helpers::hl7::*;
use netget::server::hl7::wire::{self, Header, Segment};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner, ClientStatus};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ADT: &str =
    "MSH|^~\\&|LAB|NORTH|EHR|HOSP|20260101||ADT^A01|C-1|P|2.5\rPID|1||12345^^^HOSP^MR||Doe^John\r";

#[test]
fn messages_parse_into_numbered_fields_and_refuse_structural_damage() {
    let m = wire::parse(ADT.as_bytes()).unwrap();
    assert_eq!(
        (m.message_type(), m.control_id(), m.msh(12)),
        ("ADT^A01", "C-1", "2.5")
    );
    assert_eq!(
        m.segments[1],
        Segment {
            id: "PID".into(),
            fields: vec![
                "1".into(),
                "".into(),
                "12345^^^HOSP^MR".into(),
                "".into(),
                "Doe^John".into()
            ]
        }
    );
    assert!(!m.latin1);
    let latin = wire::parse(b"MSH|^~\\&|A|B|C|D|1||ADT^A01|C|P|2.5\rPID|1||||M\xfcller\r").unwrap();
    assert!(latin.latin1);
    assert_eq!(latin.segments[1].fields[4], "Müller");
    for bad in [
        "PID|1\r",
        "MSH#^~\\&|A\r",
        "MSH|^~\\#|A|B|C|D|1||ADT^A01|C|P|2.5\r",
        "MSH|^~\\&|A|B|C|D|1||ADT^A01||P|2.5\r",
        "MSH|^~\\&|A|B|C|D|1||||P|2.5\r",
        "MSH|^~\\&|A|B|C|D|1||ADT^A01|C|P|2.5\rpid|1\r",
        "MSH|^~\\&|A|B|C|D|1||ADT^A01|C|P|2.5\rPID|a\u{7}b\r",
    ] {
        assert!(wire::parse(bad.as_bytes()).is_err(), "{bad:?} parsed");
    }
}

#[test]
fn built_fields_cannot_forge_segments_and_acks_swap_the_endpoints() {
    assert_eq!(wire::field("a|b").unwrap(), "a\\F\\b");
    for bad in ["x\rEVN|forged", "x\ny", "x\u{0}"] {
        assert!(wire::field(bad).is_err(), "{bad:?}");
    }
    let header = Header {
        sending_application: "NG",
        sending_facility: "F",
        receiving_application: "R",
        receiving_facility: "RF",
        message_type: "ORU^R01",
        control_id: "X1",
        processing_id: "T",
        version: "2.5",
    };
    let built = wire::build(
        &header,
        &[Segment {
            id: "OBX".into(),
            fields: vec!["1".into(), "ST".into(), "".into(), "".into(), "a|b".into()],
        }],
    )
    .unwrap();
    let parsed = wire::parse(&built).unwrap();
    assert_eq!(
        parsed.segments.len(),
        2,
        "a '|' in a field became \\F\\, not a new field or segment"
    );
    assert_eq!(parsed.segments[1].fields[4], "a\\F\\b");
    assert!(wire::build(
        &header,
        &[Segment {
            id: "MSH".into(),
            fields: vec![]
        }]
    )
    .is_err());
    let original = wire::parse(ADT.as_bytes()).unwrap();
    let ack = wire::parse(&wire::ack(&original, "AA", "ok", "A1", None, &[]).unwrap()).unwrap();
    assert_eq!(
        (ack.msh(3), ack.msh(5), ack.message_type(), ack.control_id()),
        ("EHR", "LAB", "ACK^A01^ACK", "A1")
    );
    assert_eq!(ack.segments[1].fields, vec!["AA", "C-1", "ok"]);
    assert!(wire::ack(&original, "OK", "", "A1", None, &[]).is_err());
    assert_eq!(wire::timestamp().len(), 14);
}

#[tokio::test]
async fn framing_is_refused_before_it_can_grow_or_desynchronize() {
    async fn read(bytes: Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
        let (mut a, mut b) = tokio::io::duplex(1 << 21);
        tokio::spawn(async move {
            let _ = a.write_all(&bytes).await;
        });
        wire::read_frame(&mut b, Duration::from_secs(2)).await
    }
    assert_eq!(
        read(wire::frame(b"MSH|x")).await.unwrap().unwrap(),
        b"MSH|x"
    );
    assert!(read(b"MSH|no start\x1c\r".to_vec()).await.is_err());
    assert!(read(b"\x0bMSH|a\x1cX".to_vec()).await.is_err());
    assert!(
        read(b"\x0bMSH|a\x1c\r\x0bMSH|b\x1c\r".to_vec())
            .await
            .is_err(),
        "two frames in one read are not one message"
    );
    let mut big = vec![wire::START];
    big.extend(std::iter::repeat_n(b'x', wire::MAX_MESSAGE_BYTES + 10));
    assert!(read(big).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_sender_and_endpoint_agree_and_refusals_stay_on_the_sender() {
    let state = state();
    let (sid, addr) = server_in(&state, endpoint_policy(), json!({})).await;
    let cid = client_in(&state, addr.to_string(), quiet_sender(), json!({"sending_application":"NETGET","sending_facility":"LAB","receiving_application":"EHR","version":"2.5.1","processing_id":"T"})).await;
    for m in [
        adt(),
        json!({"type":"hl7_send","message_type":"ORU^R01^ORU_R01","segments":[{"id":"OBX","fields":["1","NM","GLU","","5.4"]}]}),
        json!({"type":"hl7_send","message_type":"MFN^M02","segments":[]}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, m, Duration::from_secs(10))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let acks = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "hl7_ack_received",
        3,
    )
    .await;
    let codes: Vec<_> = acks
        .iter()
        .map(|a| a.request["code"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(codes, ["AA", "AE", "AR"]);
    assert_eq!(acks[0].request["control_id"], "NGC1");
    assert!(acks[1].request["segments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == "ERR"));
    let rows = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "hl7_message",
        3,
    )
    .await;
    assert_eq!(
        (
            rows[0].request["sending_application"].as_str(),
            rows[0].request["version"].as_str(),
            rows[0].request["processing_id"].as_str()
        ),
        (Some("NETGET"), Some("2.5.1"), Some("T"))
    );
    for bad in [
        json!({"type":"hl7_send","message_type":"ADT^A01","segments":[{"id":"PID","fields":["1\rEVN|forged"]}]}),
        json!({"type":"hl7_send","message_type":"ADT^A01","segments":[{"id":"MSH","fields":[]}]}),
        json!({"type":"hl7_send","message_type":"ADT^A01","segments":[{"id":"pid","fields":[]}]}),
        json!({"type":"hl7_send","message_type":"ADT^A01","processing_id":"X","segments":[]}),
    ] {
        assert!(
            matches!(
                state
                    .send_to_client(cid, bad.clone(), Duration::from_secs(5))
                    .await
                    .unwrap(),
                ClientSendOutcome::Rejected { .. }
            ),
            "{bad}"
        );
    }
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_failure_is_an_application_error_and_garbage_closes_the_connection() {
    let state = state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(&wire::frame(ADT.as_bytes())).await.unwrap();
    let reply = wire::read_frame(&mut s, Duration::from_secs(20))
        .await
        .unwrap()
        .unwrap();
    let ack = wire::parse(&reply).unwrap();
    assert_eq!(
        ack.segments[1].fields[0], "AE",
        "no handler and no model: an application error, never AA"
    );
    assert!(ack
        .segments
        .iter()
        .any(|seg| seg.id == "ERR" && seg.fields[2].starts_with("207")));
    s.write_all(&wire::frame(b"not hl7 at all")).await.unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), s.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0,
        "an unparseable message has no MSH to acknowledge"
    );
    state.remove_server(sid).await;

    // A mismatched MSA-2 ends the sender's session.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = crate::helpers::hl7::state();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let m = wire::parse(
            &wire::read_frame(&mut s, Duration::from_secs(10))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let wrong = wire::parse(ADT.as_bytes()).unwrap();
        let _ = m;
        s.write_all(&wire::frame(
            &wire::ack(&wrong, "AA", "", "Z", None, &[]).unwrap(),
        ))
        .await
        .unwrap();
        // Hold the socket until the sender gives up on it.
        let mut b = [0u8; 1];
        let _ = s.read(&mut b).await;
    });
    let cid = client_in(&state, addr.to_string(), quiet_sender(), json!({})).await;
    let _ = state
        .send_to_client(cid, adt(), Duration::from_secs(10))
        .await;
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                state.get_client(cid).await.map(|c| c.status),
                Some(ClientStatus::Error(_))
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "an ACK for another control id must end the session"
    );
}
