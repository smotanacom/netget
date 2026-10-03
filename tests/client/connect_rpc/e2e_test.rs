use crate::helpers::connect_rpc_peer as peer;
use netget::state::{client_handles::ClientSendOutcome, AppState, ClientId, ClientStatus};
use serde_json::{json, Value};
use std::time::Duration;
async fn start(state: &AppState, port: u16, extra: Value) -> ClientId {
    peer::client_id(state, port, peer::wait_handlers(), extra)
        .await
        .unwrap()
}
async fn queued(state: &AppState, id: ClientId, action: Value) {
    assert!(matches!(
        peer::send(state, id, action).await.unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
}
async fn disconnected(state: &AppState, id: ClientId) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .get_client(id)
                .await
                .is_some_and(|client| client.status == ClientStatus::Disconnected)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!state.has_client_handle(id).await);
}
#[tokio::test]
async fn independent_connect_es_unary_server_streaming_gzip_and_colon_status() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = start(&state, peer.port, json!({})).await;
    for (call, method, name) in [
        (1, "Echo", "peer"),
        (2, "Watch", "watch"),
        (3, "Echo", "status"),
    ] {
        let mut action = peer::call(call, method, name);
        action["gzip"] = json!(true);
        queued(&state, id, action).await;
        let end = peer::log(&state, id, "connect_rpc_ended", call).await;
        assert_eq!(end["code"], if call == 3 { 7 } else { 0 });
        if call == 1 {
            let message = peer::log(&state, id, "connect_rpc_message", call).await;
            assert_eq!(message["response"]["value"], 5);
            assert_eq!(message["response"]["counts"], json!({"first":2,"second":3}));
        }
        if call == 2 {
            assert_eq!(end["response_count"], 3);
            assert_eq!(
                end["metadata"]["x-peer-note"],
                json!(["contains:colon:values"])
            );
            assert_eq!(
                peer::log(&state, id, "connect_rpc_opened", call).await["metadata"]
                    ["x-peer-leading"],
                json!(["header:values"])
            );
        }
        if call == 3 {
            assert_eq!(end["message"], "denied: peer test");
        }
    }
    state.remove_client(id).await;
}
#[tokio::test]
async fn paired_netget_binary_binding_and_ordered_message_before_disconnect() {
    let state = peer::state().await;
    let (server, port) = peer::server(&state, vec![peer::handler()], json!({}))
        .await
        .unwrap();
    let mut handlers = peer::wait_handlers();
    handlers.insert(0,json!({"event_pattern":"connect_rpc_ended","handler":{"type":"static","actions":[{"type":"disconnect"}]}}));
    let id = peer::client_id(&state, port, handlers, json!({}))
        .await
        .unwrap();
    queued(&state, id, peer::call(1, "Watch", "pair")).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 1).await["response_count"],
        3
    );
    disconnected(&state, id).await;
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    assert_eq!(
        logs.iter()
            .filter(|log| log.event_type == "connect_rpc_message")
            .count(),
        3
    );
    state.remove_client(id).await;
    state.remove_server(server).await.unwrap();
}
#[tokio::test]
async fn admission_typed_validation_and_identifier_reuse_refused_then_valid() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = start(&state, peer.port, json!({})).await;
    for mut action in [
        peer::call(0, "Echo", "peer"),
        peer::call(1, "Collect", "peer"),
        peer::call(1, "Chat", "peer"),
        peer::call(1, "Absent", "peer"),
    ] {
        assert!(peer::send(&state, id, action.take()).await.is_err());
    }
    let mut invalid = peer::call(1, "Echo", "peer");
    invalid["request"]["value"] = json!("wrong type");
    assert!(peer::send(&state, id, invalid).await.is_err());
    for key in [
        "grpc-timeout",
        "connect-timeout-ms",
        "trailer-x-note",
        "authorization-bin",
        "content-type",
        "origin",
    ] {
        let mut action = peer::call(1, "Echo", "peer");
        action["metadata"] = json!({key:"bad"});
        assert!(peer::send(&state, id, action).await.is_err());
    }
    queued(&state, id, peer::call(1, "Echo", "peer")).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 1).await["code"],
        0
    );
    assert!(peer::send(&state, id, peer::call(1, "Echo", "peer"))
        .await
        .is_err());
    state.remove_client(id).await;
}
#[tokio::test]
async fn exact_and_plus_one_decoded_responses_gzip_count_and_local_request_bounds() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    for (call, name, code, count) in [
        (1, "bound", 0, 1),
        (2, "overflow", 8, 0),
        (3, "many", 8, 256),
    ] {
        let id = start(&state, peer.port, json!({})).await;
        queued(&state, id, peer::call(call, "Watch", name)).await;
        let end = peer::log(&state, id, "connect_rpc_ended", call).await;
        assert_eq!(end["code"], code, "{end}");
        assert_eq!(end["response_count"], count);
        if code != 0 {
            disconnected(&state, id).await;
        }
        state.remove_client(id).await;
    }
    let id = start(&state, peer.port, json!({})).await;
    let mut action = peer::call(4, "Echo", "peer");
    action["request"] = json!({"name":"x".repeat(4*1024*1024-4)});
    assert!(peer::send(&state, id, action).await.is_err());
    queued(&state, id, peer::call(4, "Echo", "peer")).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 4).await["code"],
        0
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn cancellation_is_responsive_with_parked_handlers_and_disconnects_http1() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id=peer::client_id(&state,peer.port,vec![json!({"event_pattern":"connect_rpc_connected","handler":{"type":"static","actions":[{"type":"wait_for_more"}]} }),json!({"event_pattern":"*","handler":{"type":"manual"}})],json!({})).await.unwrap();
    queued(&state, id, peer::call(1, "Watch", "subscribe")).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.list_intercepts().await.len() < 16 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(state.list_intercepts().await.len(), 16);
    assert!(peer::send(&state, id, peer::call(2, "Echo", "peer"))
        .await
        .is_err());
    assert!(
        peer::send(&state, id, json!({"type":"connect_rpc_cancel","call_id":2}))
            .await
            .is_err()
    );
    let start = std::time::Instant::now();
    assert!(matches!(
        peer::send(&state, id, json!({"type":"connect_rpc_cancel","call_id":1}))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    disconnected(&state, id).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 1).await["code"],
        1
    );
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
}
#[tokio::test]
async fn rpc_deadline_and_idle_cleanup_owned_session() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = start(&state, peer.port, json!({"rpc_timeout_secs":1})).await;
    queued(&state, id, peer::call(1, "Watch", "subscribe")).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 1).await["code"],
        4
    );
    // An incomplete response cannot be reused after dropping its HTTP/1.1 body.
    state.remove_client(id).await;
    let id = start(&state, peer.port, json!({"idle_timeout_secs":1})).await;
    disconnected(&state, id).await;
    state.remove_client(id).await;
}

