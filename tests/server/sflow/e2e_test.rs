use netget::state::{app_state::AppState, AccessLogOwner};
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc};

pub(crate) use crate::helpers::sflow::{logs, start};
async fn decision(rx: &mut mpsc::UnboundedReceiver<String>, tag: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = rx.recv().await {
            if line.contains(&format!("decision={tag} ")) {
                assert!(line.contains("udp_silent=true"));
                return;
            }
        }
        panic!("missing actual terminal log {tag}");
    })
    .await
    .unwrap();
}
async fn released(state: &AppState, addr: SocketAddr) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state.list_intercepts().await.is_empty() && UdpSocket::bind(addr).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned tasks must release socket and parked intercept");
}
#[tokio::test]
async fn typed_default_collection_is_silent_and_uses_no_backend() {
    let (state, id, addr, mut status) = start(None, None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    let e = logs(&state, id, "sflow_message", 1).await;
    assert_eq!(e[0].request["message"]["record_count"], 4);
    assert_eq!(
        e[0].request["message"]["samples"][3]["records"][0]["octets"],
        9007199254740999u64
    );
    decision(&mut status, "default_collect").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sender.recv(&mut [0; 1]))
            .await
            .is_err()
    );
    state.remove_server(id).await;
    released(&state, addr).await;
}
#[tokio::test]
async fn malformed_oversized_datagrams_fail_closed_without_partial_dispatch_then_recover() {
    let (state, id, addr, mut status) = start(None, None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for bytes in [vec![0; 27], vec![0; 8193], {
        let mut b = crate::helpers::sflow::golden();
        b.pop();
        b
    }] {
        sender.send_to(&bytes, addr).await.unwrap();
    }
    let errors = logs(&state, id, "sflow_invalid_datagram", 3).await;
    assert!(errors
        .iter()
        .all(|e| e.response[0]["decision"] == "fail_closed_invalid_datagram"));
    decision(&mut status, "fail_closed_invalid_datagram").await;
    assert!(!state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .any(|e| e.event_type == "sflow_message"));
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    assert_eq!(
        logs(&state, id, "sflow_message", 1).await[0].request["message"]["sequence_tracking"]
            ["status"],
        "untracked"
    );
    state.remove_server(id).await;
    released(&state, addr).await;
}
#[tokio::test]
async fn parked_handler_preserves_parser_capacity_and_owned_socket_intercept_cleanup() {
    let (state, id, addr, mut status) = start(
        Some(vec![
            json!({"event_pattern":"sflow_message","handler":{"type":"manual","timeout_secs":300}}),
        ]),
        None,
    )
    .await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..33 {
        sender
            .send_to(&crate::helpers::sflow::golden(), addr)
            .await
            .unwrap();
    }
    logs(&state, id, "sflow_event_capacity", 1).await;
    decision(&mut status, "fail_closed_event_capacity").await;
    sender.send_to(&[0; 27], addr).await.unwrap();
    logs(&state, id, "sflow_invalid_datagram", 1).await;
    state.remove_server(id).await;
    released(&state, addr).await;
}
#[tokio::test]
async fn aged_parked_datagram_row_survives_common_cleanup_until_the_handler_finishes() {
    let (state, id, addr, _) = start(
        Some(vec![
            json!({"event_pattern":"sflow_message","handler":{"type":"manual","timeout_secs":300}}),
        ]),
        None,
    )
    .await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(request) = state.list_intercepts().await.into_iter().next() {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let cid = netget::server::connection::ConnectionId::new(request.connection_id.unwrap());
    state
        .with_server_mut(id, |server| {
            server.connections.get_mut(&cid).unwrap().last_activity =
                std::time::Instant::now() - Duration::from_secs(60);
        })
        .await
        .unwrap();
    state.cleanup_old_connections(10).await;
    assert!(
        state
            .get_server(id)
            .await
            .unwrap()
            .connections
            .contains_key(&cid),
        "the idle sweep must preserve a live parked sFlow request"
    );
    assert_eq!(state.list_intercepts().await[0].id, request.id);
    state
        .resolve_intercept(request.id, vec![json!({"type":"collect_sflow_samples"})])
        .await
        .unwrap();
    logs(&state, id, "sflow_handler_decision", 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state
            .get_server(id)
            .await
            .unwrap()
            .connections
            .contains_key(&cid)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the dispatcher must remove its row when the handler finishes");
    assert!(state.list_intercepts().await.is_empty());
    state.remove_server(id).await;
    released(&state, addr).await;
}
#[tokio::test]
async fn live_idle_expiry_resets_sequence_expectation() {
    let (state, id, addr, _) = start(None, Some(json!({"session_idle_seconds":1}))).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    logs(&state, id, "sflow_message", 1).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    assert_eq!(
        logs(&state, id, "sflow_message", 2).await[1].request["message"]["sequence_tracking"]
            ["status"],
        "untracked"
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn common_memory_and_successful_collect_do_not_hide_failed_action() {
    let code="import json,sys\nr=json.load(sys.stdin)\nassert r['event']['message']['record_count']==4\nprint(json.dumps({'actions':[{'type':'set_memory','value':'telemetry seen'},{'type':'collect_sflow_samples'},{'type':'unknown_sflow_action'}]}))";
    let (state,id,addr,mut status)=start(Some(vec![json!({"event_pattern":"sflow_message","handler":{"type":"script","language":"python","code":code}})]),None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    let e = logs(&state, id, "sflow_handler_failed", 1).await;
    assert_eq!(e[0].response[0]["decision"], "fail_closed_action_error");
    assert_eq!(state.get_memory(id).await.unwrap(), "telemetry seen");
    decision(&mut status, "fail_closed_action_error").await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sender.recv(&mut [0; 1]))
            .await
            .is_err()
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn explicit_empty_collect_and_backend_error_have_terminal_decisions() {
    for (actions, tag) in [
        (json!([]), "handler_silent"),
        (json!([{"type":"collect_sflow_samples"}]), "handler_collect"),
    ] {
        let (state,id,addr,mut status)=start(Some(vec![json!({"event_pattern":"sflow_message","handler":{"type":"static","actions":actions}})]),None).await;
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender
            .send_to(&crate::helpers::sflow::golden(), addr)
            .await
            .unwrap();
        assert_eq!(
            logs(&state, id, "sflow_handler_decision", 1).await[0].response[0]["decision"],
            tag
        );
        decision(&mut status, tag).await;
        state.remove_server(id).await;
    }
    let (state, id, addr, mut status) = start(None, Some(json!({"llm_fallback":true}))).await;
    state
        .set_ollama_model(Some("sflow-test-model".into()))
        .await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(&crate::helpers::sflow::golden(), addr)
        .await
        .unwrap();
    assert_eq!(
        logs(&state, id, "sflow_handler_failed", 1).await[0].response[0]["decision"],
        "fail_closed_dispatch_error"
    );
    decision(&mut status, "fail_closed_dispatch_error").await;
    state.remove_server(id).await;
}
