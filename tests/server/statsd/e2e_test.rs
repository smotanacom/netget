use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc};

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
        protocol: "statsd".into(),
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
    .expect("StatsD bind");
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
    .expect("StatsD access log")
}
#[tokio::test]
async fn default_collector_batches_without_model_calls_rejects_truncation_and_stops() {
    let (state, id, addr) = start(None, None).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender
        .send_to(b"requests:1|c\nqueue:+2|g\nusers:alice|s", addr)
        .await
        .unwrap();
    let received = logs(&state, id, 1).await;
    assert_eq!(received[0].event_type, "statsd_batch");
    assert_eq!(received[0].request["record_count"], 3);
    assert_eq!(
        received[0].response,
        json!([{"type":"collect_statsd_batch"}])
            .as_array()
            .unwrap()
            .clone()
    );
    // Prefix is valid and would fit a naive 8192-byte buffer. The +1 sentinel
    // rejects the entire oversized packet, and the following packet still works.
    let oversize = format!("x:{}|s\n", "a".repeat(8192 - 4));
    sender.send_to(oversize.as_bytes(), addr).await.unwrap();
    sender.send_to(b"ok:2|c", addr).await.unwrap();
    let received = logs(&state, id, 3).await;
    assert_eq!(
        received
            .iter()
            .filter(|entry| entry.event_type == "statsd_batch")
            .count(),
        2
    );
    assert_eq!(
        received
            .iter()
            .filter(|entry| entry.event_type == "statsd_invalid_datagram")
            .count(),
        1
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), sender.recv(&mut [0; 1]))
            .await
            .is_err(),
        "one-way collector replied"
    );
    assert!(
        state.get_server(id).await.unwrap().connections.is_empty(),
        "per-packet peer rows leaked"
    );
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = UdpSocket::bind(addr).await {
                break socket;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stopping released UDP socket");
}
#[tokio::test]
async fn explicit_static_and_script_handlers_override_collection_default() {
    for handler in [
        json!({"type":"static","actions":[]}),
        json!({"type":"script","language":"python","code":"import json,sys\ni=json.load(sys.stdin)\nassert i['event']['records'][0]['name']=='handler_metric'\nprint(json.dumps({'actions':[]}))"}),
    ] {
        let (state, id, addr) = start(
            Some(vec![
                json!({"event_pattern":"statsd_batch","handler":handler}),
            ]),
            None,
        )
        .await;
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"handler_metric:4|c", addr).await.unwrap();
        let received = logs(&state, id, 1).await;
        assert_eq!(received[0].request["record_count"], 1);
        assert!(
            received[0].response.is_empty(),
            "configured handler was bypassed: {:?}",
            received[0].response
        );
        state.remove_server(id).await;
    }
}
#[tokio::test]
async fn statsd_dialect_refuses_dogstatsd_without_losing_next_packet() {
    let (state, id, addr) = start(None, Some(json!({"dialect":"statsd"}))).await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for wire in ["a:1|c|#tag", "_sc|db|0", "a:1|c"] {
        sender.send_to(wire.as_bytes(), addr).await.unwrap();
    }
    let entries = logs(&state, id, 3).await;
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.event_type == "statsd_invalid_datagram")
            .count(),
        2
    );
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.event_type == "statsd_batch")
            .count(),
        1
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn netget_emitter_and_collector_pair_reuses_the_structured_surface() {
    let (state, id, addr) = start(None, None).await;
    let (tx, _) = mpsc::unbounded_channel();
    let client=netget::cli::management::ClientForm { protocol:"dogstatsd".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"statsd_connected","handler":{"type":"static","actions":[{"type":"send_statsd_batch","records":[{"kind":"metric","name":"pair","value":"1","metric_type":"c","tags":["pair:yes"]}]}]}})]),..Default::default() }.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),tx).await.unwrap();
    let entries = logs(&state, id, 1).await;
    assert_eq!(
        entries[0].request["records"][0],
        json!({"kind":"metric","name":"pair","value":"1","metric_type":"c","tags":["pair:yes"]})
    );
    state.remove_client(client).await;
    state.remove_server(id).await;
}

#[tokio::test]
async fn opted_in_model_receives_one_event_for_the_complete_batch(
) -> crate::server::helpers::E2EResult<()> {
    use crate::server::helpers::{start_netget_server, NetGetConfig};
    let config=NetGetConfig::new("listen on port {AVAILABLE_PORT} via statsd. Summarize batches.").with_mock(|mock| {
        mock.on_instruction_containing("via statsd").respond_with_actions(json!([{"type":"open_server","base_stack":"statsd","port":0,"instruction":"Summarize batches","startup_params":{"llm_fallback":true}}])).expect_calls(1).and()
        .on_event("statsd_batch").respond_with_actions_from_event(|event| {
            assert_eq!(event["record_count"],2);
            assert_eq!(event["records"][0]["name"],"first");
            assert_eq!(event["records"][1]["name"],"second");
            json!([{"type":"collect_statsd_batch"}])
        }).expect_calls(1).and()
    });
    let server = start_netget_server(config).await?;
    let sender = UdpSocket::bind("127.0.0.1:0").await?;
    sender
        .send_to(b"first:1|c\nsecond:2|g", ("127.0.0.1", server.port))
        .await?;
    server.wait_for_mocks(20).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn stop_cancels_parked_batch_and_releases_listener() {
    let (state, id, addr) = start(
        Some(vec![
            json!({"event_pattern":"statsd_batch","handler":{"type":"manual","timeout_secs":300}}),
        ]),
        None,
    )
    .await;
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.send_to(b"parked:1|c", addr).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("batch parked");
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = UdpSocket::bind(addr).await {
                break socket;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop releases parked listener");
    assert!(
        state.list_intercepts().await.is_empty(),
        "stopped listener left a live intercept"
    );
}
