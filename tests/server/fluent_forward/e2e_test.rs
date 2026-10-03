use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

pub(super) async fn start(
    handlers: Option<Vec<Value>>,
    params: Option<Value>,
) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "fluent-forward".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some("This instruction must not trigger default model calls".into()),
        event_handlers: handlers,
        startup_params: params,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Forward bind");
    (state, id, addr)
}
pub(super) async fn logs(
    state: &AppState,
    id: ServerId,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let logs = state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await;
            if logs.len() >= count {
                break logs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Forward access log")
}

pub(super) fn batch(mode: &str, ack: bool) -> netget::server::fluent_forward::codec::Batch {
    serde_json::from_value(json!({"tag":"demo.logs","entries":[{"timestamp":{"seconds":1700000000,"nanoseconds":250000000},"record":{"message":"温度","n":42}}],"mode":mode,"require_ack":ack})).unwrap()
}
pub(super) async fn ack(peer: &mut TcpStream) -> String {
    let mut d = netget::server::fluent_forward::codec::Decoder::default();
    let mut buf = [0; 8192];
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(n) = d.next_node().unwrap() {
                break netget::server::fluent_forward::codec::parse_ack(n).unwrap();
            }
            let n = peer.read(&mut buf).await.unwrap();
            assert!(n > 0);
            d.feed(&buf[..n]).unwrap();
        }
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn all_carriers_fragment_coalesce_ack_and_default_model_suppression() {
    let (state, id, addr) = start(None, None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    for mode in ["message", "forward", "packed", "compressed_packed"] {
        let wire =
            netget::server::fluent_forward::codec::encode_batch(&batch(mode, true), Some(mode))
                .unwrap();
        peer.write_all(&wire[..5]).await.unwrap();
        peer.write_all(&wire[5..]).await.unwrap();
        assert_eq!(ack(&mut peer).await, mode);
    }
    let entries = logs(&state, id, 4).await;
    for mode in ["message", "forward", "packed", "compressed_packed"] {
        let e = entries.iter().find(|e| e.request["mode"] == mode).unwrap();
        assert_eq!(e.request["entries"][0]["record"]["message"], "温度");
        assert_eq!(
            e.request["entries"][0]["timestamp"]["nanoseconds"],
            250000000
        );
        assert_eq!(e.request["ack_requested"], true);
        assert!(e.request.get("chunk").is_none());
    }
    state.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    tokio::net::TcpListener::bind(addr).await.unwrap();
}
#[tokio::test]
async fn bad_bombs_and_incomplete_frames_close_sender_only() {
    let (state, id, addr) = start(None, None).await;
    for wire in [
        vec![0xc1],
        vec![0xdb, 0, 4, 0, 1],
        vec![0xc6, 0, 4, 0, 0],
        vec![0x91; 34],
    ] {
        let mut peer = TcpStream::connect(addr).await.unwrap();
        peer.write_all(&wire).await.unwrap();
        peer.shutdown().await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
    logs(&state, id, 4).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(
        &netget::server::fluent_forward::codec::encode_batch(
            &batch("forward", true),
            Some("healthy"),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(ack(&mut peer).await, "healthy");
    state.remove_server(id).await;
}
#[tokio::test]
async fn static_script_reject_and_memory_handlers_and_manual_stop() {
    for handler in [
        json!({"type":"static","actions":[]}),
        json!({"type":"script","language":"python","code":"import json,sys\nx=json.load(sys.stdin)\nassert x['event']['entries'][0]['record']['message']=='温度'\nprint(json.dumps({'actions':[{'type':'set_memory','value':'observed'},{'type':'accept_forward_batch'}]}))"}),
    ] {
        let script = handler["type"] == "script";
        let (state, id, addr) = start(
            Some(vec![
                json!({"event_pattern":"forward_batch","handler":handler}),
            ]),
            None,
        )
        .await;
        let mut peer = TcpStream::connect(addr).await.unwrap();
        peer.write_all(
            &netget::server::fluent_forward::codec::encode_batch(
                &batch("packed", true),
                Some("matched"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(ack(&mut peer).await, "matched");
        logs(&state, id, 1).await;
        if script {
            assert_eq!(state.get_server(id).await.unwrap().memory, "observed");
        }
        state.remove_server(id).await;
    }
    for handler in [
        json!({"type":"static","actions":[{"type":"reject_forward_batch"}]}),
        json!({"type":"manual","timeout_secs":300}),
    ] {
        let manual = handler["type"] == "manual";
        let (state, id, addr) = start(
            Some(vec![
                json!({"event_pattern":"forward_batch","handler":handler}),
            ]),
            None,
        )
        .await;
        let mut peer = TcpStream::connect(addr).await.unwrap();
        peer.write_all(
            &netget::server::fluent_forward::codec::encode_batch(
                &batch("message", true),
                Some("reject"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        if manual {
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.list_intercepts().await.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            state.remove_server(id).await;
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        state.remove_server(id).await;
        assert!(state.list_intercepts().await.is_empty());
    }
}

#[tokio::test]
async fn trickled_partial_forward_does_not_reset_absolute_read_deadline() {
    let (state, id, addr) = start(None, None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(&[0xdb]).await.unwrap();
    for bytes in [1, 2] {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state
                    .get_server(id)
                    .await
                    .unwrap()
                    .connections
                    .values()
                    .any(|c| c.bytes_received >= bytes)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if bytes == 1 {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(29)).await;
            tokio::time::resume();
            peer.write_all(&[0]).await.unwrap();
        }
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(logs(&state, id, 1).await[0].response[0]["error"]
        .as_str()
        .unwrap()
        .contains("deadline"));
    state.remove_server(id).await;
}

#[tokio::test]
async fn valid_accept_with_invalid_action_closes_without_ack() {
    let(state,id,addr)=start(Some(vec![json!({"event_pattern":"forward_batch","handler":{"type":"script","language":"python","code":"import json,sys\nx=json.load(sys.stdin)\nassert x['event']['tag']=='demo.logs'\nprint(json.dumps({'actions':[{'type':'accept_forward_batch'},{'type':'unknown_forward_action'}]}))"}})]),None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(
        &netget::server::fluent_forward::codec::encode_batch(
            &batch("forward", true),
            Some("must-not-ack"),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let e = logs(&state, id, 2).await;
    assert!(e.iter().any(|e| e.event_type == "forward_handler_failed"
        && e.response
            == vec![
                json!({"decision":"fail_closed_handler_action_error","failed_action_count":1})
            ]));
    assert!(e.iter().any(|e| e.event_type == "forward_invalid_stream"
        && e.response[0]["error"]
            .as_str()
            .unwrap()
            .contains("handler action failed")));
    state.remove_server(id).await;
}

#[tokio::test]
async fn explicitly_opted_in_forward_batch_reaches_standard_model_dispatcher(
) -> crate::server::helpers::E2EResult<()> {
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    let config=NetGetConfig::new("listen on port {AVAILABLE_PORT} via fluent-forward. Summarize logs.").with_mock(|mock| {
  mock.on_instruction_containing("via fluent-forward").respond_with_actions(json!([{"type":"open_server","base_stack":"fluent-forward","port":0,"instruction":"Summarize logs","startup_params":{"llm_fallback":true}}])).expect_calls(1).and()
  .on_event("forward_batch").respond_with_actions_from_event(|event|{assert_eq!(event["tag"],"demo.logs");json!([{"type":"accept_forward_batch"}])}).expect_calls(1).and()
 });
    let server = start_netget_server(config).await?;
    let mut peer = TcpStream::connect(("127.0.0.1", server.port)).await?;
    peer.write_all(&netget::server::fluent_forward::codec::encode_batch(
        &batch("forward", true),
        Some("model"),
    )?)
    .await?;
    assert_eq!(ack(&mut peer).await, "model");
    server.wait_for_mocks(20).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn netget_pair_ack_followups_use_current_memory_and_stop_at_depth_bound() {
    let (state, server, addr) = start(None, None).await;
    let (tx, _) = mpsc::unbounded_channel();
    let b = serde_json::to_value(batch("packed", true)).unwrap();
    let code=format!("import json,sys\nx=json.load(sys.stdin)\nn=int(x['client']['memory'] or '0')+1\nassert x['event']['tag']=='demo.logs'\nprint(json.dumps({{'actions':[{{'type':'set_memory','value':str(n)}},{{'type':'send_forward_batch','batch':json.loads({})}}]}}))",serde_json::to_string(&b.to_string()).unwrap());
    let id=netget::cli::management::ClientForm{protocol:"fluent-forward".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"forward_connected","handler":{"type":"static","actions":[{"type":"send_forward_batch","batch":b}]}}),json!({"event_pattern":"forward_ack","handler":{"type":"script","language":"python","code":code}})]),..Default::default()}.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while state.get_client(id).await.unwrap().memory != "8" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let e = logs(&state, server, 8).await;
    assert_eq!(
        e.iter().filter(|e| e.event_type == "forward_batch").count(),
        8
    );
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
    state.remove_server(server).await;
}

#[tokio::test]
async fn connection_cap_refuses_excess_and_invalid_stream_releases_slot() {
    use netget::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;
    let (state, id, addr) = start(None, None).await;
    let mut peers = Vec::new();
    for _ in 0..DEFAULT_MAX_CONNECTIONS {
        peers.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.get_server(id).await.unwrap().connections.len() != DEFAULT_MAX_CONNECTIONS {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut excess = TcpStream::connect(addr).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), excess.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    peers[0].write_all(&[0xc1]).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peers[0].read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.get_server(id).await.unwrap().connections.len() != DEFAULT_MAX_CONNECTIONS - 1 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let mut replacement = TcpStream::connect(addr).await.unwrap();
    replacement
        .write_all(
            &netget::server::fluent_forward::codec::encode_batch(
                &batch("forward", true),
                Some("replacement"),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ack(&mut replacement).await, "replacement");
    state.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), replacement.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    tokio::net::TcpListener::bind(addr).await.unwrap();
}

#[tokio::test]
async fn invalid_handler_reference_closes_without_ack_and_records_failure() {
    let (state, id, addr) = start(Some(vec![json!({"event_pattern":"forward_batch","handler":{"type":"static","actions":[{"type":"set_memory","value":"{{event.absent}}"},{"type":"accept_forward_batch"}]}})]), None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(
        &netget::server::fluent_forward::codec::encode_batch(
            &batch("forward", true),
            Some("invalid-reference"),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let entries = logs(&state, id, 2).await;
    assert!(entries
        .iter()
        .any(|e| e.event_type == "forward_handler_failed"
            && e.response[0]["decision"] == "fail_closed_handler_error"));
    state.remove_server(id).await;
}
