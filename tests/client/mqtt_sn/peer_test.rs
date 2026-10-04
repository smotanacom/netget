//! NetGet's MQTT-SN client against the Eclipse Paho MQTT-SN Gateway (C++, independent,
//! unchanged) in front of Mosquitto: publishes with QoS 0, 1 and 2 reach mosquitto_sub; a
//! subscription receives what mosquitto_pub sends; a message sent while the client sleeps is
//! held by the Paho gateway and delivered on wake. Fails, never skips.
use crate::helpers::mqtt_sn::*;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn client_through_paho_gateway() {
    let broker = start_mosquitto().await.unwrap();
    let gateway = start_paho_gateway(broker.port).await.unwrap();
    let port = broker.port.to_string();
    let sub = Watched::spawn(
        "mosquitto_sub",
        &[
            "-h",
            "127.0.0.1",
            "-p",
            &port,
            "-t",
            "sensors/#",
            "-v",
            "-q",
            "2",
        ],
    );
    broker
        .wait_for_log("sensors/#", Duration::from_secs(20))
        .await
        .unwrap();

    let state = state();
    let cid = client_in(
        &state,
        gateway.addr(),
        json!({"client_id": "netget-s1", "keep_alive_secs": 30}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(20));
    let op = |name: &'static str| move |r: &Value| r["operation"] == name;
    for a in [
        json!({"type": "mqttsn_subscribe", "topic": "cmd/led", "qos": 1}),
        json!({"type": "mqttsn_publish", "topic": "sensors/temp", "payload": "21.5", "qos": 1}),
        json!({"type": "mqttsn_publish", "topic": "sensors/hum", "payload": "40", "qos": 2}),
        json!({"type": "mqttsn_publish", "topic": "sensors/door", "payload": "open"}),
    ] {
        assert!(
            matches!(
                send(a.clone()).await.unwrap(),
                ClientSendOutcome::Sent { .. }
            ),
            "{a}"
        );
    }
    let subscribed = &wait_for(&state, owner, "mqttsn_result", 1, op("subscribe")).await[0];
    assert_eq!(subscribed["return_code"], "accepted", "{subscribed}");
    let published = wait_for(&state, owner, "mqttsn_result", 3, op("publish")).await;
    assert_eq!(
        published
            .iter()
            .map(|r| r["return_code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["accepted", "accepted", "sent"]
    );
    sub.wait_for("sensors/temp 21.5").await;
    sub.wait_for("sensors/hum 40").await;
    sub.wait_for("sensors/door open").await;

    let mosquitto_pub = |msg: &'static str| {
        let port = port.clone();
        async move {
            let s = tokio::process::Command::new("mosquitto_pub")
                .args([
                    "-h",
                    "127.0.0.1",
                    "-p",
                    &port,
                    "-t",
                    "cmd/led",
                    "-m",
                    msg,
                    "-q",
                    "1",
                ])
                .status()
                .await
                .unwrap();
            assert!(s.success());
        }
    };
    mosquitto_pub("on").await;
    let got = &wait_for(&state, owner, "mqttsn_message_received", 1, |m| {
        m["payload"] == "on"
    })
    .await[0];
    assert_eq!(
        (got["topic"].as_str(), got["qos"].as_i64()),
        (Some("cmd/led"), Some(1)),
        "{got}"
    );

    // Asleep, the Paho gateway holds the next command until the client wakes.
    assert!(matches!(
        send(json!({"type": "mqttsn_sleep", "duration_secs": 30}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    wait_for(&state, owner, "mqttsn_result", 1, op("sleep")).await;
    mosquitto_pub("off").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        state
            .list_access_logs_for(Some(owner), None)
            .await
            .iter()
            .all(|e| e.request["payload"] != "off"),
        "delivered while asleep"
    );
    assert!(matches!(
        send(json!({"type": "mqttsn_wake"})).await.unwrap(),
        ClientSendOutcome::Sent { .. }
    ));
    let woke = &wait_for(&state, owner, "mqttsn_result", 1, op("wake")).await[0];
    assert_eq!(woke["messages"], 1, "{woke}\n{}", gateway.log());
    wait_for(&state, owner, "mqttsn_message_received", 1, |m| {
        m["payload"] == "off"
    })
    .await;
    assert!(matches!(
        send(json!({"type": "disconnect"})).await.unwrap(),
        ClientSendOutcome::Disconnected
    ));
    drop(sub);
    state.remove_client(cid).await;
}
