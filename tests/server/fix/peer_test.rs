//! QuickFIX/Go 0.9.12 (independent, unchanged; it validates every application message against
//! FIX44.xml before its application sees it) as initiator against NetGet's acceptor with the
//! policy script: logon, a NewOrderSingle answered with an ExecutionReport, one answered with a
//! BusinessMessageReject, heartbeats at HeartBtInt 1, logout; and a refused CompID. Fails,
//! never skips.
use crate::helpers::fix::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn quickfix_go_initiator_against_netget_acceptor() {
    let qfgo = peer("NETGET_FIX_QFGO");
    let dict = peer("NETGET_FIX_DICTIONARY");
    let state = state();
    let (sid, addr) = server_in(&state, policy(), json!({})).await;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(&qfgo)
            .args([
                "initiator",
                "127.0.0.1",
                &addr.port().to_string(),
                "CLIENT",
                &dict,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("qfgo timed out")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "qfgo failed:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let l = lines(&text);
    assert!(l.iter().any(|v| v["event"] == "logon"), "{text}");
    let exec = l
        .iter()
        .find(|v| v["dir"] == "app" && v["type"] == "8")
        .unwrap_or_else(|| panic!("no ExecutionReport passed QuickFIX validation:\n{text}"));
    let f = &exec["fields"];
    assert_eq!(
        (
            f["37"].as_str(),
            f["11"].as_str(),
            f["39"].as_str(),
            f["55"].as_str(),
            f["151"].as_str()
        ),
        (Some("O-1"), Some("1"), Some("0"), Some("AAPL"), Some("100")),
        "{exec}"
    );
    assert_eq!(
        (f["49"].as_str(), f["56"].as_str(), f["8"].as_str()),
        (Some("NETGET"), Some("CLIENT"), Some("FIX.4.4"))
    );
    let rej = l
        .iter()
        .find(|v| v["dir"] == "app" && v["type"] == "j")
        .unwrap_or_else(|| panic!("no BusinessMessageReject:\n{text}"));
    assert_eq!(
        (
            rej["fields"]["380"].as_str(),
            rej["fields"]["372"].as_str(),
            rej["fields"]["58"].as_str()
        ),
        (Some("2"), Some("D"), Some("symbol not traded here"))
    );
    let heartbeats = l
        .iter()
        .filter(|v| v["dir"] == "admin" && v["type"] == "0")
        .count();
    assert!(
        heartbeats >= 2,
        "NetGet keeps HeartBtInt 1 ({heartbeats} heartbeats):\n{text}"
    );
    assert!(
        !l.iter().any(|v| v["dir"] == "admin" && v["type"] == "3"),
        "NetGet rejected a QuickFIX message:\n{text}"
    );
    assert!(l.iter().any(|v| v["event"] == "logout"));

    let owner = AccessLogOwner::Server(sid.as_u32());
    let logon = logs(&state, owner, "fix_logon", 1).await;
    assert_eq!(
        (
            logon[0].request["sender_comp_id"].as_str(),
            logon[0].request["heartbeat_secs"].as_u64()
        ),
        (Some("CLIENT"), Some(1))
    );
    let msgs = logs(&state, owner, "fix_message", 2).await;
    assert_eq!(msgs[0].request["msg_type_name"], "NewOrderSingle");
    let price = msgs[0].request["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "Price")
        .unwrap();
    assert_eq!(price["value"], "150.25");

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(&qfgo)
            .args([
                "initiator",
                "127.0.0.1",
                &addr.port().to_string(),
                "INTRUDER",
                &dict,
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("qfgo timed out")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let l = lines(&text);
    assert!(!l.iter().any(|v| v["event"] == "logon"), "{text}");
    // A refused Logon reaches none of QuickFIX's callbacks; its screen log shows the wire.
    assert!(
        text.contains("35=5") && text.contains("58=unknown counterparty INTRUDER"),
        "{text}"
    );
    state.remove_server(sid).await;
}
