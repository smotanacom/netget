use crate::helpers::diameter::{aa, client, disconnected, identity, intercept, policy, server};
use netget::{server::diameter::codec::*, state::client_handles::ClientSendOutcome};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
};
async fn read(peer: &mut TcpStream) -> Packet {
    read_packet(peer, Duration::from_secs(5)).await.unwrap()
}
async fn send(peer: &mut TcpStream, p: &Packet) {
    write_packet(peer, p).await.unwrap();
}
async fn negotiate(listener: TcpListener) -> TcpStream {
    let (mut peer, _) = listener.accept().await.unwrap();
    let cer = read(&mut peer).await;
    let mut cea = cer.answer(2001);
    capability_fields(
        &mut cea,
        &identity("server.example"),
        "127.0.0.1".parse().unwrap(),
    );
    send(&mut peer, &cea).await;
    peer
}
#[tokio::test]
async fn bad_capability_answers_never_register_handle_or_emit_connected_event() {
    use netget::{cli::management::ClientForm, llm::OllamaClient};
    for mutation in 0..5 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let cer = read(&mut socket).await;
            let mut cea = cer.answer(2001);
            capability_fields(
                &mut cea,
                &identity("server.example"),
                "127.0.0.1".parse().unwrap(),
            );
            match mutation {
                0 => cea.hop ^= 1,
                1 => cea.end ^= 1,
                2 => cea.avps.retain(|a| a.code != AUTH_APP),
                3 => cea.avps.retain(|a| a.code != HOST_IP),
                _ => {
                    cea.avps.iter_mut().find(|a| a.code == RESULT).unwrap().data =
                        5010u32.to_be_bytes().to_vec()
                }
            }
            send(&mut socket, &cea).await;
            let mut b = [0; 1];
            let result = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut b))
                .await
                .unwrap();
            assert!(matches!(result, Ok(0)) || result.is_err());
        });
        let state = crate::helpers::diameter::state();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let result = ClientForm {
            protocol: "diameter".into(),
            remote_addr: Some(addr.to_string()),
            instruction: Some(String::new()),
            startup_params: Some(json!({"origin_host":"client.example","origin_realm":"example"})),
            ..Default::default()
        }
        .create(&state, OllamaClient::new("http://127.0.0.1:1"), tx)
        .await;
        assert!(result.is_err(), "capability mutation {mutation}");
        assert!(state.list_access_logs(None).await.is_empty());
        assert!(state.get_all_clients().await.is_empty());
        peer.await.unwrap();
    }
}
#[tokio::test]
async fn injection_bypasses_parked_connected_handler_and_rejects_second_pending_request() {
    let (state, sid, addr, _) = server(
        Some(vec![
            json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let (c, cid) = client(
        addr.to_string(),
        Some(vec![
            json!({"event_pattern":"diameter_connected","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    intercept(&c).await;
    let send_state = c.clone();
    let first = tokio::spawn(async move {
        send_state
            .send_to_client(cid, aa("Correct", 3), Duration::from_secs(10))
            .await
    });
    intercept(&state).await;
    assert!(matches!(
        c.send_to_client(cid, aa("wrong", 3), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(matches!(
        c.send_to_client(cid, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    assert!(first.await.unwrap().is_err());
    disconnected(&c, cid).await;
    assert!(c.list_intercepts().await.is_empty());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    c.remove_client(cid).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn removal_cancels_pending_request_and_server_handler_without_replay() {
    let (state, sid, addr, _) = server(
        Some(vec![
            json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let (c, cid) = client(addr.to_string(), None, None).await;
    let sender = c.clone();
    let pending = tokio::spawn(async move {
        sender
            .send_to_client(cid, aa("Correct", 3), Duration::from_secs(10))
            .await
    });
    intercept(&state).await;
    c.remove_client(cid).await;
    assert!(pending.await.unwrap().is_err());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state
            .list_access_logs(None)
            .await
            .iter()
            .filter(|e| e.event_type == "diameter_aa_request")
            .count(),
        0
    );
    state.remove_server(sid).await;
}
#[tokio::test]
async fn parked_client_handler_answers_real_watchdog_and_disconnect_control() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = tokio::spawn(negotiate(listener));
    let (state, id) = client(
        addr.to_string(),
        Some(vec![
            json!({"event_pattern":"diameter_connected","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let mut peer = accept.await.unwrap();
    intercept(&state).await;
    for command in [DWR, DPR] {
        let mut p = Packet::request(command, 0).unwrap();
        p.origin(&identity("server.example"));
        if command == DPR {
            p.avps.push(Avp::number(DISCONNECT_CAUSE, 0));
        }
        send(&mut peer, &p).await;
        let a = read(&mut peer).await;
        assert!(a.matches(&p));
        assert_eq!(a.num(RESULT).unwrap(), 2001);
    }
    disconnected(&state, id).await;
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
}
#[tokio::test]
async fn uncorrelated_stateful_missing_or_unsupported_successful_answers_never_accept() {
    for mutation in 0..10 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(negotiate(listener));
        let (state, id) = client(addr.to_string(), None, None).await;
        let mut peer = accept.await.unwrap();
        let sender = state.clone();
        let pending = tokio::spawn(async move {
            sender
                .send_to_client(id, aa("Correct", 3), Duration::from_secs(10))
                .await
        });
        let p = read(&mut peer).await;
        let mut a = Reply {
            verdict: Verdict::Accept,
            ..Default::default()
        }
        .packet(&p, &identity("server.example"))
        .unwrap();
        match mutation {
            0 => a.hop ^= 1,
            1 => a.end ^= 1,
            2 => a.command = DWR,
            3 => a.application = 0,
            4 => a.avps.retain(|v| v.code != AUTH_STATE),
            5 => {
                a.avps
                    .iter_mut()
                    .find(|v| v.code == AUTH_STATE)
                    .unwrap()
                    .data = 0u32.to_be_bytes().to_vec()
            }
            6 => {
                a.avps.iter_mut().find(|v| v.code == SESSION).unwrap().data =
                    b"wrong-session".to_vec()
            }
            7 => a.avps.push(Avp::number(999999, 1)),
            8 => a.avps.push(Avp::number(RESULT, 2001)),
            _ => {
                a.avps.iter_mut().find(|v| v.code == HOST).unwrap().data = b"wrong.example".to_vec()
            }
        }
        send(&mut peer, &a).await;
        assert!(pending.await.unwrap().is_err(), "mutation{mutation}");
        disconnected(&state, id).await;
        assert!(!state
            .list_access_logs(None)
            .await
            .iter()
            .any(|e| e.event_type == "diameter_aa_result"));
        let mut b = [0; 1];
        let closed = tokio::time::timeout(Duration::from_secs(5), peer.read(&mut b))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0)) || closed.is_err());
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn constructed_injected_actions_are_rejected_safely_and_valid_control_still_works() {
    let (s, sid, addr, _) = server(Some(policy("accept")), None).await;
    let (c, cid) = client(addr.to_string(), None, None).await;
    let mut deep = serde_json::Value::Null;
    for _ in 0..10000 {
        deep = serde_json::Value::Array(vec![deep]);
    }
    assert!(matches!(
        c.send_to_client(cid, deep, Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(matches!(
        c.send_to_client(cid, aa("Correct", 3), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
    c.remove_client(cid).await;
    s.remove_server(sid).await;
}
#[tokio::test]
async fn per_request_deadline_closes_pending_operation_without_replay() {
    let (s, sid, addr, _) = server(
        Some(vec![
            json!({"event_pattern":"diameter_aa_request","handler":{"type":"manual"}}),
        ]),
        None,
    )
    .await;
    let (c, cid) = client(
        addr.to_string(),
        None,
        Some(
            json!({"origin_host":"client.example","origin_realm":"example","io_timeout_seconds":1}),
        ),
    )
    .await;
    let sender = c.clone();
    let pending = tokio::spawn(async move {
        sender
            .send_to_client(cid, aa("Correct", 3), Duration::from_secs(5))
            .await
    });
    intercept(&s).await;
    assert!(pending.await.unwrap().is_err());
    disconnected(&c, cid).await;
    c.remove_client(cid).await;
    s.remove_server(sid).await;
}
