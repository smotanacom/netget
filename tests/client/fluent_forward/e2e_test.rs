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
        protocol: "fluent-forward".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(vec![
            json!({"event_pattern":"forward_connected","handler":handler}),
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
pub(super) async fn send(state: &AppState, id: ClientId, batch: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"send_forward_batch","batch":batch}),
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}

pub(super) fn batch(mode: &str, ack: bool) -> Value {
    json!({"tag":"demo.logs","entries":[{"timestamp":{"seconds":1700000000,"nanoseconds":250000000},"record":{"message":"温度","n":42}}],"mode":mode,"require_ack":ack})
}
pub(super) async fn received(
    peer: &mut tokio::net::TcpStream,
) -> netget::server::fluent_forward::codec::Batch {
    let mut d = netget::server::fluent_forward::codec::Decoder::default();
    let mut buf = [0; 8192];
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(n) = d.next_node().unwrap() {
                break netget::server::fluent_forward::codec::parse_batch(n)
                    .unwrap()
                    .unwrap();
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
async fn injection_atomic_rejection_manual_connect_and_disconnect() {
    use tokio::io::AsyncWriteExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"manual","timeout_secs":300}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    assert!(matches!(
        send(&state, id, json!({"tag":"x","entries":[]})).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), peer.read(&mut [0; 1]))
            .await
            .is_err()
    );
    assert!(matches!(
        send(&state, id, batch("compressed_packed", true)).await,
        ClientSendOutcome::Sent { .. }
    ));
    let b = received(&mut peer).await;
    assert_eq!(b.entries[0].record["message"], "温度");
    peer.write_all(
        &netget::server::fluent_forward::codec::encode_ack(b.chunk.as_ref().unwrap()).unwrap(),
    )
    .await
    .unwrap();
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(id).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(0))
            || closed.is_err_and(|e| matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof
            ))
    );
}
#[tokio::test]
async fn mismatched_ack_closes_handle_without_retry() {
    use tokio::io::AsyncWriteExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"static","actions":[]}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    send(&state, id, batch("forward", true)).await;
    received(&mut peer).await;
    peer.write_all(&netget::server::fluent_forward::codec::encode_ack("wrong").unwrap())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
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
    state.remove_client(id).await;
}

#[tokio::test]
async fn pending_ack_cap_rejects_before_wire_and_parked_event_queue_overflow_closes() {
    use netget::client::fluent_forward::MAX_PENDING_ACKS;
    use tokio::io::AsyncWriteExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"manual","timeout_secs":300}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    let mut acknowledgments = Vec::new();
    for _ in 0..MAX_PENDING_ACKS {
        assert!(matches!(
            send(&state, id, batch("forward", true)).await,
            ClientSendOutcome::Sent { .. }
        ));
        let b = received(&mut peer).await;
        acknowledgments.extend(
            netget::server::fluent_forward::codec::encode_ack(b.chunk.as_ref().unwrap()).unwrap(),
        );
    }
    assert!(matches!(
        send(&state, id, batch("forward", true)).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), peer.read(&mut [0; 1]))
            .await
            .is_err()
    );
    peer.write_all(&acknowledgments).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                send(&state, id, batch("forward", true)).await,
                ClientSendOutcome::Sent { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let b = received(&mut peer).await;
    peer.write_all(
        &netget::server::fluent_forward::codec::encode_ack(b.chunk.as_ref().unwrap()).unwrap(),
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
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    state.remove_client(id).await;
    assert!(state.list_intercepts().await.is_empty());
}

#[tokio::test]
async fn ack_deadline_closes_without_retry_and_records_standard_timeout_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"static","actions":[]}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    send(&state, id, batch("forward", true)).await;
    received(&mut peer).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::time::resume();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None
        )
        .await
        .iter()
        .any(|e| e.event_type == "forward_ack_timeout" && e.request["pending_count"] == 1));
    assert!(!state.has_client_handle(id).await);
    assert_eq!(
        state.get_client(id).await.unwrap().status,
        ClientStatus::Disconnected
    );
    state.remove_client(id).await;
}

#[tokio::test]
async fn packed_depth_rejection_is_atomic_and_oversized_handler_batch_closes() {
    use netget::server::fluent_forward::codec::MAX_DEPTH;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(
        listener.local_addr().unwrap().to_string(),
        json!({"type":"static","actions":[]}),
    )
    .await;
    let (mut peer, _) = listener.accept().await.unwrap();
    let mut nested = json!(null);
    for _ in 0..MAX_DEPTH - 1 {
        nested = json!([nested]);
    }
    for mode in ["packed", "compressed_packed"] {
        let mut b = batch(mode, true);
        b["entries"][0]["record"] = json!({"nested":nested});
        assert!(matches!(
            send(&state, id, b).await,
            ClientSendOutcome::Rejected { .. }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), peer.read(&mut [0; 1]))
                .await
                .is_err()
        );
    }
    assert!(matches!(
        send(&state, id, batch("packed", false)).await,
        ClientSendOutcome::Sent { .. }
    ));
    assert_eq!(
        received(&mut peer).await.entries[0].record["message"],
        "温度"
    );
    state.remove_client(id).await;

    let action = json!({"type":"send_forward_batch","batch":batch("forward", false)});
    let (state, id) = start(listener.local_addr().unwrap().to_string(), json!({"type":"static","actions":vec![action;netget::client::fluent_forward::MAX_HANDLER_ACTIONS + 1]})).await;
    let (mut peer, _) = listener.accept().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn repeated_malformed_and_secure_greeting_close_without_retry() {
    use netget::server::fluent_forward::codec::{encode_ack, encode_node, Node};
    use tokio::io::AsyncWriteExt;
    for reply in [
        None,
        Some(vec![0xc1]),
        Some(
            encode_node(&Node::Array(vec![
                Node::Str(b"HELO".to_vec()),
                Node::Map(vec![]),
            ]))
            .unwrap(),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, id) = start(
            listener.local_addr().unwrap().to_string(),
            json!({"type":"static","actions":[]}),
        )
        .await;
        let (mut peer, _) = listener.accept().await.unwrap();
        send(&state, id, batch("forward", true)).await;
        let b = received(&mut peer).await;
        let bytes = reply.unwrap_or_else(|| {
            let ack = encode_ack(b.chunk.as_ref().unwrap()).unwrap();
            [ack.clone(), ack].concat()
        });
        peer.write_all(&bytes).await.unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while state.has_client_handle(id).await {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            state.get_client(id).await.unwrap().status,
            ClientStatus::Disconnected
        );
        state.remove_client(id).await;
    }
}
