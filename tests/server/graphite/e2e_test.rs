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
        protocol: "graphite".into(),
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
    .expect("Graphite bind");
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
    .expect("Graphite access log")
}

#[tokio::test]
async fn fragmented_coalesced_lines_collect_without_model_and_stop_closes_live_stream() {
    let (state, id, addr) = start(None, None).await;
    let mut sender = TcpStream::connect(addr).await.unwrap();
    sender.write_all("温".as_bytes()).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(75), sender.read(&mut [0; 1]))
            .await
            .is_err()
    );
    assert!(state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .is_empty());
    sender
        .write_all("度 1.5 1700000000.25\nother -2 -1\n".as_bytes())
        .await
        .unwrap();
    let entries = logs(&state, id, 1).await;
    let metrics: Vec<_> = entries
        .iter()
        .flat_map(|e| e.request["metrics"].as_array().unwrap().clone())
        .collect();
    assert_eq!(metrics.len(), 2);
    assert_eq!(
        metrics[0],
        json!({"path":"温度","value":1.5,"timestamp":1700000000.25})
    );
    assert!(metrics[1]["timestamp"].as_f64().unwrap() > 1700000000.0);
    assert!(entries
        .iter()
        .all(|e| e.response == vec![json!({"type":"collect_graphite_batch"})]));
    state.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), sender.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    tokio::net::TcpListener::bind(addr)
        .await
        .expect("listener released");
}

#[tokio::test]
async fn malformed_oversize_and_eof_partial_close_only_offending_peer() {
    let (state, id, addr) = start(None, None).await;
    for wire in [
        b"a NaN 1\n".to_vec(),
        vec![b'a'; 4097],
        b"unterminated 1 2".to_vec(),
        vec![0xff, b' ', b'1', b' ', b'2', b'\n'],
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
    let entries = logs(&state, id, 4).await;
    assert!(entries
        .iter()
        .all(|e| e.event_type == "graphite_invalid_stream"));
    let mut next = TcpStream::connect(addr).await.unwrap();
    next.write_all(b"healthy 3 4\n").await.unwrap();
    assert!(logs(&state, id, 5)
        .await
        .iter()
        .any(|e| e.event_type == "graphite_batch"));
    state.remove_server(id).await;
}

#[tokio::test]
async fn explicit_static_and_script_handlers_run_even_with_fallback_disabled() {
    for handler in [
        json!({"type":"static","actions":[]}),
        json!({"type":"script","language":"python","code":"import json,sys\nx=json.load(sys.stdin)\nassert x['event']['metrics'][0]['path']=='handler_metric'\nprint(json.dumps({'actions':[]}))"}),
    ] {
        let (state, id, addr) = start(
            Some(vec![
                json!({"event_pattern":"graphite_batch","handler":handler}),
            ]),
            None,
        )
        .await;
        let mut peer = TcpStream::connect(addr).await.unwrap();
        peer.write_all(b"handler_metric 4 5\n").await.unwrap();
        assert!(logs(&state, id, 1).await[0].response.is_empty());
        state.remove_server(id).await;
    }
}

#[tokio::test]
async fn stop_cancels_parked_manual_handler_and_all_sockets() {
    let (state,id,addr)=start(Some(vec![json!({"event_pattern":"graphite_batch","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"parked 1 2\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(state.list_intercepts().await.is_empty());
    tokio::net::TcpListener::bind(addr)
        .await
        .expect("listener released");
}

#[tokio::test]
async fn trickled_partial_line_does_not_reset_absolute_read_deadline() {
    let (state, id, addr) = start(None, None).await;
    let mut peer = TcpStream::connect(addr).await.unwrap();
    peer.write_all(b"a").await.unwrap();
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
            peer.write_all(b"b").await.unwrap();
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
async fn static_client_handler_pairs_with_collector() {
    let (state, id, addr) = start(None, None).await;
    let (tx, _) = mpsc::unbounded_channel();
    let client=netget::cli::management::ClientForm { protocol:"graphite".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"graphite_connected","handler":{"type":"static","actions":[{"type":"send_graphite_batch","metrics":[{"path":"pair","value":42,"timestamp":123}]}]}})]),..Default::default() }.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),tx).await.unwrap();
    assert_eq!(
        logs(&state, id, 1).await[0].request["metrics"][0],
        json!({"path":"pair","value":42.0,"timestamp":123.0})
    );
    state.remove_client(client).await;
    state.remove_server(id).await;
}

#[tokio::test]
async fn explicitly_opted_in_batch_uses_the_standard_model_dispatcher(
) -> crate::server::helpers::E2EResult<()> {
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    let config=NetGetConfig::new("listen on port {AVAILABLE_PORT} via graphite. Summarize batches.").with_mock(|mock| {
        mock.on_instruction_containing("via graphite").respond_with_actions(json!([{"type":"open_server","base_stack":"graphite","port":0,"instruction":"Summarize batches","startup_params":{"llm_fallback":true}}])).expect_calls(1).and()
        .on_event("graphite_batch").respond_with_actions_from_event(|event| {
            assert_eq!(event["metrics"][0]["path"],"model");
            json!([{"type":"collect_graphite_batch"}])
        }).expect_calls(1).and()
    });
    let server = start_netget_server(config).await?;
    let mut peer = TcpStream::connect(("127.0.0.1", server.port)).await?;
    peer.write_all(b"model 1 2\n").await?;
    server.wait_for_mocks(20).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
