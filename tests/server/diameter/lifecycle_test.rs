use crate::helpers::diameter::{aa, client, identity, intercept, logs, negotiated, policy, server};
use netget::{
    server::diameter::codec::*,
    state::{client_handles::ClientSendOutcome, AccessLogOwner},
};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
async fn read(peer: &mut TcpStream) -> Packet {
    read_packet(peer, Duration::from_secs(5)).await.unwrap()
}
async fn send(peer: &mut TcpStream, p: &Packet) {
    write_packet(peer, p).await.unwrap();
}
async fn eof(peer: &mut TcpStream) {
    let mut b = [0; 1];
    let result = tokio::time::timeout(Duration::from_secs(5), peer.read(&mut b))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0)) || result.is_err());
}
fn request() -> Packet {
    serde_json::from_value::<Request>(json!({"username":"alice","password":"Correct"}))
        .unwrap()
        .packet(
            &identity("client.example"),
            &identity("server.example"),
            "client.example;one",
        )
        .unwrap()
}
#[tokio::test]
async fn capability_refusals_include_required_base_fields_and_close_without_aaa_events() {
    let (state, id, addr, _) = server(Some(policy("accept")), None).await;
    for (mutation, code) in [(0, 5001), (1, 5010), (2, 5017), (3, 5012)] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        let mut cer = Packet::request(CER, 0).unwrap();
        capability_fields(
            &mut cer,
            &identity("client.example"),
            "127.0.0.1".parse().unwrap(),
        );
        match mutation {
            0 => cer.avps.push(Avp::number(999999, 1)),
            1 => cer.avps.retain(|a| a.code != AUTH_APP),
            2 => cer.avps.push(Avp::number(SECURITY, 1)),
            _ => cer.avps.retain(|a| a.code != HOST),
        }
        send(&mut peer, &cer).await;
        let cea = read(&mut peer).await;
        assert!(cea.matches(&cer));
        assert_eq!(cea.num(RESULT).unwrap(), code);
        assert_eq!(cea.flags & 0x20, 0);
        assert_eq!(
            Identity::from_packet(&cea).unwrap(),
            identity("server.example")
        );
        assert_eq!(
            decode_address(&cea.one(HOST_IP).unwrap().data).unwrap(),
            addr.ip()
        );
        assert_eq!(cea.num(VENDOR).unwrap(), 0);
        assert!(!cea.text(PRODUCT, 255).unwrap().is_empty());
        assert_eq!(cea.one(PRODUCT).unwrap().flags, 0);
        assert_eq!(cea.num(AUTH_APP).unwrap(), 1);
        if mutation == 0 {
            let failed = &cea.one(FAILED_AVP).unwrap().data;
            assert_eq!(&failed[..4], &999999u32.to_be_bytes());
        }
        eof(&mut peer).await;
    }
    assert!(state.list_access_logs(None).await.is_empty());
    state.remove_server(id).await;
}
#[tokio::test]
async fn accepted_connection_capacity_refuses_excess_and_removal_closes_all_owned_sockets() {
    let (state, id, addr, _) = server(None, None).await;
    let capacity = netget::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;
    let mut peers = Vec::with_capacity(capacity);
    for _ in 0..capacity {
        peers.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.get_server(id).await.unwrap().connections.len() != capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut excess = TcpStream::connect(addr).await.unwrap();
    eof(&mut excess).await;
    assert_eq!(
        state.get_server(id).await.unwrap().connections.len(),
        capacity
    );
    state.remove_server(id).await;
    for mut peer in peers {
        eof(&mut peer).await;
    }
    assert!(state.list_intercepts().await.is_empty());
}
#[tokio::test]
async fn native_pair_records_common_access_before_accept_and_returns_typed_attributes() {
    let (state, sid, addr, _) = server(Some(policy("accept")), None).await;
    let (c, cid) = client(addr.to_string(), None, None).await;
    for typ in 1..=3 {
        assert!(matches!(
            c.send_to_client(cid, aa("Correct", typ), Duration::from_secs(5))
                .await
                .unwrap(),
            ClientSendOutcome::Executed { .. }
        ));
        let rows = logs(
            &c,
            AccessLogOwner::Client(cid.as_u32()),
            "diameter_aa_result",
            typ as usize,
        )
        .await;
        assert_eq!(rows[(typ - 1) as usize].request["reply"]["accepted"], true);
    }
    let rows = state.list_access_logs(None).await;
    assert_eq!(
        rows.iter()
            .filter(|e| e.event_type == "diameter_aa_request")
            .count(),
        3
    );
    assert!(rows.iter().all(|e| e.response[0]["type"]
        == if e.request["request"].get("password").is_some() {
            "private_handler_result"
        } else {
            "respond_diameter_aa"
        }));
    let stats = c.get_client(cid).await.unwrap().connection.unwrap();
    assert_eq!(stats.connected_addr, Some(addr));
    assert!(stats.packets_received >= 4 && stats.packets_sent >= 4);
    assert!(stats.bytes_received > 20 && stats.bytes_sent > 20);
    c.remove_client(cid).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn failed_action_duplicate_reply_and_no_action_fail_closed() {
    for (actions, code) in [
        (
            vec![
                json!({"type":"respond_diameter_aa","reply":{"verdict":"accept"}}),
                json!({"type":"respond_diameter_aa","reply":{"verdict":"accept","service_type":20}}),
            ],
            5012,
        ),
        (
            vec![json!({"type":"respond_diameter_aa","reply":{"verdict":"accept"}}); 2],
            5012,
        ),
        (vec![], 4001),
    ] {
        let (state,id,addr,_)=server(Some(vec![json!({"event_pattern":"diameter_aa_request","handler":{"type":"static","actions":actions}})]),None).await;
        let mut peer = negotiated(addr).await;
        let p = request();
        send(&mut peer, &p).await;
        let answer = read(&mut peer).await;
        assert!(answer.matches(&p));
        assert_eq!(answer.num(RESULT).unwrap(), code);
        assert_eq!(answer.flags & 0x20, 0);
        state.remove_server(id).await;
    }
}
#[tokio::test]
async fn parked_manual_handler_still_answers_watchdog_rejects_second_aaa_and_disconnects() {
    let (state, id, addr, _) = server(
        Some(vec![
            json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let mut peer = negotiated(addr).await;
    send(&mut peer, &request()).await;
    intercept(&state).await;
    let mut wd = Packet::request(DWR, 0).unwrap();
    wd.origin(&identity("client.example"));
    send(&mut peer, &wd).await;
    assert!(read(&mut peer).await.matches(&wd));
    let second = request();
    send(&mut peer, &second).await;
    let busy = read(&mut peer).await;
    assert!(busy.matches(&second));
    assert_eq!(busy.num(RESULT).unwrap(), 3004);
    assert_eq!(busy.flags & 0x20, 0x20);
    let mut dpr = Packet::request(DPR, 0).unwrap();
    dpr.origin(&identity("client.example"));
    dpr.avps.push(Avp::number(DISCONNECT_CAUSE, 0));
    send(&mut peer, &dpr).await;
    let dpa = read(&mut peer).await;
    assert!(dpa.matches(&dpr));
    assert_eq!(dpa.num(RESULT).unwrap(), 2001);
    eof(&mut peer).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await;
}
#[tokio::test]
async fn stopping_registered_server_cancels_parked_handler_and_owned_socket() {
    let (state, id, addr, _) = server(
        Some(vec![
            json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let mut peer = negotiated(addr).await;
    send(&mut peer, &request()).await;
    intercept(&state).await;
    state.remove_server(id).await;
    eof(&mut peer).await;
    assert!(state.list_intercepts().await.is_empty());
}
#[tokio::test]
async fn peer_eof_cancels_parked_manual_handler() {
    let (state, id, addr, _) = server(
        Some(vec![
            json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let mut peer = negotiated(addr).await;
    send(&mut peer, &request()).await;
    intercept(&state).await;
    drop(peer);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await;
}
#[tokio::test]
async fn malformed_semantics_optional_extensions_and_unknown_mandatory_avps_are_bounded() {
    let (state, id, addr, _) = server(Some(policy("accept")), None).await;
    let mut peer = negotiated(addr).await;
    for mutation in 0..8 {
        let mut p = request();
        match mutation {
            0 => {
                p.avps
                    .iter_mut()
                    .find(|v| v.code == AUTH_STATE)
                    .unwrap()
                    .data = 0u32.to_be_bytes().to_vec()
            }
            1 => p.avps.push(Avp::number(AUTH_TYPE, 3)),
            2 => {
                p.avps.iter_mut().find(|v| v.code == HOST).unwrap().data = b"other.example".to_vec()
            }
            3 => {
                p.avps
                    .iter_mut()
                    .find(|v| v.code == DEST_REALM)
                    .unwrap()
                    .data = b"other".to_vec()
            }
            4 => p.avps.push(Avp::number(999999, 1)),
            5 => p.avps.push(Avp {
                code: 999999,
                flags: 0,
                vendor: None,
                data: vec![1],
            }),
            6 => p.avps.push(Avp {
                code: 999999,
                flags: 0xc0,
                vendor: Some(123),
                data: vec![1],
            }),
            _ => p.avps.iter_mut().find(|v| v.code == PASSWORD).unwrap().data = vec![b'x'; 129],
        }
        send(&mut peer, &p).await;
        let a = read(&mut peer).await;
        assert!(a.matches(&p));
        let code = match mutation {
            4 | 6 => 5001,
            5 => 2001,
            _ => 5012,
        };
        assert_eq!(a.num(RESULT).unwrap(), code);
        if code == 5001 {
            assert!(a
                .one(FAILED_AVP)
                .unwrap()
                .data
                .starts_with(&999999u32.to_be_bytes()));
        }
    }
    assert_eq!(state.list_access_logs(None).await.len(), 1);
    state.remove_server(id).await;
}
#[tokio::test]
async fn whole_frame_bounds_and_initial_packet_deadline_close_owned_connection() {
    let (state, id, addr, _) = server(
        None,
        Some(
            json!({"origin_host":"server.example","origin_realm":"example","io_timeout_seconds":1}),
        ),
    )
    .await;
    let mut silent = TcpStream::connect(addr).await.unwrap();
    eof(&mut silent).await;
    let mut oversized = TcpStream::connect(addr).await.unwrap();
    let mut h = [0; 20];
    h[0] = 1;
    let size = (MAX_FRAME_BYTES + 4) as u32;
    h[1..4].copy_from_slice(&size.to_be_bytes()[1..]);
    oversized.write_all(&h).await.unwrap();
    eof(&mut oversized).await;
    let mut partial = TcpStream::connect(addr).await.unwrap();
    partial.write_all(&[1, 0]).await.unwrap();
    eof(&mut partial).await;
    state.remove_server(id).await;
}
#[tokio::test]
async fn native_watchdog_has_strict_identifier_correlation() {
    let (state,id,addr,_)=server(None,Some(json!({"origin_host":"server.example","origin_realm":"example","watchdog_interval_seconds":1,"io_timeout_seconds":5}))).await;
    let mut peer = negotiated(addr).await;
    let wd = read(&mut peer).await;
    assert_eq!(wd.command, DWR);
    assert!(wd.is_request());
    let mut bad = wd.answer(2001);
    bad.origin(&identity("client.example"));
    bad.hop ^= 1;
    send(&mut peer, &bad).await;
    eof(&mut peer).await;
    state.remove_server(id).await;
}
#[tokio::test]
async fn backend_outage_handler_deadline_and_private_script_failure_never_accept() {
    for (handlers, params) in [
        (
            None,
            json!({"origin_host":"server.example","origin_realm":"example","llm_fallback":true,"handler_timeout_seconds":1}),
        ),
        (
            Some(vec![
                json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
            ]),
            json!({"origin_host":"server.example","origin_realm":"example","handler_timeout_seconds":1}),
        ),
        (
            Some(vec![
                json!({"event_pattern":"diameter_aa_request","handler":{"type":"script","language":"python","code":"import json,sys\np=json.load(sys.stdin)['event']['request']['password']\nsys.stderr.write(p)\nprint('{malformed '+p)"}}),
            ]),
            json!({"origin_host":"server.example","origin_realm":"example"}),
        ),
    ] {
        let (state, id, addr, mut status) = server(handlers, Some(params)).await;
        let mut peer = negotiated(addr).await;
        send(&mut peer, &request()).await;
        let p = read(&mut peer).await;
        assert_eq!(p.num(RESULT).unwrap(), 5012);
        let mut diagnostics = String::new();
        while let Ok(line) = status.try_recv() {
            diagnostics.push_str(&line);
        }
        assert!(!diagnostics.contains("Correct"), "{diagnostics}");
        state.remove_server(id).await;
    }
}
#[test]
fn constructed_result_failures_precede_success_and_deep_results_are_safely_consumed() {
    use netget::llm::actions::{
        executor::{ActionFailure, ExecutionResult},
        protocol_trait::ActionResult,
    };
    let r = ExecutionResult {
        failures: vec![ActionFailure {
            index: 1,
            action: "bad".into(),
            error: "failed".into(),
        }],
        protocol_results: vec![ActionResult::Custom {
            name: "respond_diameter_aa".into(),
            data: json!({"verdict":"accept"}),
        }],
        ..Default::default()
    };
    assert!(netget::server::diameter::chosen_reply(r).is_err());
    let mut deep = serde_json::Value::Null;
    for _ in 0..10000 {
        deep = serde_json::Value::Array(vec![deep]);
    }
    let r = ExecutionResult {
        protocol_results: vec![ActionResult::Custom {
            name: "respond_diameter_aa".into(),
            data: deep,
        }],
        ..Default::default()
    };
    assert!(netget::server::diameter::chosen_reply(r).is_err());
}
