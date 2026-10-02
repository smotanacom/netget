use netget::cli::management::ClientForm;
use netget::state::{
    app_state::AppState, client_handles::ClientSendOutcome, ClientId, ClientStatus,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpListener, sync::mpsc};

pub(super) async fn start(remote: String, handler: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "graphite".into(),
        remote_addr: Some(remote),
        instruction: Some("test".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"graphite_connected","handler":handler}),
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
pub(super) async fn send(state: &AppState, id: ClientId, metrics: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"send_graphite_batch","metrics":metrics}),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn injected_batch_encodes_exact_wire_and_disconnect_closes_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"static","actions":[]}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    let expected = "servers.demo.load 0.5 1700000000.25\n温度;location=lab -2 -1\n";
    let outcome=send(&state,id,json!([{"path":"servers.demo.load","value":0.5,"timestamp":1700000000.25},{"path":"温度;location=lab","value":-2,"timestamp":-1}])).await;
    assert!(matches!(outcome,ClientSendOutcome::Sent{bytes_sent} if bytes_sent==expected.len()));
    let mut bytes = vec![0; expected.len()];
    tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, expected.as_bytes());
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(!state.has_client_handle(id).await);
    assert_eq!(
        state.get_client(id).await.unwrap().status,
        ClientStatus::Disconnected
    );
    state.remove_client(id).await;
}

#[tokio::test]
async fn invalid_action_is_atomic_and_does_not_poison_next_send() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"static","actions":[]}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    for metrics in [
        json!([]),
        json!([{"path":"good","value":1,"timestamp":2},{"path":"bad\nline","value":1,"timestamp":2}]),
        json!([{"path":"bad","value":"NaN","timestamp":2}]),
        json!([{"path":"bad","value":1,"timestamp":-2}]),
    ] {
        assert!(matches!(
            send(&state, id, metrics).await,
            ClientSendOutcome::Rejected { .. }
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), peer.read(&mut [0; 1]))
            .await
            .is_err()
    );
    assert!(matches!(
        send(&state, id, json!([{"path":"ok","value":1,"timestamp":2}])).await,
        ClientSendOutcome::Sent { .. }
    ));
    let mut bytes = [0; 7];
    tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, b"ok 1 2\n");
    state.remove_client(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn manual_connected_handler_allows_injection_and_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"manual","timeout_secs":300}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        send(&state, id, json!([{"path":"ok","value":1,"timestamp":2}])).await,
        ClientSendOutcome::Sent { .. }
    ));
    let mut bytes = [0; 7];
    peer.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes, b"ok 1 2\n");
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(id).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ipv6_script_handler_sends_and_remote_eof_cleans_command_handle() {
    let listener = TcpListener::bind("[::1]:0").await.expect("IPv6 loopback");
    let (state,id)=start(listener.local_addr().unwrap().to_string(),json!({"type":"script","language":"python","code":"import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'send_graphite_batch','metrics':[{'path':'ipv6','value':7,'timestamp':8}]}]}))"})).await;
    let (mut peer, _) = listener.accept().await.unwrap();
    let mut bytes = [0; 9];
    tokio::time::timeout(Duration::from_secs(10), peer.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, b"ipv6 7 8\n");
    drop(peer);
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state.get_client(id).await.unwrap().status,
        ClientStatus::Disconnected
    );
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
}
