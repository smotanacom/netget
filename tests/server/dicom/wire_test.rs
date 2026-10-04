//! NetGet's SCP from raw PDUs (bounds, refusals Rust owns) and the NetGet pair.
use crate::helpers::dicom::*;
use netget::server::dicom::pdu::{self, AssociateRq, Pdu, PresentationContext};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const SC: &str = "1.2.840.10008.5.1.4.1.1.7";

async fn read(s: &mut TcpStream) -> Option<Pdu> {
    tokio::time::timeout(Duration::from_secs(10), pdu::read(s))
        .await
        .expect("no PDU in time")
        .unwrap()
}

async fn associated(addr: std::net::SocketAddr) -> TcpStream {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let rq = AssociateRq {
        called: "NETGET".into(),
        calling: "RAW".into(),
        contexts: vec![PresentationContext {
            id: 1,
            abstract_syntax: SC.into(),
            transfer_syntaxes: vec!["1.2.840.10008.1.2".into()],
        }],
        max_pdu: 16384,
        implementation: Some("1.2.3.4".into()),
    };
    s.write_all(&pdu::encode(&Pdu::AssociateRq(rq), "NETGET", "RAW"))
        .await
        .unwrap();
    match read(&mut s).await {
        Some(Pdu::AssociateAc(results, _)) => {
            assert_eq!(results, vec![(1, 0, "1.2.840.10008.1.2".into())])
        }
        other => panic!("expected A-ASSOCIATE-AC, got {other:?}"),
    }
    s
}

async fn answer(s: &mut TcpStream) -> pdu::Message {
    let mut a = pdu::Assembly::default();
    loop {
        match read(s).await {
            Some(Pdu::Data(pdvs)) => {
                if let Some(m) = a.feed(pdvs).unwrap() {
                    return m;
                }
            }
            other => panic!("expected P-DATA, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn rust_owns_bounds_and_refusals() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        scp_policy(&dir.path().join("db.json")),
        json!({"idle_timeout_secs": 2}),
    )
    .await;

    // A PDU announcing more than 1 MiB is aborted before anything is read for it.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut head = vec![0x01, 0];
    head.extend_from_slice(&(pdu::MAX_PDU + 7).to_be_bytes());
    s.write_all(&head).await.unwrap();
    assert_eq!(
        read(&mut s).await,
        Some(Pdu::Abort {
            source: 2,
            reason: 2
        })
    );

    // Anything but A-ASSOCIATE-RQ first is aborted.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(&pdu::encode(&Pdu::ReleaseRq, "", ""))
        .await
        .unwrap();
    assert_eq!(
        read(&mut s).await,
        Some(Pdu::Abort {
            source: 2,
            reason: 2
        })
    );

    // A C-STORE whose dataset names another instance than its command: A900, handler not asked.
    let mut s = associated(addr).await;
    let cmd = pdu::command(&[
        (0x0000_0002, pdu::ui(SC)),
        (0x0000_0100, pdu::us(pdu::C_STORE_RQ)),
        (0x0000_0110, pdu::us(7)),
        (0x0000_0700, pdu::us(0)),
        (0x0000_0800, pdu::us(0)),
        (0x0000_1000, pdu::ui("1.2.3.1")),
    ]);
    let ds = netget::server::dicom::dataset::encode(
        json!({"00080016": {"vr": "UI", "Value": [SC]}, "00080018": {"vr": "UI", "Value": ["1.2.3.2"]}}).as_object().unwrap(),
        "1.2.840.10008.1.2",
    )
    .unwrap();
    for p in pdu::data_pdus(1, true, &cmd, 16384)
        .into_iter()
        .chain(pdu::data_pdus(1, false, &ds, 16384))
    {
        s.write_all(&p).await.unwrap();
    }
    let m = answer(&mut s).await;
    assert_eq!(pdu::field_u16(&m.command, 0x0000_0100), Some(0x8001));
    assert_eq!(pdu::field_u16(&m.command, 0x0000_0120), Some(7));
    assert_eq!(pdu::field_u16(&m.command, 0x0000_0900), Some(0xA900));

    // A DIMSE command the server does not implement (C-MOVE): 0211 unrecognized operation.
    let cmd = pdu::command(&[
        (0x0000_0002, pdu::ui(SC)),
        (0x0000_0100, pdu::us(0x0021)),
        (0x0000_0110, pdu::us(8)),
        (0x0000_0800, pdu::us(pdu::NO_DATASET)),
    ]);
    for p in pdu::data_pdus(1, true, &cmd, 16384) {
        s.write_all(&p).await.unwrap();
    }
    assert_eq!(
        pdu::field_u16(&answer(&mut s).await.command, 0x0000_0900),
        Some(0x0211)
    );

    // Idle past idle_timeout_secs: the association is aborted and closed.
    let started = std::time::Instant::now();
    assert_eq!(
        read(&mut s).await,
        Some(Pdu::Abort {
            source: 2,
            reason: 0
        })
    );
    assert!(started.elapsed() < Duration::from_secs(6));
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).await.unwrap();
    assert!(state
        .list_access_logs_for(Some(AccessLogOwner::Server(sid.as_u32())), None)
        .await
        .iter()
        .all(|e| e.event_type != "dicom_store"));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_against_netget_server() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        scp_policy(&dir.path().join("db.json")),
        json!({"ae_title": "ARCHIVE"}),
    )
    .await;
    let cid = client_in(&state, addr.to_string(), json!({"called_ae": "ARCHIVE"}))
        .await
        .unwrap();
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(30));
    for a in [
        json!({"type": "dicom_echo"}),
        json!({"type": "dicom_store", "sop_class_uid": SC, "sop_instance_uid": "1.2.3.9", "dataset": {"00100010": {"vr": "PN", "Value": [{"Alphabetic": "Pair^Test"}]}, "0020000D": {"vr": "UI", "Value": ["1.2.3.10"]}}}),
        json!({"type": "dicom_find", "level": "STUDY", "identifier": {"00100010": {"vr": "PN", "Value": [{"Alphabetic": "Pair*"}]}, "0020000D": {"vr": "UI"}}}),
    ] {
        assert!(matches!(
            send(a).await.unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let r: Vec<Value> = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "dicom_response",
        3,
    )
    .await
    .into_iter()
    .map(|r| r.request)
    .collect();
    assert_eq!(r[0]["status"], "0000");
    assert_eq!(r[1]["status"], "0000");
    assert_eq!(
        r[2]["matches"],
        json!([{"00080052": {"vr": "CS", "Value": ["STUDY"]}, "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Pair^Test"}]}, "0020000D": {"vr": "UI", "Value": ["1.2.3.10"]}}])
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
