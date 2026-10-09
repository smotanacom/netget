//! NetGet's FIX initiator against QuickFIX/Go 0.9.12 as acceptor (independent, unchanged; it
//! validates every application message against FIX44.xml): logon, a NewOrderSingle answered
//! with an ExecutionReport, an OrderCancelRequest answered with a BusinessMessageReject, logout.
//! Fails, never skips.
use crate::helpers::fix::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_works_against_quickfix_go() {
    let qfgo = start_acceptor().await.unwrap();
    let state = state();
    let cid = client_in(
        &state,
        qfgo.addr(),
        json!({"sender_comp_id": "CLIENT", "target_comp_id": "QFGO", "heartbeat_secs": 30}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    assert_eq!(
        logs(&state, owner, "fix_logged_on", 1).await[0].request["target_comp_id"],
        "QFGO"
    );
    let send = |a: serde_json::Value| state.send_to_client(cid, a, Duration::from_secs(10));
    let order = json!({"type": "fix_send", "msg_type": "NewOrderSingle", "fields": [
        {"name": "ClOrdID", "value": "1"}, {"name": "HandlInst", "value": "1"}, {"name": "Symbol", "value": "MSFT"},
        {"name": "Side", "value": "1"}, {"name": "TransactTime", "value": "20261004-12:00:00.000"},
        {"name": "OrderQty", "value": "100"}, {"name": "OrdType", "value": "2"}, {"name": "Price", "value": "410.5"}]});
    assert!(matches!(
        send(order).await.unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let cancel = json!({"type": "fix_send", "msg_type": "OrderCancelRequest", "fields": [
        {"name": "OrigClOrdID", "value": "1"}, {"name": "ClOrdID", "value": "2"}, {"name": "Symbol", "value": "MSFT"},
        {"name": "Side", "value": "1"}, {"name": "TransactTime", "value": "20261004-12:00:01.000"}, {"name": "OrderQty", "value": "100"}]});
    assert!(matches!(
        send(cancel).await.unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let m = logs(&state, owner, "fix_message", 2).await;
    let by_type = |t: &str| {
        m.iter()
            .find(|r| r.request["msg_type"] == t)
            .unwrap_or_else(|| {
                panic!(
                    "no {t}: {:?}",
                    m.iter().map(|r| &r.request).collect::<Vec<_>>()
                )
            })
    };
    let er = by_type("8");
    assert!(er.request["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == "OrderID" && f["value"] == "QF-1"));
    let j = by_type("j");
    assert!(j.request["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == "Text" && f["value"] == "cancels are not supported here"));
    assert!(matches!(
        send(json!({"type": "fix_logout", "text": "done"}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let out = logs(&state, owner, "fix_logged_out", 1).await;
    assert_eq!(
        out[0].request["reason"], "peer logged out",
        "{}",
        out[0].request
    );

    let log = qfgo.log();
    let got: Vec<serde_json::Value> = lines(&log)
        .into_iter()
        .filter(|v| v["dir"] == "app")
        .collect();
    assert_eq!(got.len(), 2, "QuickFIX validated both messages:\n{log}");
    assert_eq!(
        (
            got[0]["fields"]["55"].as_str(),
            got[0]["fields"]["44"].as_str(),
            got[0]["fields"]["49"].as_str()
        ),
        (Some("MSFT"), Some("410.5"), Some("CLIENT"))
    );
    assert!(lines(&log).iter().any(|v| v["event"] == "logout"));
    state.remove_client(cid).await;
}
