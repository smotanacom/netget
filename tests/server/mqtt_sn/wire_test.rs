//! The MQTT-SN codec and gateway from the wire: every packet type round-trips (including the
//! 3-byte length), topic filters match as MQTT's do, and NetGet's client against NetGet's
//! gateway covers QoS 2 both ways, a handler-published greeting, a lost client's will, a sleeping
//! client's held messages, a message injected through a peer handle, and gateway refusals.
use crate::helpers::mqtt_sn::*;
use netget::server::mqtt_sn::packet::{self, Flags, Packet};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UdpSocket;

#[test]
fn packets_round_trip() {
    let f = Flags {
        dup: true,
        qos: 2,
        retain: true,
        will: false,
        clean_session: true,
        topic_id_type: packet::TOPIC_SHORT,
    };
    for p in [
        Packet::Advertise {
            gw_id: 3,
            duration: 900,
        },
        Packet::SearchGw { radius: 1 },
        Packet::GwInfo {
            gw_id: 3,
            gw_add: vec![],
        },
        Packet::Connect {
            flags: Flags {
                will: true,
                clean_session: true,
                ..Flags::default()
            },
            duration: 30,
            client_id: "s1".into(),
        },
        Packet::ConnAck {
            rc: packet::CONGESTION,
        },
        Packet::WillTopicReq,
        Packet::WillTopic {
            flags: Flags {
                qos: 1,
                retain: true,
                ..Flags::default()
            },
            topic: "a/b".into(),
        },
        Packet::WillMsgReq,
        Packet::WillMsg {
            msg: b"bye".to_vec(),
        },
        Packet::Register {
            topic_id: 0,
            msg_id: 9,
            topic: "a/b".into(),
        },
        Packet::RegAck {
            topic_id: 4,
            msg_id: 9,
            rc: 0,
        },
        Packet::Publish {
            flags: f,
            topic: u16::from_be_bytes(*b"ab"),
            msg_id: 77,
            data: vec![0xff; 400],
        },
        Packet::Publish {
            flags: Flags {
                qos: -1,
                topic_id_type: packet::TOPIC_PREDEFINED,
                ..Flags::default()
            },
            topic: 7,
            msg_id: 0,
            data: b"x".to_vec(),
        },
        Packet::PubAck {
            topic_id: 4,
            msg_id: 77,
            rc: packet::INVALID_TOPIC_ID,
        },
        Packet::PubRec { msg_id: 1 },
        Packet::PubRel { msg_id: 1 },
        Packet::PubComp { msg_id: 1 },
        Packet::Subscribe {
            flags: Flags {
                qos: 1,
                ..Flags::default()
            },
            msg_id: 2,
            topic_name: Some("a/#".into()),
            topic_id: 0,
        },
        Packet::Subscribe {
            flags: Flags {
                topic_id_type: packet::TOPIC_PREDEFINED,
                ..Flags::default()
            },
            msg_id: 2,
            topic_name: None,
            topic_id: 7,
        },
        Packet::SubAck {
            flags: Flags {
                qos: 1,
                ..Flags::default()
            },
            topic_id: 0,
            msg_id: 2,
            rc: 0,
        },
        Packet::Unsubscribe {
            flags: Flags::default(),
            msg_id: 3,
            topic_name: Some("a/#".into()),
            topic_id: 0,
        },
        Packet::UnsubAck { msg_id: 3 },
        Packet::PingReq {
            client_id: Some("s1".into()),
        },
        Packet::PingReq { client_id: None },
        Packet::PingResp,
        Packet::Disconnect { duration: Some(60) },
        Packet::Disconnect { duration: None },
        Packet::WillTopicUpd {
            flags: Flags::default(),
            topic: "c".into(),
        },
        Packet::WillTopicResp { rc: 0 },
        Packet::WillMsgUpd { msg: b"m".to_vec() },
        Packet::WillMsgResp { rc: 0 },
    ] {
        let bytes = packet::encode(&p);
        assert_eq!(packet::decode(&bytes).unwrap(), p, "{bytes:02x?}");
        assert!(packet::decode(&bytes[..bytes.len() - 1]).is_err() || bytes.len() <= 2);
    }
    let long = packet::encode(&Packet::WillMsg { msg: vec![1; 300] });
    assert_eq!(
        (long[0], u16::from_be_bytes([long[1], long[2]]) as usize),
        (1, long.len())
    );
    let wrapped = packet::wrap_forwarder(&[0xaa, 0xbb], &packet::encode(&Packet::PingResp));
    let (node, inner) = packet::unwrap_forwarder(&wrapped).unwrap().unwrap();
    assert_eq!(
        (node, packet::decode(inner).unwrap()),
        (vec![0xaa, 0xbb], Packet::PingResp)
    );
    for (f, t, m) in [
        ("a/#", "a", true),
        ("a/#", "a/b/c", true),
        ("a/+", "a/b", true),
        ("a/+", "a/b/c", false),
        ("#", "$SYS/x", false),
        ("+/b", "a/b", true),
        ("a", "a/b", false),
    ] {
        assert_eq!(packet::matches(f, t), m, "{f} {t}");
    }
    assert!(
        !packet::valid_filter("a/#/b")
            && !packet::valid_filter("a+")
            && packet::valid_filter("+/+/#")
    );
}

