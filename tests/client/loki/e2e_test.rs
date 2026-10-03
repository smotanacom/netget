use netget::cli::management::ClientForm;
use netget::state::{
    app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId, ClientStatus,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};
pub(super) async fn start(
    remote: String,
    handlers: Option<Vec<Value>>,
    token: Option<&str>,
) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "loki".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: handlers,
        startup_params: token.map(|t| json!({"auth_token":t})),
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
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}
pub(super) fn batch(encoding: &str) -> Value {
    json!({"tenant_id":"tenant-one","encoding":encoding,"streams":[{"labels":{"app":"sample 名","host":"say \"hi\" \\ \n\t"},"entries":[{"timestamp_ns":1700000000000000123i64,"line":"entry 名 \"quote\" \n\t","structured_metadata":{"trace_id":"0123名","user_id":"two"}},{"timestamp_ns":1700000000000000124i64,"line":"second line"}]}]})
}
pub(super) async fn send(state: &AppState, id: ClientId, batch: Value) -> ClientSendOutcome {
    state
        .send_to_client(
            id,
            json!({"type":"push_loki_entries","batch":batch}),
            Duration::from_secs(15),
        )
        .await
        .unwrap()
}
pub(super) async fn response_logs(
    state: &AppState,
    id: ClientId,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut entries = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == "loki_push_response")
                .collect::<Vec<_>>();
            if entries.len() >= count {
                entries.sort_by_key(|e| e.id);
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
pub(super) async fn received(peer: &mut tokio::net::TcpStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        let mut b = [0; 8192];
        loop {
            let n = peer.read(&mut b).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&b[..n]);
            if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&bytes[..at]);
                let len = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|n| n.parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= at + 4 + len {
                    break bytes;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn parked_handler_response_queue_cap_closes_and_removal_cancels_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=start(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"loki_connected","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    for _ in 0..=netget::client::loki::MAX_QUEUED_EVENTS {
        let write = state.send_to_client(
            id,
            json!({"type":"push_loki_entries","batch":batch("json")}),
            Duration::from_secs(5),
        );
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            peer.write_all(
                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        };
        let (outcome, _) = tokio::join!(write, receive);
        assert!(matches!(
            outcome.unwrap(),
            ClientSendOutcome::Executed { .. }
        ));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    assert!(state.list_intercepts().await.is_empty());
    let (state, id) = start(listener.local_addr().unwrap().to_string(), None, None).await;
    let write = state.send_to_client(
        id,
        json!({"type":"push_loki_entries","batch":batch("json")}),
        Duration::from_secs(5),
    );
    let remove = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        received(&mut peer).await;
        state.remove_client(id).await;
        let closed = tokio::time::timeout(Duration::from_secs(5), peer.read(&mut [0; 1]))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0)) || closed.is_err());
    };
    let (outcome, _) = tokio::join!(write, remove);
    assert!(outcome.is_err());
    assert!(!state.has_client_handle(id).await);
}

