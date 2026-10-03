use netget::cli::management::ClientForm;
use netget::state::{
    app_state::AppState, client_handles::ClientSendOutcome, ClientId, ClientStatus,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{net::UdpSocket, sync::mpsc};

pub(super) async fn start(remote: String, handler: Value, dialect: &str) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "statsd".into(),
        remote_addr: Some(remote),
        instruction: Some("test".into()),
        startup_params: Some(json!({"dialect":dialect})),
        event_handlers: Some(vec![
            json!({"event_pattern":"statsd_connected","handler":handler}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("client handle");
    (state, id)
}
pub(super) async fn send(state: &AppState, id: ClientId, records: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"send_statsd_batch","records":records}),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}
#[tokio::test]
async fn injected_typed_batch_has_exact_wire_bytes_and_disconnect_releases_socket() {
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = receiver.local_addr().unwrap();
    let records = json!([
        {"kind":"metric","name":"requests","value":"2","metric_type":"c","sample_rate":0.5,"tags":["env:test"]},
        {"kind":"metric","name":"level","value":"+3","metric_type":"g"},
        {"kind":"event","title":"雪","text":"hello\nworld","alert_type":"warning"},
        {"kind":"service_check","name":"db","status":2,"message":"timeout|retry"}
    ]);
    let (state, id) = start(
        addr.to_string(),
        json!({"type":"static","actions":[]}),
        "dogstatsd",
    )
    .await;
    let expected="requests:2|c|@0.5|#env:test\nlevel:+3|g\n_e{3,12}:雪|hello\\nworld|t:warning\n_sc|db|2|m:timeout|retry";
    let outcome = send(&state, id, records).await;
    assert!(matches!(outcome,ClientSendOutcome::Sent{bytes_sent} if bytes_sent==expected.len()));
    let mut buf = [0; 8193];
    let (n, local) = tokio::time::timeout(Duration::from_secs(5), receiver.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], expected.as_bytes());
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    let outcome = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(outcome, ClientSendOutcome::Disconnected));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !state.has_client_handle(id).await {
                if let Ok(socket) = UdpSocket::bind(local).await {
                    break socket;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnect socket released");
    assert_eq!(
        state.get_client(id).await.unwrap().status,
        ClientStatus::Disconnected
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn static_connect_handler_emits_and_bad_injections_are_rejected_without_send() {
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state,id)=start(receiver.local_addr().unwrap().to_string(),json!({"type":"static","actions":[{"type":"send_statsd_batch","records":[{"kind":"metric","name":"initial","value":"1","metric_type":"c"}]}]}),"statsd").await;
    let mut buf = [0; 128];
    let (n, local) = tokio::time::timeout(Duration::from_secs(5), receiver.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"initial:1|c");
    for records in [
        json!([]),
        json!([{"kind":"metric","name":"bad","value":"NaN","metric_type":"c"}]),
        json!([{"kind":"metric","name":"dog","value":"1","metric_type":"c","tags":["x"]}]),
    ] {
        assert!(matches!(
            send(&state, id, records).await,
            ClientSendOutcome::Rejected { .. }
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), receiver.recv(&mut buf))
            .await
            .is_err()
    );
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = UdpSocket::bind(local).await {
                break socket;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stop socket released");
}
#[tokio::test]
async fn manual_connected_handler_does_not_block_injected_send_or_disconnect() {
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        receiver.local_addr().unwrap().to_string(),
        json!({"type":"manual","timeout_secs":300}),
        "statsd",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("connected event parked before injection");
    assert!(matches!(
        send(
            &state,
            id,
            json!([{"kind":"metric","name":"manual","value":"1","metric_type":"c"}])
        )
        .await,
        ClientSendOutcome::Sent { .. }
    ));
    let mut buf = [0; 128];
    tokio::time::timeout(Duration::from_secs(5), receiver.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(id).await;
}
#[tokio::test]
async fn ipv6_emission_and_script_handler_work() {
    let receiver = UdpSocket::bind("[::1]:0")
        .await
        .expect("IPv6 loopback required");
    let (state,id)=start(receiver.local_addr().unwrap().to_string(),json!({"type":"script","language":"python","code":"import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'send_statsd_batch','records':[{'kind':'metric','name':'ipv6','value':'7','metric_type':'g'}]}]}))"}),"statsd").await;
    let mut buf = [0; 128];
    let n = tokio::time::timeout(Duration::from_secs(10), receiver.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"ipv6:7|g");
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
}
