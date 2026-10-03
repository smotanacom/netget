use netget::cli::management::ClientForm;
use netget::server::sflow::codec;
use netget::state::{
    app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId, ClientStatus,
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{net::UdpSocket, sync::mpsc};

pub(super) async fn start(remote: String, handlers: Option<Vec<Value>>) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "sflow".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await
            && state.get_client(id).await.unwrap().status != ClientStatus::Disconnected
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}
pub(super) fn batch() -> Value {
    serde_json::to_value(crate::helpers::sflow::batch()).unwrap()
}
pub(super) async fn send(state: &AppState, id: ClientId, batch: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"export_sflow_samples","batch":batch}),
            Duration::from_secs(15),
        )
        .await
        .unwrap()
}
pub(super) async fn receive(peer: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut b = vec![0; 8193];
    let (n, addr) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut b))
        .await
        .unwrap()
        .unwrap();
    b.truncate(n);
    (b, addr)
}
async fn disconnected(state: &AppState, id: ClientId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.get_client(id).await.unwrap().status != ClientStatus::Disconnected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn atomic_batch_validation_and_injection_work_while_connected_handler_is_parked() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"sflow_connected","handler":{"type":"manual","timeout_secs":300}})])).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut bad = batch();
    bad["samples"][3]["records"][0]["octets"] = json!(-1);
    assert!(matches!(
        send(&state, id, bad).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    assert!(matches!(
        send(&state, id, batch()).await,
        ClientSendOutcome::Executed { .. }
    ));
    let (wire, _) = receive(&peer).await;
    let mut expected = crate::helpers::sflow::golden();
    expected[16..20].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(wire, expected);
    assert!(!state.list_intercepts().await.is_empty());
    let outcome = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(outcome, ClientSendOutcome::Disconnected));
    disconnected(&state, id).await;
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
}
#[tokio::test]
async fn per_agent_datagram_sequences_count_batches_and_export_events_are_transport_only() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(peer.local_addr().unwrap().to_string(), None).await;
    for (sub, seq) in [(77, 0), (78, 0), (77, 1), (78, 1)] {
        let mut b = batch();
        b["sub_agent_id"] = json!(sub);
        assert!(matches!(
            send(&state, id, b).await,
            ClientSendOutcome::Executed { .. }
        ));
        let (wire, _) = receive(&peer).await;
        let decoded = codec::decode(&wire).unwrap();
        assert_eq!(decoded.sequence_number, seq);
        assert_eq!(decoded.record_count, 4);
        assert_eq!(decoded.sub_agent_id, sub);
    }
    let entries = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let logs = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == "sflow_exported")
                .collect::<Vec<_>>();
            if logs.len() == 4 {
                break logs;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(entries
        .iter()
        .all(|e| e.request["local_transport_only"] == true && e.request["sample_count"] == 4));
    state.remove_client(id).await;
}
#[tokio::test]
async fn exporter_state_cap_is_transactional_and_disconnect_resets_logical_sequence() {
    use netget::client::sflow::transport::{Sequences, MAX_AGENTS};
    let mut sequences = Sequences::default();
    let mut b = crate::helpers::sflow::batch();
    let first = sequences.prepare(&b, 0).unwrap();
    assert_eq!(first.sequence, 0);
    drop(first);
    assert_eq!(sequences.prepare(&b, 0).unwrap().sequence, 0);
    for sub in 0..MAX_AGENTS as u32 {
        b.sub_agent_id = sub;
        let prepared = sequences.prepare(&b, 0).unwrap();
        assert_eq!(prepared.sequence, 0);
        sequences.commit(prepared);
    }
    b.sub_agent_id = 100;
    assert!(sequences.prepare(&b, 0).is_err());
    b.sub_agent_id = 0;
    assert_eq!(sequences.prepare(&b, 0).unwrap().sequence, 1);
    let reset = Sequences::default();
    assert_eq!(reset.prepare(&b, 0).unwrap().sequence, 0);
}
#[tokio::test]
async fn response_event_and_action_capacity_cancel_owned_handler() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"sflow_connected","handler":{"type":"manual","timeout_secs":300}})])).await;
    for _ in 0..33 {
        assert!(matches!(
            send(&state, id, batch()).await,
            ClientSendOutcome::Executed { .. }
        ));
        receive(&peer).await;
    }
    disconnected(&state, id).await;
    assert!(!state.has_client_handle(id).await);
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
    let actions = vec![json!({"type":"export_sflow_samples","batch":batch()}); 33];
    let (state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"sflow_connected","handler":{"type":"static","actions":actions}})])).await;
    disconnected(&state, id).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn followup_depth_stops_automatic_export_storm_and_removal_releases_intercept_and_socket() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let action = json!({"type":"export_sflow_samples","batch":batch()});
    let (state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![
        json!({"event_pattern":"sflow_connected","handler":{"type":"static","actions":[action.clone()]}}),
        json!({"event_pattern":"sflow_exported","handler":{"type":"static","actions":[action]}}),
    ])).await;
    for _ in 0..netget::client::sflow::MAX_FOLLOWUP_DEPTH {
        receive(&peer).await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.recv(&mut [0; 8193]))
            .await
            .is_err()
    );
    state.remove_client(id).await;
    let (state,id)=start(peer.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"sflow_connected","handler":{"type":"manual","timeout_secs":300}})])).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    send(&state, id, batch()).await;
    let (_, source) = receive(&peer).await;
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !state.has_client_handle(id).await
                && state.list_intercepts().await.is_empty()
                && UdpSocket::bind(source).await.is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn native_exporter_collector_share_typed_events_and_common_script_memory() {
    let (state, server, addr, _) = crate::helpers::sflow::start(None, None).await;
    let (tx, _) = mpsc::unbounded_channel();
    let code="import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'set_memory','value':'client telemetry observed'}]}))";
    let client=ClientForm{protocol:"sflow".into(),remote_addr:Some(addr.to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"sflow_exported","handler":{"type":"script","language":"python","code":code}})]),..Default::default()}.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(client).await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    send(&state, client, batch()).await;
    assert_eq!(
        crate::helpers::sflow::logs(&state, server, "sflow_message", 1).await[0].request["message"]
            ["record_count"],
        4
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.get_memory_for_client(client).await.as_deref()
            != Some("client telemetry observed")
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(client).await;
    state.remove_server(server).await;
}
#[tokio::test]
async fn unsolicited_udp_reply_closes_exporter_without_claiming_ack() {
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(peer.local_addr().unwrap().to_string(), None).await;
    send(&state, id, batch()).await;
    let (_, source) = receive(&peer).await;
    peer.send_to(b"not-an-ack", source).await.unwrap();
    disconnected(&state, id).await;
    assert!(!state.has_client_handle(id).await);
    state.remove_client(id).await;
}
