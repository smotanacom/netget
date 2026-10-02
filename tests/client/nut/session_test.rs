use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, AccessLogOwner, ClientId},
};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
pub async fn start(remote: String, connected: serde_json::Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let llm = netget::llm::OllamaClient::new("http://127.0.0.1:1");
    let (tx, _) = mpsc::unbounded_channel();
    let id=ClientForm { protocol:"nut".into(),remote_addr:Some(remote),instruction:Some("Read UPS data".into()),event_handlers:Some(vec![json!({"event_pattern":"nut_connected","handler":{"type":"static","actions":connected}}),json!({"event_pattern":"nut_response","handler":{"type":"static","actions":[]}})]),..Default::default() }.create(&state,llm,tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}
pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .any(|e| serde_json::to_string(e).unwrap().contains(needle))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn client_handlers_injection_and_fragmented_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        let mut s = String::new();
        r.read_line(&mut s).await.unwrap();
        assert_eq!(s, "LIST UPS\n");
        for fragment in [
            "BEGIN LIST UP",
            "S\nUPS rack1 \"fixture UPS\"\n",
            "END LIST UPS\n",
        ] {
            r.get_mut().write_all(fragment.as_bytes()).await.unwrap();
        }
        s.clear();
        r.read_line(&mut s).await.unwrap();
        assert_eq!(s, "GET VAR rack1 ups.status\n");
        r.get_mut()
            .write_all(b"VAR rack1 ups.status \"OB LB\"\n")
            .await
            .unwrap();
        s.clear();
        r.read_line(&mut s).await.unwrap();
        assert_eq!(s, "");
    });
    let (state, id) = start(
        address.to_string(),
        json!([{"type":"nut_request","operation":"list_ups"}]),
    )
    .await;
    wait_log(&state, id, "fixture UPS").await;
    let result = state
        .send_to_client(
            id,
            json!({"type":"nut_request","operation":"get_var","ups":"rack1","name":"ups.status"}),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    assert!(matches!(
        result,
        netget::state::client_handles::ClientSendOutcome::Sent { .. }
    ));
    wait_log(&state, id, "OB LB").await;
    state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(3))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), fixture)
        .await
        .unwrap()
        .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn protocol_errors_are_events_and_invalid_actions_are_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        let mut s = String::new();
        r.read_line(&mut s).await.unwrap();
        assert_eq!(s, "GET VAR missing ups.status\n");
        r.get_mut().write_all(b"ERR UNKNOWN-UPS\n").await.unwrap();
        s.clear();
        r.read_line(&mut s).await.unwrap();
    });
    let (state, id) = start(address.to_string(), json!([])).await;
    let bad=state.send_to_client(id,json!({"type":"nut_request","operation":"get_var","ups":"x\nLOGOUT","name":"ups.status"}),Duration::from_secs(3)).await.unwrap();
    assert!(matches!(
        bad,
        netget::state::client_handles::ClientSendOutcome::Rejected { .. }
    ));
    state
        .send_to_client(
            id,
            json!({"type":"nut_request","operation":"get_var","ups":"missing","name":"ups.status"}),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    wait_log(&state, id, "UNKNOWN-UPS").await;
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(3), fixture)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn injected_disconnect_interrupts_stalled_response_and_busy_is_explicit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (received_tx, received_rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(socket);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "LIST UPS\n");
        received_tx.send(()).unwrap();
        line.clear();
        assert_eq!(r.read_line(&mut line).await.unwrap(), 0);
    });
    let (state, id) = start(
        addr.to_string(),
        json!([{"type":"nut_request","operation":"list_ups"}]),
    )
    .await;
    received_rx.await.unwrap();
    let busy = state
        .send_to_client(
            id,
            json!({"type":"nut_request","operation":"list_ups"}),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        busy,
        netget::state::client_handles::ClientSendOutcome::Rejected { .. }
    ));
    let result = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(
        result,
        netget::state::client_handles::ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn final_response_event_is_delivered_when_peer_closes_immediately() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        reader
            .get_mut()
            .write_all(b"VAR rack1 ups.status \"OL final-reply\"\n")
            .await
            .unwrap();
    });
    let (state, id) = start(
        addr.to_string(),
        json!([{"type":"nut_request","operation":"get_var","ups":"rack1","name":"ups.status"}]),
    )
    .await;
    wait_log(&state, id, "final-reply").await;
    fixture.await.unwrap();
    state.remove_client(id).await;
}
