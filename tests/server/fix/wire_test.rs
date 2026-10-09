//! NetGet's acceptor from raw FIX: the Logon rules, TestRequest, resend with gap fill and
//! PossDupFlag, garbled input ignored, gap detection, a sequence number too low, the heartbeat
//! timeout, a fail-closed application answer, an injected message, and the NetGet pair.
use crate::helpers::fix::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn session_layer_from_the_wire() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({"logon_timeout_secs": 2})).await;

    // Not a Logon first: disconnected without a word.
    let mut raw = Raw::connect(addr).await;
    raw.send("0", &[]).await;
    assert!(raw.recv().await.is_none());
    // Wrong TargetCompID: Logout naming it.
    let mut raw = Raw::connect(addr).await;
    raw.target = "SOMEONE".into();
    let m = raw.logon("30").await;
    assert_eq!(
        (m.msg_type(), m.get(58)),
        ("5", Some("TargetCompID is not this acceptor"))
    );
    // Silence after connecting: closed when the logon timeout passes.
    let mut raw = Raw::connect(addr).await;
    assert!(raw.recv().await.is_none());

    let mut raw = Raw::connect(addr).await;
    let m = raw.logon("30").await;
    assert_eq!(
        (m.msg_type(), m.get(108), m.get(141), m.seq()),
        ("A", Some("30"), Some("Y"), Some(1))
    );
    raw.send("1", &[(112, "PING")]).await;
    let hb = raw.expect("0").await;
    assert_eq!((hb.get(112), hb.seq()), (Some("PING"), Some(2)));
    raw.send(
        "D",
        &[
            (11, "7"),
            (21, "1"),
            (55, "IBM"),
            (54, "2"),
            (60, "20261004-12:00:00.000"),
            (38, "10"),
            (40, "1"),
        ],
    )
    .await;
    let er = raw.expect("8").await;
    assert_eq!(
        (er.get(37), er.get(55), er.get(54), er.seq()),
        (Some("O-7"), Some("IBM"), Some("2"), Some(3))
    );

    // A garbled copy (bad CheckSum) is ignored and not counted.
    let mut garbled = raw.encode(
        "D",
        raw.seq,
        &[],
        &[
            (11, "8"),
            (21, "1"),
            (55, "IBM"),
            (54, "1"),
            (60, "20261004-12:00:00.000"),
            (38, "1"),
            (40, "1"),
        ],
    );
    let n = garbled.len();
    garbled[n - 2] = if garbled[n - 2] == b'9' { b'8' } else { b'9' };
    raw.write(&garbled).await;
    raw.send(
        "D",
        &[
            (11, "8"),
            (21, "1"),
            (55, "IBM"),
            (54, "1"),
            (60, "20261004-12:00:00.000"),
            (38, "1"),
            (40, "1"),
        ],
    )
    .await;
    assert_eq!(raw.expect("8").await.get(11), Some("8"));

    // ResendRequest from 1: the Logon and Heartbeat become one gap fill; the reports come back
    // with PossDupFlag and their original sending times.
    raw.send("2", &[(7, "1"), (16, "0")]).await;
    let gap = raw.expect("4").await;
    assert_eq!(
        (gap.seq(), gap.get(123), gap.get(36), gap.get(43)),
        (Some(1), Some("Y"), Some("3"), Some("Y"))
    );
    let again = raw.expect("8").await;
    assert_eq!(
        (again.seq(), again.get(43), again.get(11)),
        (Some(3), Some("Y"), Some("7"))
    );
    assert!(again.get(122).is_some(), "OrigSendingTime");
    assert_eq!(raw.expect("8").await.seq(), Some(4));

    // A gap: NetGet asks for the missing range and holds the out-of-order message.
    let skipped = raw.seq;
    raw.seq += 3;
    raw.send(
        "D",
        &[
            (11, "9"),
            (21, "1"),
            (55, "IBM"),
            (54, "1"),
            (60, "20261004-12:00:00.000"),
            (38, "1"),
            (40, "1"),
        ],
    )
    .await;
    let rr = raw.expect("2").await;
    assert_eq!(
        (rr.get(7), rr.get(16)),
        (Some(skipped.to_string().as_str()), Some("0"))
    );
    let fill = raw.encode(
        "4",
        skipped,
        &[(43, "Y")],
        &[(123, "Y"), (36, &(skipped + 3).to_string())],
    );
    raw.write(&fill).await;
    // The held message is part of the requested range: resent with PossDupFlag.
    let resent = raw.encode(
        "D",
        skipped + 3,
        &[(43, "Y"), (122, "20261004-12:00:00.000")],
        &[
            (11, "9"),
            (21, "1"),
            (55, "IBM"),
            (54, "1"),
            (60, "20261004-12:00:00.000"),
            (38, "1"),
            (40, "1"),
        ],
    );
    raw.write(&resent).await;
    assert_eq!(raw.expect("8").await.get(11), Some("9"));
    raw.send(
        "D",
        &[
            (11, "10"),
            (21, "1"),
            (55, "IBM"),
            (54, "1"),
            (60, "20261004-12:00:00.000"),
            (38, "1"),
            (40, "1"),
        ],
    )
    .await;
    assert_eq!(raw.expect("8").await.get(11), Some("10"));

    // An operator's injected message goes out on the session's next sequence number.
    let conn = logs(&state, AccessLogOwner::Server(sid.as_u32()), "fix_logon", 1)
        .await
        .last()
        .unwrap()
        .connection_id
        .unwrap();
    let news = json!({"type": "fix_send", "msg_type": "News", "fields": [{"name": "Headline", "value": "market closes early"}]});
    assert!(matches!(
        state
            .send_to_peer(sid, conn, news, Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    assert_eq!(raw.expect("B").await.get(148), Some("market closes early"));

    // Too low without PossDupFlag: Logout and close.
    raw.seq -= 1;
    raw.send("0", &[]).await;
    let bye = raw.expect("5").await;
    assert!(
        bye.get(58).unwrap().starts_with("MsgSeqNum too low"),
        "{:?}",
        bye.fields
    );
    assert!(raw.recv().await.is_none());
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_timeout_and_fail_closed_answers() {
    let state = state();
    let (sid, addr) = server_in(&state, logon_only(), json!({})).await;
    let mut raw = Raw::connect(addr).await;
    assert_eq!(raw.logon("1").await.msg_type(), "A");
    // No model: the order is refused as application-not-available, never filled.
    raw.send(
        "D",
        &[
            (11, "1"),
            (21, "1"),
            (55, "IBM"),
            (54, "1"),
            (60, "20261004-12:00:00.000"),
            (38, "1"),
            (40, "1"),
        ],
    )
    .await;
    let j = raw.expect("j").await;
    assert_eq!(
        (j.get(380), j.get(372), j.get(45)),
        (Some("4"), Some("D"), Some("2"))
    );
    // Silent counterparty at HeartBtInt 1: a TestRequest, then a Logout.
    let started = std::time::Instant::now();
    let tr = raw.expect("1").await;
    assert!(tr.get(112).is_some());
    let bye = raw.expect("5").await;
    assert_eq!(bye.get(58), Some("no answer to TestRequest"));
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(raw.recv().await.is_none());

    // No logon decision at all: refused.
    let (sid2, addr2) = server_in(&state, vec![], json!({})).await;
    let mut raw = Raw::connect(addr2).await;
    let m = raw.logon("30").await;
    assert_eq!(
        (m.msg_type(), m.get(58)),
        ("5", Some("logon cannot be processed now"))
    );
    state.remove_server(sid).await;
    state.remove_server(sid2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_initiator_against_netget_acceptor() {
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({"sender_comp_id": "EXCHANGE"})).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        json!({"sender_comp_id": "CLIENT", "target_comp_id": "EXCHANGE", "heartbeat_secs": 5}),
    )
    .await
    .unwrap();
    let order = json!({"type": "fix_send", "msg_type": "NewOrderSingle", "fields": [
        {"name": "ClOrdID", "value": "P1"}, {"name": "HandlInst", "value": "1"}, {"name": "Symbol", "value": "MSFT"},
        {"name": "Side", "value": "1"}, {"name": "TransactTime", "value": "20261004-12:00:00.000"},
        {"name": "OrderQty", "value": 25}, {"name": "OrdType", "value": "1"}]});
    assert!(matches!(
        state
            .send_to_client(cid, order, Duration::from_secs(10))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let owner = AccessLogOwner::Client(cid.as_u32());
    let m = logs(&state, owner, "fix_message", 1).await;
    assert_eq!(m[0].request["msg_type_name"], "ExecutionReport");
    let fields = m[0].request["fields"].as_array().unwrap();
    assert!(
        fields
            .iter()
            .any(|f| f["name"] == "OrderID" && f["value"] == "O-P1"),
        "{fields:?}"
    );
    let news =
        json!({"type": "fix_send", "msg_type": "B", "fields": [{"tag": 148, "value": "bye"}]});
    state
        .send_to_client(cid, news, Duration::from_secs(10))
        .await
        .unwrap();
    let out = logs(&state, owner, "fix_logged_out", 1).await;
    assert!(
        out[0].request["reason"]
            .as_str()
            .unwrap()
            .contains("news means goodbye"),
        "{}",
        out[0].request
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}