#[tokio::test]
async fn malformed_http_responses_record_failure_before_session_cleanup() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    for (body, headers, code) in [
        (
            [vec![0, 0, 0, 0, 0], vec![2, 0, 0, 0, 2], b"{}".to_vec()].concat(),
            "",
            0,
        ),
        (vec![0, 0, 0, 0, 0], "", 13),
        (vec![2, 0, 0, 0, 3, b'{'], "", 13),
        (vec![128, 0, 0, 0, 0], "", 13),
        (vec![0, 0, 64, 0, 1], "", 8),
        (
            [vec![2, 0, 0, 0, 14], b"{\"error\":null}".to_vec()].concat(),
            "",
            13,
        ),
        (
            vec![2, 0, 0, 0, 2, b'{', b'}'],
            "connect-content-encoding: gzip\r\nconnect-content-encoding: gzip\r\n",
            3,
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut input = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                input.extend_from_slice(&chunk[..count]);
                assert!(input.len() <= 32768);
                if input.ends_with(b"\r\n0\r\n\r\n") {
                    break;
                }
                if let Some(offset) = input.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let text = String::from_utf8_lossy(&input[..offset]).to_ascii_lowercase();
                    if let Some(length) = text
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .map(|length| length.parse::<usize>().unwrap())
                    {
                        if input.len() >= offset + 4 + length {
                            break;
                        }
                    }
                }
            }
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/connect+proto\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",body.len()).as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let id = start(&state, port, json!({"rpc_timeout_secs":2})).await;
        queued(&state, id, peer::call(1, "Watch", "fixture")).await;
        assert_eq!(
            peer::log(&state, id, "connect_rpc_ended", 1).await["code"],
            code
        );
        if code == 0 {
            assert!(peer::log(&state, id, "connect_rpc_message", 1).await["response"].is_object());
        }
        disconnected(&state, id).await;
        state.remove_client(id).await;
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn automatic_rpc_followups_stop_at_depth_four() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let mut handlers = peer::wait_handlers();
    handlers.insert(0,json!({"event_pattern":"connect_rpc_ended","handler":{"type":"script","language":"python","code":
        "import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'connect_rpc_call','call_id':e['call_id']+1,'service':'streams.Session','method':'Echo','request':{'name':'next'}}]}))"}}));
    let id = peer::client_id(&state, peer.port, handlers, json!({}))
        .await
        .unwrap();
    queued(&state, id, peer::call(1, "Echo", "next")).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 5).await["code"],
        0
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    assert_eq!(
        logs.iter()
            .filter(|log| log.event_type == "connect_rpc_ended")
            .count(),
        5
    );
    state.remove_client(id).await;
}

#[tokio::test]
async fn metadata_boundaries_and_256_identifier_budget() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = start(&state, peer.port, json!({})).await;
    for metadata in [
        json!({"x":"a".repeat(1025)}),
        json!({"x".repeat(129):"a"}),
        Value::Object(
            (0..17)
                .map(|index| (format!("x{index}"), json!("")))
                .collect(),
        ),
        Value::Object(
            (0..8)
                .map(|index| ((b'a' + index as u8).to_string(), json!("a".repeat(1024))))
                .collect(),
        ),
    ] {
        let mut action = peer::call(1, "Echo", "peer");
        action["metadata"] = metadata;
        assert!(peer::send(&state, id, action).await.is_err());
    }
    let mut action = peer::call(1, "Echo", "peer");
    action["metadata"] = Value::Object(
        (0..8)
            .map(|index| {
                (
                    (char::from(b'a' + index)).to_string(),
                    json!("a".repeat(1023)),
                )
            })
            .collect(),
    );
    queued(&state, id, action).await;
    assert_eq!(
        peer::log(&state, id, "connect_rpc_ended", 1).await["code"],
        0
    );
    for call in 2..=256 {
        queued(&state, id, peer::call(call, "Echo", "peer")).await;
        assert_eq!(
            peer::log(&state, id, "connect_rpc_ended", call).await["code"],
            0
        );
    }
    assert!(peer::send(&state, id, peer::call(257, "Echo", "peer"))
        .await
        .is_err());
    state.remove_client(id).await;
}
