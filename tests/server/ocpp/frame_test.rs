//! OCPP-J framing and core-field rules without peers, handshake and frame refusals from a raw
//! WebSocket, and the NetGet pair.
use crate::helpers::ocpp::*;
use futures::{SinkExt, StreamExt};
use netget::server::ocpp::frame::{self, Frame, Version};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

#[test]
fn frames_parse_encode_and_refuse_with_the_id_when_one_is_known() {
    let call = frame::parse(r#"[2,"a1","Heartbeat",{}]"#).unwrap();
    assert_eq!(
        call,
        Frame::Call {
            id: "a1".into(),
            action: "Heartbeat".into(),
            payload: json!({})
        }
    );
    assert_eq!(frame::encode(&call).unwrap(), r#"[2,"a1","Heartbeat",{}]"#);
    assert!(matches!(
        frame::parse(r#"[3,"a1",{"currentTime":"x"}]"#).unwrap(),
        Frame::Result { .. }
    ));
    assert!(matches!(
        frame::parse(r#"[4,"a1","NotSupported","no",{}]"#).unwrap(),
        Frame::Error { .. }
    ));
    for (bad, id) in [
        ("{}", None),
        ("[5,\"x\"]", Some("x")),
        (r#"[2,"x","Heart beat",{}]"#, Some("x")),
        (r#"[2,"x","Heartbeat",[]]"#, Some("x")),
        (r#"[3,"x"]"#, Some("x")),
        (r#"[2,1,"Heartbeat",{}]"#, None),
        (
            r#"[2,"0123456789012345678901234567890123456","Heartbeat",{}]"#,
            None,
        ),
        ("not json", None),
    ] {
        let (got, _) = frame::parse(bad).unwrap_err();
        assert_eq!(got.as_deref(), id, "{bad}");
    }
    let deep = (0..40).fold(json!({}), |v, _| json!({"a": v}));
    assert!(frame::parse(&format!(r#"[2,"x","Heartbeat",{deep}]"#)).is_err());
}

#[test]
fn core_fields_follow_each_versions_schema_and_spelling() {
    assert!(frame::check_request(
        Version::V16,
        "BootNotification",
        &json!({"chargePointVendor":"v"})
    )
    .is_err());
    assert!(frame::check_request(
        Version::V16,
        "BootNotification",
        &json!({"chargePointVendor":"v","chargePointModel":"m"})
    )
    .is_ok());
    assert!(frame::check_request(
        Version::V201,
        "BootNotification",
        &json!({"chargePointVendor":"v","chargePointModel":"m"})
    )
    .is_err());
    assert!(frame::check_response(
        Version::V16,
        "BootNotification",
        &json!({"status":"Accepted","currentTime":"t"})
    )
    .is_err());
    assert!(
        frame::check_request(Version::V16, "VendorThing", &json!({})).is_ok(),
        "non-core actions pass to the handler"
    );
    assert_eq!(
        Version::V16.occurrence_code(),
        "OccurenceConstraintViolation"
    );
    assert_eq!(
        Version::V201.occurrence_code(),
        "OccurrenceConstraintViolation"
    );
    assert!(frame::validate_error_code(Version::V16, "FormatViolation").is_err());
    assert!(frame::validate_error_code(Version::V201, "FormatViolation").is_ok());
    assert_eq!(frame::charge_point_id("/ocpp/CP%2D1").unwrap(), "CP-1");
    for bad in ["/", "/ocpp/a%2Fb", "/x/a b"] {
        assert!(frame::charge_point_id(bad).is_err(), "{bad}");
    }
}

async fn raw(
    addr: std::net::SocketAddr,
    sub: Option<&str>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let mut req = format!("ws://{addr}/CP-RAW").into_client_request().unwrap();
    if let Some(s) = sub {
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", s.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn reply(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    text: &str,
) -> Value {
    ws.send(Message::Text(text.into())).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_csms_refuses_bad_handshakes_frames_and_missing_fields_and_never_invents_answers() {
    let state = state();
    let (sid, addr) = server_in(&state, csms_policy(), json!({"ocpp_versions":["2.0.1"]})).await;
    assert!(
        raw(addr, None).await.is_err(),
        "no subprotocol: handshake refused"
    );
    assert!(
        raw(addr, Some("ocpp1.6")).await.is_err(),
        "1.6 not offered by this CSMS"
    );
    let mut ws = raw(addr, Some("ocpp1.6, ocpp2.0.1")).await.unwrap();
    let r = reply(
        &mut ws,
        r#"[2,"m1","BootNotification",{"reason":"PowerUp"}]"#,
    )
    .await;
    assert_eq!(
        (r[0].as_u64(), r[1].as_str(), r[2].as_str()),
        (Some(4), Some("m1"), Some("OccurrenceConstraintViolation"))
    );
    let r = reply(&mut ws, r#"[2,"m2","Heartbeat",{"x":1"#).await;
    assert_eq!(
        (r[0].as_u64(), r[2].as_str()),
        (Some(4), Some("FormatViolation"))
    );
    let r = reply(&mut ws, r#"[2,"m3","Heartbeat",{}]"#).await;
    assert_eq!((r[0].as_u64(), r[1].as_str()), (Some(3), Some("m3")));
    assert!(r[2]["currentTime"].is_string());
    let r = reply(&mut ws, r#"[2,"m4","DataTransfer",{"vendorId":"x"}]"#).await;
    assert_eq!(r[2], "NotSupported");
    state.remove_server(sid).await;

    // Handler-less: InternalError, never a made-up acceptance.
    let state = crate::helpers::ocpp::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let mut ws = raw(addr, Some("ocpp1.6")).await.unwrap();
    let r = reply(
        &mut ws,
        r#"[2,"m1","BootNotification",{"chargePointVendor":"v","chargePointModel":"m"}]"#,
    )
    .await;
    assert_eq!(
        (r[0].as_u64(), r[2].as_str()),
        (Some(4), Some("InternalError"))
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_charge_point_and_csms_walk_both_versions_and_correlate_calls_each_way() {
    for (version, last) in [("1.6", "StopTransaction"), ("2.0.1", "TransactionEvent")] {
        let state = state();
        let (sid, addr) = server_in(&state, csms_policy(), json!({})).await;
        let cid = client_in(
            &state,
            addr.to_string(),
            charge_point_policy(version),
            json!({"charge_point_id":"CP-NG","ocpp_version":version}),
        )
        .await
        .unwrap();
        let router = AccessLogOwner::Client(cid.as_u32());
        let n = if version == "1.6" { 6 } else { 5 };
        let responses = logs(&state, router, "ocpp_call_response", n).await;
        assert_eq!(responses.last().unwrap().request["action"], last);
        let calls = logs(&state, AccessLogOwner::Server(sid.as_u32()), "ocpp_call", n).await;
        assert_eq!(calls[0].request["ocpp_version"], version);
        let conn = calls[0].connection_id.unwrap();
        assert!(matches!(state.send_to_peer(sid, conn, json!({"type":"ocpp_send_call","action":"Reset","payload":{"type":if version == "1.6" {"Hard"} else {"OnIdle"}}}), Duration::from_secs(10)).await.unwrap(), ClientSendOutcome::Sent { .. }));
        let r = logs(
            &state,
            AccessLogOwner::Server(sid.as_u32()),
            "ocpp_call_response",
            1,
        )
        .await;
        assert_eq!(r[0].request["payload"]["status"], "Accepted");
        // Charge-point side refusals happen before anything is sent.
        for bad in [
            json!({"type":"ocpp_call","action":"BootNotification","payload":{}}),
            json!({"type":"ocpp_call_result","payload":{"status":"Accepted"}}),
            json!({"type":"ocpp_call","action":"Boot Notification","payload":{}}),
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
}
