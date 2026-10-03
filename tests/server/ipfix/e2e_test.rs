use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc};
pub(super) async fn start(
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "ipfix".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some("Default must not call the model".into()),
        event_handlers: handlers,
        startup_params: params,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (state, id, addr)
}
pub(super) async fn logs(
    state: &AppState,
    id: ServerId,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut logs = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if logs.len() >= count {
                logs.sort_by_key(|e| e.id);
                break logs;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
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
    .expect("aborted owned parser/handler must release the socket and intercept");
}
#[tokio::test]
async fn collector_keeps_wire_state_without_default_model_and_never_replies() {
    let (state, id, addr) = start(None, None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tpl = super::codec_test::packet(42, 0, &[(2, super::codec_test::template(300, &[(8, 4)]))]);
    sender.send_to(&tpl, addr).await.unwrap();
    logs(&state, id, "ipfix_message", 1).await;
    let d = super::codec_test::packet(42, 0, &[(300, vec![192, 0, 2, 1])]);
    sender.send_to(&d, addr).await.unwrap();
    let e = logs(&state, id, "ipfix_message", 2).await;
    assert_eq!(
        e[1].request["message"]["data_sets"][0]["records"][0][0],
        json!({"kind":"ipv4","value":"192.0.2.1"})
    );
    assert_eq!(
        e[1].request["message"]["sequence_tracking"]["status"],
        "in_order"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sender.recv(&mut [0; 1]))
            .await
            .is_err()
    );
    state.remove_server(id).await;
    released(&state, addr).await;
}
#[tokio::test]
async fn malformed_and_oversized_datagrams_log_failure_without_dispatch_and_recover() {
    let (state, id, addr) = start(None, None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for b in [
        vec![0; 15],
        vec![0; 8193],
        super::codec_test::packet(42, 0, &[(2, super::codec_test::template(300, &[(8, 3)]))]),
    ] {
        sender.send_to(&b, addr).await.unwrap();
    }
    let e = logs(&state, id, "ipfix_invalid_datagram", 3).await;
    assert!(e
        .iter()
        .all(|e| e.response[0]["decision"] == "fail_closed_invalid_datagram"));
    assert!(!state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .any(|e| e.event_type == "ipfix_message"));
    let w = netget::server::ipfix::codec::encode(&super::codec_test::sample(), 0, 0)
        .unwrap()
        .0;
    sender.send_to(&w, addr).await.unwrap();
    assert_eq!(
        logs(&state, id, "ipfix_message", 1).await[0].request["message"]["record_count"],
        1
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn parked_handler_does_not_block_receive_expiry_or_bounded_queue_and_stop() {
    let (state, id, addr) = start(
        Some(vec![
            json!({"event_pattern":"ipfix_message","handler":{"type":"manual","timeout_secs":300}}),
        ]),
        Some(json!({"template_ttl_seconds":1})),
    )
    .await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let w = netget::server::ipfix::codec::encode(&super::codec_test::sample(), 0, 0)
        .unwrap()
        .0;
    sender.send_to(&w, addr).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..33 {
        sender.send_to(&w, addr).await.unwrap();
    }
    logs(&state, id, "ipfix_event_capacity", 1).await;
    sender.send_to(&[0; 15], addr).await.unwrap();
    logs(&state, id, "ipfix_invalid_datagram", 1).await;
    state.remove_server(id).await;
    released(&state, addr).await;
}
#[tokio::test]
async fn actual_template_expiry_and_redefinition_follow_wire_order() {
    let (state, id, addr) = start(None, Some(json!({"template_ttl_seconds":1}))).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let t = super::codec_test::packet(1, 0, &[(2, super::codec_test::template(300, &[(8, 4)]))]);
    sender.send_to(&t, addr).await.unwrap();
    logs(&state, id, "ipfix_message", 1).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    let d = super::codec_test::packet(1, 0, &[(300, vec![192, 0, 2, 1])]);
    sender.send_to(&d, addr).await.unwrap();
    let e = logs(&state, id, "ipfix_message", 2).await;
    assert_eq!(
        e[1].request["message"]["unknown_data_sets"][0]["template_id"],
        300
    );
    let t = super::codec_test::packet(
        1,
        0,
        &[
            (2, super::codec_test::template(300, &[(7, 2)])),
            (300, vec![1, 187]),
        ],
    );
    sender.send_to(&t, addr).await.unwrap();
    let e = logs(&state, id, "ipfix_message", 3).await;
    assert_eq!(
        e[2].request["message"]["data_sets"][0]["records"][0][0],
        json!({"kind":"unsigned","value":443})
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn common_script_memory_and_failed_action_remain_silent() {
    let code="import json,sys\nr=json.load(sys.stdin)\nassert r['event']['message']['record_count']==1\nprint(json.dumps({'actions':[{'type':'set_memory','value':'IPFIX observed'},{'type':'collect_ipfix_records'},{'type':'unknown_ipfix_action'}]}))";
    let(state,id,addr)=start(Some(vec![json!({"event_pattern":"ipfix_message","handler":{"type":"script","language":"python","code":code}})]),None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let w = netget::server::ipfix::codec::encode(&super::codec_test::sample(), 0, 0)
        .unwrap()
        .0;
    sender.send_to(&w, addr).await.unwrap();
    let e = logs(&state, id, "ipfix_handler_failed", 1).await;
    assert_eq!(e[0].response[0]["decision"], "fail_closed_handler_error");
    assert_eq!(state.get_memory(id).await.unwrap(), "IPFIX observed");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sender.recv(&mut [0; 1]))
            .await
            .is_err()
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn explicit_llm_opt_in_uses_standard_typed_event_dispatcher(
) -> crate::server::helpers::E2EResult<()> {
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    let config=NetGetConfig::new("listen on port {AVAILABLE_PORT} via ipfix. Review flows.").with_mock(|mock| {
        mock.on_instruction_containing("via ipfix").respond_with_actions(json!([{"type":"open_server","base_stack":"ipfix","port":0,"instruction":"Review flows","startup_params":{"llm_fallback":true}}])).expect_calls(1).and()
        .on_event("ipfix_message").respond_with_actions_from_event(|event| {
            assert_eq!(event["message"]["observation_domain_id"],42);
            assert_eq!(event["message"]["record_count"],1);
            assert_eq!(event["message"]["data_sets"][0]["records"][0][0],json!({"kind":"ipv4","value":"192.0.2.1"}));
            json!([{"type":"collect_ipfix_records"}])
        }).expect_calls(1).and()
    });
    let server = start_netget_server(config).await?;
    let sender = UdpSocket::bind("127.0.0.1:0").await?;
    let w = netget::server::ipfix::codec::encode(&super::codec_test::sample(), 0, 0)?.0;
    sender
        .send_to(&w, format!("127.0.0.1:{}", server.port))
        .await?;
    server.wait_for_mocks(20).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