async fn send(state: &netget::state::app_state::AppState, cid: netget::state::ClientId, a: Value) {
    let r = state
        .send_to_client(cid, a.clone(), Duration::from_secs(20))
        .await
        .unwrap();
    assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_through_netget_gateway() {
    let state = state();
    let (sid, gw) = server_in(&state, json!({"gateway_id": 42})).await;
    let a = client_in(&state, gw.to_string(), json!({"client_id": "alpha"}))
        .await
        .unwrap();
    let b = client_in(&state, gw.to_string(), json!({"client_id": "beta", "keep_alive_secs": 1, "will_topic": "lab/status", "will_message": "beta lost"})).await.unwrap();
    let (ao, bo) = (
        AccessLogOwner::Client(a.as_u32()),
        AccessLogOwner::Client(b.as_u32()),
    );
    let op = |name: &'static str| move |r: &Value| r["operation"] == name;

    send(
        &state,
        a,
        json!({"type": "mqttsn_subscribe", "topic": "lab/#", "qos": 2}),
    )
    .await;
    let sub = &wait_for(&state, ao, "mqttsn_result", 1, op("subscribe")).await[0];
    assert_eq!(
        (
            sub["granted_qos"].as_i64(),
            sub["return_code"].as_str(),
            sub["topic_id"].as_u64()
        ),
        (Some(2), Some("accepted"), Some(0))
    );
    send(
        &state,
        a,
        json!({"type": "mqttsn_subscribe", "topic": "hold/#", "qos": 2}),
    )
    .await;
    let greeting = &wait_for(&state, ao, "mqttsn_message_received", 1, |m| {
        m["topic"] == "hold/greeting"
    })
    .await[0];
    assert_eq!(
        (greeting["payload"].as_str(), greeting["qos"].as_i64()),
        (Some("hello"), Some(1))
    );
    assert_eq!(
        wait_for(&state, ao, "mqttsn_result", 2, op("subscribe")).await[1]["granted_qos"],
        1
    );

    send(
        &state,
        b,
        json!({"type": "mqttsn_publish", "topic": "lab/x", "payload": "q2", "qos": 2}),
    )
    .await;
    send(&state, b, json!({"type": "mqttsn_publish", "topic": "lab/y", "payload": "ff00", "encoding": "hex", "qos": 1})).await;
    send(
        &state,
        b,
        json!({"type": "mqttsn_publish", "topic": "admin/x", "payload": "no", "qos": 1}),
    )
    .await;
    let results = wait_for(&state, bo, "mqttsn_result", 3, op("publish")).await;
    assert_eq!(
        results
            .iter()
            .map(|r| r["return_code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["accepted", "accepted", "not_supported"]
    );
    let got = wait_for(&state, ao, "mqttsn_message_received", 2, |m| {
        m["topic"].as_str().unwrap().starts_with("lab/")
    })
    .await;
    assert_eq!(
        (got[0]["payload"].as_str(), got[0]["qos"].as_i64()),
        (Some("q2"), Some(2))
    );
    assert_eq!(
        (
            got[1]["payload"].as_str(),
            got[1]["payload_encoding"].as_str()
        ),
        (Some("ff00"), Some("hex"))
    );

    // beta vanishes without DISCONNECT; after 1.5 x its 1 s keep-alive its will is published.
    state.remove_client(b).await;
    let will = &wait_for(&state, ao, "mqttsn_message_received", 1, |m| {
        m["topic"] == "lab/status"
    })
    .await[0];
    assert_eq!(will["payload"], "beta lost");

    // alpha sleeps; a message published meanwhile is held until it wakes.
    send(
        &state,
        a,
        json!({"type": "mqttsn_sleep", "duration_secs": 30}),
    )
    .await;
    wait_for(&state, ao, "mqttsn_result", 1, op("sleep")).await;
    let c = client_in(&state, gw.to_string(), json!({"client_id": "gamma"}))
        .await
        .unwrap();
    send(
        &state,
        c,
        json!({"type": "mqttsn_publish", "topic": "lab/z", "payload": "held", "qos": 1}),
    )
    .await;
    wait_for(
        &state,
        AccessLogOwner::Client(c.as_u32()),
        "mqttsn_result",
        1,
        op("publish"),
    )
    .await;
    let before = state
        .list_access_logs_for(Some(ao), None)
        .await
        .iter()
        .filter(|e| e.request["topic"] == "lab/z")
        .count();
    assert_eq!(before, 0, "a sleeping client was sent a message");
    send(&state, a, json!({"type": "mqttsn_wake"})).await;
    let woke = &wait_for(&state, ao, "mqttsn_result", 1, op("wake")).await[0];
    assert_eq!(woke["messages"], 1, "{woke}");
    wait_for(&state, ao, "mqttsn_message_received", 1, |m| {
        m["topic"] == "lab/z" && m["payload"] == "held"
    })
    .await;

    // A message injected through alpha's peer handle reaches alpha (once awake again).
    send(&state, a, json!({"type": "mqttsn_connect"})).await;
    wait_for(&state, ao, "mqttsn_result", 1, op("connect")).await;
    let conn = state
        .get_server(sid)
        .await
        .unwrap()
        .connections
        .values()
        .find(|c| {
            c.protocol_info.data["client_id"] == "alpha"
                && c.status == netget::state::server::ConnectionStatus::Active
        })
        .map(|c| c.id.as_u32())
        .unwrap();
    let r = state
        .send_to_peer(
            sid,
            conn,
            json!({"type": "mqttsn_publish", "topic": "direct/one", "payload": "for alpha"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{r:?}");
    wait_for(&state, ao, "mqttsn_message_received", 1, |m| {
        m["topic"] == "direct/one"
    })
    .await;

    // Gateway refusals from a raw socket.
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    s.connect(gw).await.unwrap();
    let exchange = |p: Packet| {
        let s = &s;
        async move {
            s.send(&packet::encode(&p)).await.unwrap();
            let mut b = [0u8; 512];
            let n = tokio::time::timeout(Duration::from_secs(10), s.recv(&mut b))
                .await
                .expect("no answer")
                .unwrap();
            packet::decode(&b[..n]).unwrap()
        }
    };
    assert_eq!(
        exchange(Packet::SearchGw { radius: 0 }).await,
        Packet::GwInfo {
            gw_id: 42,
            gw_add: vec![]
        }
    );
    let publish = Packet::Publish {
        flags: Flags {
            qos: 1,
            ..Flags::default()
        },
        topic: 9999,
        msg_id: 5,
        data: b"x".to_vec(),
    };
    assert_eq!(
        exchange(publish.clone()).await,
        Packet::Disconnect { duration: None }
    );
    assert_eq!(
        exchange(Packet::Connect {
            flags: Flags {
                clean_session: true,
                ..Flags::default()
            },
            duration: 10,
            client_id: "intruder".into()
        })
        .await,
        Packet::ConnAck {
            rc: packet::NOT_SUPPORTED
        }
    );
    assert_eq!(
        exchange(Packet::Connect {
            flags: Flags {
                clean_session: true,
                ..Flags::default()
            },
            duration: 10,
            client_id: "raw".into()
        })
        .await,
        Packet::ConnAck { rc: 0 }
    );
    assert_eq!(
        exchange(publish).await,
        Packet::PubAck {
            topic_id: 9999,
            msg_id: 5,
            rc: packet::INVALID_TOPIC_ID
        }
    );
    assert_eq!(
        exchange(Packet::Subscribe {
            flags: Flags::default(),
            msg_id: 6,
            topic_name: Some("bad/#/x".into()),
            topic_id: 0
        })
        .await,
        Packet::SubAck {
            flags: Flags::default(),
            topic_id: 0,
            msg_id: 6,
            rc: packet::NOT_SUPPORTED
        }
    );
    assert_eq!(
        exchange(Packet::Disconnect { duration: None }).await,
        Packet::Disconnect { duration: None }
    );
    state.remove_client(a).await;
    state.remove_client(c).await;
    state.remove_server(sid).await;
}