#[test]
fn origin_limits_exclude_tls_paths_credentials_and_queries() {
    use netget::client::loki::transport::Origin;
    for bad in [
        "https://localhost:3100",
        "http://user:secret@localhost:3100",
        "http://localhost:3100/api",
        "http://localhost:3100?x=1",
        "http://localhost:3100/#frag",
    ] {
        assert!(Origin::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(
        Origin::parse("localhost:3100").unwrap().connect_addr,
        "localhost:3100"
    );
    assert_eq!(
        Origin::parse("http://[::1]:3100").unwrap().connect_addr,
        "[::1]:3100"
    );
}

#[tokio::test]
async fn oversized_handler_action_list_closes_before_opening_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let action = json!({"type":"push_loki_entries","batch":batch("json")});
    let id=ClientForm {protocol:"loki".into(),remote_addr:Some(listener.local_addr().unwrap().to_string()),instruction:Some(String::new()),event_handlers:Some(vec![json!({"event_pattern":"loki_connected","handler":{"type":"static","actions":vec![action;netget::client::loki::MAX_HANDLER_ACTIONS+1]}})]),..Default::default()}.create(&state,netget::llm::OllamaClient::new("http://127.0.0.1:1"),tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.get_client(id).await.unwrap().status != ClientStatus::Disconnected {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(!state.has_client_handle(id).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}

#[tokio::test]
async fn blocked_and_rate_limited_responses_are_typed_without_retry_or_closing_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(listener.local_addr().unwrap().to_string(), None, None).await;
    for status in [260, 429] {
        let action = json!({"type":"push_loki_entries","batch":batch("json")});
        let write = state.send_to_client(id, action, Duration::from_secs(5));
        let serve = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            let retry = if status == 429 {
                "Retry-After: 3\r\n"
            } else {
                ""
            };
            peer.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 7\r\n{retry}Connection: close\r\n\r\nblocked").as_bytes()).await.unwrap();
        };
        let (outcome, _) = tokio::join!(write, serve);
        assert!(matches!(
            outcome.unwrap(),
            ClientSendOutcome::Executed { .. }
        ));
    }
    let e = response_logs(&state, id, 2).await;
    assert_eq!(e[0].request["status"], 260);
    assert_eq!(e[0].request["error"]["message"], "blocked");
    assert_eq!(e[1].request["retry_after_seconds"], 3);
    assert_eq!(e[0].request["tenant_id"], "tenant-one");
    assert!(state.has_client_handle(id).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn atomic_rejection_and_injection_work_during_manual_connected_handler() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=start(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"loki_connected","handler":{"type":"manual","timeout_secs":300}})]),Some("peer-secret")).await;
    let mut invalid = batch("json");
    invalid["streams"] = json!([]);
    assert!(matches!(
        send(&state, id, invalid).await,
        ClientSendOutcome::Rejected { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    let write = state.send_to_client(
        id,
        json!({"type":"push_loki_entries","batch":batch("json")}),
        Duration::from_secs(5),
    );
    let peer = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        let body = received(&mut peer).await;
        assert!(String::from_utf8_lossy(&body).starts_with("POST /loki/api/v1/push HTTP/1.1"));
        peer.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    };
    let (outcome, _) = tokio::join!(write, peer);
    assert!(matches!(
        outcome.unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
    assert_eq!(state.get_client_llm_calls(id).await, 0);
    assert!(matches!(
        state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    state.remove_client(id).await;
    assert!(state.list_intercepts().await.is_empty());
}
#[tokio::test]
async fn disconnect_cancels_inflight_http_and_rejects_second_write() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(listener.local_addr().unwrap().to_string(), None, None).await;
    let writing = state.send_to_client(
        id,
        json!({"type":"push_loki_entries","batch":batch("json")}),
        Duration::from_secs(15),
    );
    let driving = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        received(&mut peer).await;
        assert!(matches!(
            send(&state, id, batch("json")).await,
            ClientSendOutcome::Rejected { .. }
        ));
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
    };
    let (result, _) = tokio::join!(writing, driving);
    assert!(result.is_err());
    assert!(!state.has_client_handle(id).await);
    assert_eq!(
        state.get_client(id).await.unwrap().status,
        ClientStatus::Disconnected
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn errors_are_typed_and_malformed_or_oversized_responses_close_without_retry() {
    for (case,wire) in [b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 1\r\n\r\n\xff".to_vec(),b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",netget::client::loki::transport::MAX_RESPONSE_BYTES+1,"x".repeat(netget::client::loki::transport::MAX_RESPONSE_BYTES+1)).into_bytes(),format!("HTTP/1.1 400 Bad Request\r\nContent-Length: 4097\r\n\r\n{}","x".repeat(4097)).into_bytes(),format!("HTTP/1.1 204 No Content\r\nX-Large: {}\r\n\r\n","x".repeat(32*1024)).into_bytes(),format!("HTTP/1.1 204 No Content\r\n{}\r\n",(0..65).map(|i|format!("X-{i}: y\r\n")).collect::<String>()).into_bytes(),b"HTTP/1.1 204 No Content\r\nContent-Length: 1\r\n\r\nx".to_vec(),b"HTTP/1.1 429 Error\r\nRetry-After: tomorrow\r\nContent-Length: 1\r\n\r\nx".to_vec(),b"HTTP/1.1 400 Error\r\nRetry-After: 3\r\nContent-Length: 1\r\n\r\nx".to_vec()].into_iter().enumerate() {
  let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();let(state,id)=start(listener.local_addr().unwrap().to_string(),None,None).await;let write=state.send_to_client(id,json!({"type":"push_loki_entries","batch":batch("json")}),Duration::from_secs(15));let peer=async {let(mut peer,_)=listener.accept().await.unwrap();received(&mut peer).await;let _=peer.write_all(&wire).await;};let(result,_)=tokio::join!(write,peer);assert!(result.is_err(),"response case {case} accepted: {result:?}");tokio::time::timeout(Duration::from_secs(5),async{while state.has_client_handle(id).await {tokio::time::sleep(Duration::from_millis(5)).await;}}).await.unwrap();assert!(tokio::time::timeout(Duration::from_millis(50),listener.accept()).await.is_err());state.remove_client(id).await;
 }
}
#[tokio::test]
async fn whole_exchange_deadline_cancels_socket_and_handle() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(listener.local_addr().unwrap().to_string(), None, None).await;
    let write = state.send_to_client(
        id,
        json!({"type":"push_loki_entries","batch":batch("json")}),
        Duration::from_secs(15),
    );
    let driving = async {
        let (mut peer, _) = listener.accept().await.unwrap();
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
    };
    let (result, _) = tokio::join!(write, driving);
    assert!(result.is_err());
    state.remove_client(id).await;
}
