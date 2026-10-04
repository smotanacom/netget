//! mqtt-sn-tools (C, independent, unchanged) against NetGet's gateway: subscribers by name,
//! wildcard and short topic receive what publishers send with QoS 1, 0 and -1 (a predefined
//! topic, no connection), through forwarder encapsulation; a refused publish and a refused
//! client. Fails, never skips.
use crate::helpers::mqtt_sn::*;
use netget::state::AccessLogOwner;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn mqtt_sn_tools_through_netget() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        json!({"predefined_topics": {"7": "sensors/predef"}}),
    )
    .await;
    let owner = AccessLogOwner::Server(sid.as_u32());
    let port = addr.port().to_string();
    let sub_tool = tool("NETGET_MQTTSN_SUB");
    let sub = Watched::spawn(
        &sub_tool,
        &[
            "-h",
            "127.0.0.1",
            "-p",
            &port,
            "-i",
            "sub-wild",
            "-t",
            "sensors/#",
            "-q",
            "1",
            "-v",
        ],
    );
    let short = Watched::spawn(
        &sub_tool,
        &[
            "-h",
            "127.0.0.1",
            "-p",
            &port,
            "-i",
            "sub-short",
            "-t",
            "ab",
            "-t",
            "admin/#",
            "-v",
        ],
    );
    wait_for(&state, owner, "mqttsn_subscribe", 3, |_| true).await;

    for args in [
        vec!["-i", "pub-1", "-t", "sensors/temp", "-m", "21.5", "-q", "1"],
        vec!["-i", "pub-2", "-t", "sensors/hum", "-m", "40"],
        vec!["-T", "7", "-m", "no-connection", "-q", "-1"],
        vec!["-i", "pub-4", "-t", "ab", "-m", "short-topic"],
        vec![
            "-i",
            "pub-5",
            "--fe",
            "-t",
            "sensors/fwd",
            "-m",
            "forwarded",
            "-q",
            "1",
        ],
    ] {
        let (ok, said) = mqtt_sn_pub(addr.port(), &args).await;
        assert!(ok, "{args:?}:\n{said}");
    }
    sub.wait_for("sensors/temp: 21.5").await;
    sub.wait_for("sensors/hum: 40").await;
    // Delivered under predefined id 7, which mqtt-sn-sub prints by number: it knows no names.
    sub.wait_for("0007: no-connection").await;
    sub.wait_for("sensors/fwd: forwarded").await;
    short.wait_for("ab: short-topic").await;
    assert!(!sub.text().contains("short-topic"), "{}", sub.text());

    // The handler refuses admin/ publishes (mqtt-sn-pub ignores the PUBACK's return code, so the
    // evidence is that the admin/# subscriber never sees it) and the client "intruder".
    let (ok, said) = mqtt_sn_pub(
        addr.port(),
        &["-i", "pub-6", "-t", "admin/reboot", "-m", "now", "-q", "1"],
    )
    .await;
    assert!(ok, "{said}");
    let (ok, said) = mqtt_sn_pub(
        addr.port(),
        &["-i", "intruder", "-t", "sensors/x", "-m", "x"],
    )
    .await;
    assert!(!ok && said.contains("CONNACK return code: 0x03"), "{said}");
    let refused = wait_for(&state, owner, "mqttsn_message", 1, |m| {
        m["topic"] == "admin/reboot"
    })
    .await;
    assert_eq!(refused[0]["client_id"], "pub-6");
    assert!(
        !short.text().contains("now") && !sub.text().contains("sensors/x"),
        "{}{}",
        short.text(),
        sub.text()
    );

    let messages = wait_for(&state, owner, "mqttsn_message", 6, |_| true).await;
    let predef = messages
        .iter()
        .find(|m| m["topic"] == "sensors/predef")
        .unwrap();
    assert_eq!(
        (predef["qos"].as_i64(), predef["client_id"].clone()),
        (Some(-1), json!(null)),
        "{predef}"
    );
    let connects = wait_for(&state, owner, "mqttsn_connect", 1, |c| {
        c["client_id"] == "pub-5"
    })
    .await;
    assert!(connects[0]["forwarder_node"].is_string(), "{}", connects[0]);
    drop(sub);
    drop(short);
    state.remove_server(sid).await;
}
