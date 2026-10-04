//! LwM2M from NetGet's side: SenML JSON, text and link-format codecs; NetGet's device against
//! NetGet's server (registration, reads, a write, an observation and its notification,
//! deregistration); raw registrations refused for a missing endpoint, a refused device and an
//! unknown registration.
use crate::helpers::lwm2m::*;
use netget::server::coap::codec::{self, CoapMessage, MessageType};
use netget::server::lwm2m::content;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;
use tokio::net::UdpSocket;

#[test]
fn codecs() {
    let senml = br#"[{"bn":"/3/0/","n":"0","vs":"Acme"},{"n":"9","v":95},{"n":"21","vb":true},{"bn":"/5/0/","n":"0","vd":"AP8"},{"n":"1","vlo":"3:0"}]"#;
    let values = content::senml_decode(senml).unwrap();
    assert_eq!(
        values,
        vec![
            json!({"path": "/3/0/0", "value": "Acme"}),
            json!({"path": "/3/0/9", "value": 95}),
            json!({"path": "/3/0/21", "value": true}),
            json!({"path": "/5/0/0", "opaque": "00ff"}),
            json!({"path": "/5/0/1", "object_link": "3:0"}),
        ]
    );
    assert_eq!(
        content::senml_decode(&content::senml_encode(&values).unwrap()).unwrap(),
        values
    );
    assert_eq!(
        content::text_decode("/3/0/14", b"+02"),
        vec![json!({"path": "/3/0/14", "value": "+02"})]
    );
    assert_eq!(
        content::text_decode("/3/0/9", b"95"),
        vec![json!({"path": "/3/0/9", "value": 95})]
    );
    assert_eq!(
        content::text_decode("/3303/0/5700", b"21.5"),
        vec![json!({"path": "/3303/0/5700", "value": 21.5})]
    );
    let links =
        content::links_decode(br#"</>;rt="oma.lwm2m";ct=110,</1/0>,</3/0>;ver=1.1"#).unwrap();
    assert_eq!(links[0]["attributes"]["rt"], "oma.lwm2m");
    assert_eq!(
        (
            links[2]["path"].as_str(),
            links[2]["attributes"]["ver"].as_str()
        ),
        (Some("/3/0"), Some("1.1"))
    );
    for bad in ["", "3/0", "/a", "/1/2/3/4/5", "/70000"] {
        assert!(!content::valid_path(bad), "{bad}");
    }
    assert!(content::senml_encode(&[json!({"path": "/3/0/0", "value": [1]})]).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_device_against_netget_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = state();
    let (sid, addr) = server_in(&state).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        json!({"endpoint": "netget-pair", "lifetime": 30, "objects": ["/3/0", "/3303/0"]}),
        device_policy(&dir.path().join("d.json")),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Server(sid.as_u32());
    let reg = wait_for(&state, owner, "lwm2m_register", |e| {
        e["endpoint"] == "netget-pair"
    })
    .await;
    assert_eq!(
        (reg["version"].as_str(), reg["lifetime"].as_u64()),
        (Some("1.1"), Some(30))
    );
    let r = |op: &'static str, path: &'static str| {
        let state = &state;
        async move {
            wait_for(state, owner, "lwm2m_response", |e| {
                e["operation"] == op && e["path"] == path
            })
            .await
        }
    };
    assert_eq!(
        r("read", "/3/0/0").await["values"][0]["value"],
        "NetGet Device"
    );
    assert_eq!(
        r("read", "/3303/0").await["values"],
        json!([{"path": "/3303/0/5700", "value": 21.5}, {"path": "/3303/0/5701", "value": "Cel"}])
    );
    assert_eq!(r("read", "/3/0/14").await["values"][0]["value"], "+02");
    assert_eq!(r("read", "/42/0").await["code"], "4.04");
    assert_eq!(r("observe", "/3303/0/5700").await["code"], "2.05");
    let n = state.send_to_client(cid, json!({"type": "lwm2m_notify", "path": "/3303/0/5700", "values": [{"path": "/3303/0/5700", "value": 30.0}]}), Duration::from_secs(10)).await.unwrap();
    assert!(matches!(n, ClientSendOutcome::Sent { .. }), "{n:?}");
    let note = wait_for(&state, owner, "lwm2m_notification", |_| true).await;
    assert_eq!(
        (
            note["values"][0]["value"].as_f64(),
            note["sequence"].as_u64()
        ),
        (Some(30.0), Some(1))
    );
    let unobserved = state.send_to_client(cid, json!({"type": "lwm2m_notify", "path": "/3/0/0", "values": [{"path": "/3/0/0", "value": "x"}]}), Duration::from_secs(10)).await.unwrap();
    assert!(matches!(unobserved, ClientSendOutcome::Rejected { .. }));
    assert!(matches!(
        state
            .send_to_client(cid, json!({"type": "disconnect"}), Duration::from_secs(20))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    assert_eq!(
        wait_for(&state, owner, "lwm2m_deregister", |_| true).await["reason"],
        "deregistered"
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_registration_refusals() {
    let state = state();
    let (sid, addr) = server_in(&state).await;
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    s.connect(addr).await.unwrap();
    let mut mid = 100u16;
    let mut send = |code: u8, path: &[&str], query: &[&str], payload: &[u8]| {
        mid += 1;
        let mut options: Vec<(u16, Vec<u8>)> = path
            .iter()
            .map(|p| (codec::OPT_URI_PATH, p.as_bytes().to_vec()))
            .collect();
        options.extend(
            query
                .iter()
                .map(|q| (codec::OPT_URI_QUERY, q.as_bytes().to_vec())),
        );
        CoapMessage {
            mtype: MessageType::Confirmable,
            code,
            message_id: mid,
            token: vec![1, 2],
            options,
            payload: payload.to_vec(),
        }
        .encode()
        .unwrap()
    };
    let exchange = |bytes: Vec<u8>| {
        let s = &s;
        async move {
            s.send(&bytes).await.unwrap();
            let mut b = [0u8; 2048];
            let n = tokio::time::timeout(Duration::from_secs(20), s.recv(&mut b))
                .await
                .expect("no answer")
                .unwrap();
            CoapMessage::decode(&b[..n]).unwrap().code
        }
    };
    assert_eq!(
        exchange(send(codec::CODE_POST, &["rd"], &["lt=60"], b"</3/0>")).await,
        codec::CODE_BAD_REQUEST
    );
    assert_eq!(
        exchange(send(codec::CODE_POST, &["rd"], &["ep=intruder"], b"</3/0>")).await,
        codec::code(4, 3)
    );
    assert_eq!(
        exchange(send(codec::CODE_POST, &["rd", "999"], &["lt=60"], b"")).await,
        codec::CODE_NOT_FOUND
    );
    assert_eq!(
        exchange(send(codec::CODE_DELETE, &["rd", "999"], &[], b"")).await,
        codec::CODE_NOT_FOUND
    );
    assert_eq!(
        exchange(send(codec::CODE_GET, &["other"], &[], b"")).await,
        codec::CODE_NOT_FOUND
    );
    state.remove_server(sid).await;
}
