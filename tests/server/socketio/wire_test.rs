//! Engine.IO and Socket.IO on the wire without peers: codecs, handshake errors, long-polling
//! payloads, acknowledgements, concurrent polls, heartbeat timeout, rooms, peer pushes, a
//! handler-less server failing closed, and the NetGet client/server pair over both transports.
use crate::helpers::socketio::*;
use netget::server::socketio::packet::{self, Eio, Kind, Sio};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn codecs() {
    let p = Sio::decode("2/admin,12[\"chat\",{\"a\":1}]").unwrap();
    assert_eq!(
        (p.kind, p.nsp.as_str(), p.id),
        (Kind::Event, "/admin", Some(12))
    );
    assert_eq!(p.event().unwrap(), ("chat".into(), vec![json!({"a": 1})]));
    assert_eq!(p.encode(), "2/admin,12[\"chat\",{\"a\":1}]");
    assert_eq!(Sio::decode("0").unwrap().nsp, "/");
    assert_eq!(
        Sio::new(Kind::Connect, "/", None, Some(json!({"sid": "x"}))).encode(),
        "0{\"sid\":\"x\"}"
    );
    for bad in [
        "",
        "9",
        "51-[\"a\",{\"_placeholder\":true,\"num\":0}]",
        "2[\"\"]",
        "2/bad ns,[\"a\"]",
        "2{not json",
    ] {
        assert!(
            Sio::decode(bad)
                .and_then(|p| if p.kind == Kind::Event {
                    p.event().map(|_| p)
                } else {
                    Ok(p)
                })
                .is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(
        packet::split_payload("2\u{1e}42[\"x\"]\u{1e}6").unwrap(),
        vec![
            Eio::Ping(String::new()),
            Eio::Message("2[\"x\"]".into()),
            Eio::Noop
        ]
    );
    assert!(
        packet::split_payload("bAAA").is_err(),
        "binary Engine.IO payloads are refused"
    );
}

async fn get(url: &str) -> (u16, String) {
    let r = reqwest::get(url).await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

async fn post(url: &str, body: &str) -> (u16, String) {
    let r = reqwest::Client::new()
        .post(url)
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}

/// Poll until the payload holds every needle (packets arrive as the server produces them).
async fn poll_until(url: &str, needles: &[&str]) -> Vec<String> {
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !needles
            .iter()
            .all(|n| seen.iter().any(|p: &String| p.contains(n)))
        {
            let (s, body) = get(url).await;
            assert_eq!(s, 200, "{body}");
            seen.extend(body.split('\u{1e}').map(str::to_owned));
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never saw {needles:?} in {seen:?}"));
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn polling_handshake_acks_rooms_heartbeat_and_peer_push() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        chat_policy(),
        json!({"namespaces": ["/", "/admin"], "ping_interval_ms": 1000, "ping_timeout_ms": 1000}),
    )
    .await;
    let base = format!("http://{addr}/socket.io/");
    let (s, body) = get(&format!("{base}?EIO=3&transport=polling")).await;
    assert_eq!(
        (
            s,
            serde_json::from_str::<Value>(&body).unwrap()["code"].as_u64()
        ),
        (400, Some(5))
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            &get(&format!("{base}?EIO=4&transport=carrier-pigeon"))
                .await
                .1
        )
        .unwrap()["code"],
        0
    );
    assert_eq!(
        serde_json::from_str::<Value>(
            &get(&format!("{base}?EIO=4&transport=polling&sid=nope"))
                .await
                .1
        )
        .unwrap()["code"],
        1
    );
    let open =
        |body: &str| -> Value { serde_json::from_str(body.strip_prefix('0').unwrap()).unwrap() };
    let (_, body) = get(&format!("{base}?EIO=4&transport=polling")).await;
    let info = open(&body);
    assert_eq!(
        (
            info["upgrades"].clone(),
            info["pingInterval"].as_u64(),
            info["maxPayload"].as_u64()
        ),
        (json!(["websocket"]), Some(1000), Some(1_000_000))
    );
    let url = format!(
        "{base}?EIO=4&transport=polling&sid={}",
        info["sid"].as_str().unwrap()
    );
    assert_eq!(post(&url, "40").await, (200, "ok".into()));
    let seen = poll_until(&url, &["40{\"sid\"", "42[\"welcome\",\"netget\",\"/\"]"]).await;
    assert!(seen.iter().any(|p| p.starts_with("40{")));
    // An event with an ack id: the broadcast and the acknowledgement both come back.
    post(&url, "421[\"chat message\",\"hello\"]\u{1e}3").await;
    poll_until(
        &url,
        &[
            "42[\"chat message\",\"broadcast\",\"hello\"]",
            "431[\"delivered\",\"hello\"]",
        ],
    )
    .await;
    // A binary event is refused without killing the session.
    post(&url, "451-[\"x\",{\"_placeholder\":true,\"num\":0}]").await;
    post(&url, "42[\"join\",\"r1\"]\u{1e}3").await;
    poll_until(&url, &["42[\"joined\",\"r1\"]"]).await;
    // The peer handle pushes an emit into the session.
    let conn = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "socketio_connect",
        1,
    )
    .await[0]
        .connection_id
        .unwrap();
    let pushed = state
        .send_to_peer(
            sid,
            conn,
            json!({"type":"socketio_emit","event":"news","args":[{"n":1}]}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(pushed, ClientSendOutcome::Sent { .. }),
        "{pushed:?}"
    );
    poll_until(&url, &["42[\"news\",{\"n\":1}]"]).await;
    assert!(
        matches!(
            state
                .send_to_peer(
                    sid,
                    conn,
                    json!({"type":"socketio_emit","event":"x","namespace":"/admin"}),
                    Duration::from_secs(5)
                )
                .await
                .unwrap(),
            ClientSendOutcome::Rejected { .. }
        ),
        "no socket in /admin"
    );
    // Two polls at once: a protocol error that closes the session.
    let (a, b) = tokio::join!(get(&url), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        get(&url).await
    });
    assert!(a.0 == 400 || b.0 == 400, "{a:?} {b:?}");
    assert_eq!(
        serde_json::from_str::<Value>(&get(&url).await.1).unwrap()["code"],
        1
    );
    // A session that never answers pings is closed after pingInterval + pingTimeout.
    let (_, body) = get(&format!("{base}?EIO=4&transport=polling")).await;
    let idle = format!(
        "{base}?EIO=4&transport=polling&sid={}",
        open(&body)["sid"].as_str().unwrap()
    );
    post(&idle, "40").await;
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (s, body) = get(&idle).await;
            if s == 400 || body.contains('1') && body.split('\u{1e}').any(|p| p == "1") {
                break;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "an unanswered ping closes the session");
    let timed_out = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let gone = logs(
                &state,
                AccessLogOwner::Server(sid.as_u32()),
                "socketio_disconnect",
                1,
            )
            .await;
            if gone.iter().any(|g| g.request["reason"] == "ping timeout") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        timed_out.is_ok(),
        "the idle socket left with reason ping timeout"
    );
    state.remove_server(sid).await;

    // No handler and no model: CONNECT is refused with a category message, never accepted.
    let state = crate::helpers::socketio::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let base = format!("http://{addr}/socket.io/");
    let (_, body) = get(&format!("{base}?EIO=4&transport=polling")).await;
    let url = format!(
        "{base}?EIO=4&transport=polling&sid={}",
        open(&body)["sid"].as_str().unwrap()
    );
    post(&url, "40").await;
    let seen = poll_until(&url, &["44{\"message\""]).await;
    assert!(!seen.iter().any(|p| p.starts_with("40{")), "{seen:?}");
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_server_agree_over_both_transports() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        chat_policy(),
        json!({"namespaces": ["/", "/admin"]}),
    )
    .await;
    for transport in ["websocket", "polling"] {
        let cid = client_in(&state, addr.to_string(), json!({"transport": transport, "namespaces": ["/", "/admin"], "auth": {"token": "secret"}})).await.unwrap();
        let owner = AccessLogOwner::Client(cid.as_u32());
        let connected = logs(&state, owner, "socketio_connected", 2).await;
        assert!(connected
            .iter()
            .all(|c| c.request["transport"] == transport));
        let welcome = logs(&state, owner, "socketio_event", 1).await;
        assert_eq!(
            (
                welcome[0].request["event"].as_str(),
                welcome[0].request["args"].clone()
            ),
            (Some("welcome"), json!(["netget", "/"]))
        );
        let send = |a: Value| state.send_to_client(cid, a, Duration::from_secs(10));
        assert!(matches!(
            send(json!({"type":"socketio_emit","event":"chat message","args":["pair"],"ack":true}))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
        let acks = logs(&state, owner, "socketio_ack_received", 1).await;
        assert_eq!(acks[0].request["args"], json!(["delivered", "pair"]));
        send(json!({"type":"socketio_emit","event":"ask me"}))
            .await
            .unwrap();
        let ping = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let rows = logs(&state, owner, "socketio_event", 1).await;
                if let Some(r) = rows.iter().find(|r| r.request["event"] == "ping me") {
                    break r.request.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let ack_id = ping["ack_id"]
            .as_u64()
            .expect("the server asked for an ack");
        send(json!({"type":"socketio_ack","ack_id":ack_id,"args":["pong from netget"]}))
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let rows = logs(&state, owner, "socketio_event", 1).await;
                if let Some(r) = rows.iter().find(|r| r.request["event"] == "pong received") {
                    break r.request.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(pong["args"], json!(["pong from netget"]));
        assert!(matches!(
            send(json!({"type":"socketio_emit","event":"x","namespace":"/nowhere"}))
                .await
                .unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
        send(json!({"type":"socketio_emit","event":"bye"}))
            .await
            .unwrap();
        let gone = logs(&state, owner, "socketio_disconnected", 1).await;
        assert_eq!(gone[0].request["namespace"], "/");
        state.remove_client(cid).await;
    }
    let refused = client_in(
        &state,
        addr.to_string(),
        json!({"namespaces": ["/admin"], "auth": {"token": "bad"}}),
    )
    .await
    .unwrap();
    let errs = logs(
        &state,
        AccessLogOwner::Client(refused.as_u32()),
        "socketio_connect_error",
        1,
    )
    .await;
    assert_eq!(errs[0].request["message"], "not authorized");
    state.remove_client(refused).await;
    state.remove_server(sid).await;
}
