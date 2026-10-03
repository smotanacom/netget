use crate::helpers::prometheus_remote_write::{
    batch, client, disconnected, logs, received, reply, server,
};
use netget::{
    server::prometheus_remote_write::codec::{self, WriteBatch},
    state::{client_handles::ClientSendOutcome, AccessLogOwner},
};
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;
#[tokio::test]
async fn native_pair_observes_typed_data_and_common_client_memory() {
    let (sstate, sid, addr, _) = server(None, None).await;
    let handlers = Some(vec![
        json!({"event_pattern":"remote_write_response","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'set_memory','value':'accepted'}]}))"}}),
    ]);
    let (state, id) = client(addr.to_string(), handlers, None).await;
    let out = state
        .send_to_client(
            id,
            json!({"type":"write_remote_samples","batch":batch()}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(out, ClientSendOutcome::Executed { .. }));
    let rows = logs(
        &state,
        AccessLogOwner::Client(id.as_u32()),
        "remote_write_response",
        1,
    )
    .await;
    assert_eq!(rows[0].request["accepted"], true);
    assert_eq!(rows[0].request["durable_storage_confirmed"], false);
    let rows = logs(
        &sstate,
        AccessLogOwner::Server(sid.as_u32()),
        "remote_write_request",
        1,
    )
    .await;
    assert_eq!(rows[0].request["series"], batch()["series"]);
    assert_eq!(state.get_memory_for_client(id).await.unwrap(), "accepted");
    state.remove_client(id).await;
    sstate.remove_server(sid).await;
}
#[tokio::test]
async fn retries_identical_body_with_backoff_on_5xx_then_ignores_binary_2xx_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
    let send = state.send_to_client(
        id,
        json!({"type":"write_remote_samples","batch":batch()}),
        Duration::from_secs(30),
    );
    let receive = async {
        let mut previous = None;
        let mut expected = None;
        for (index, status) in [500, 503, 500, 503, 500, 503, 500, 503, 202]
            .into_iter()
            .enumerate()
        {
            let (mut peer, _) = listener.accept().await.unwrap();
            let now = std::time::Instant::now();
            if let Some(at) = previous {
                let expected = Duration::from_millis((100u64 << (index - 1)).min(5000));
                assert!(
                    now.duration_since(at) >= expected.saturating_sub(Duration::from_millis(10))
                );
                assert!(now.duration_since(at) <= expected + Duration::from_secs(2));
            }
            previous = Some(now);
            let wire = received(&mut peer).await;
            if let Some(expected) = &expected {
                assert_eq!(&wire, expected);
            } else {
                expected = Some(wire.clone());
            }
            let at = wire.windows(4).position(|b| b == b"\r\n\r\n").unwrap() + 4;
            let headers = String::from_utf8_lossy(&wire[..at]).to_ascii_lowercase();
            assert!(headers.contains("content-encoding: snappy"));
            assert!(headers.contains("x-prometheus-remote-write-version: 0.1.0"));
            assert!(headers.contains("user-agent: netget/"));
            assert_eq!(
                codec::decode_batch(&wire[at..]).unwrap().series,
                serde_json::from_value::<WriteBatch>(batch())
                    .unwrap()
                    .series
            );
            reply(&mut peer, status, &[0xff, 0, 0xfe]).await;
        }
    };
    let (out, _) = tokio::join!(send, receive);
    assert!(matches!(out.unwrap(), ClientSendOutcome::Executed { .. }));
    let rows = logs(
        &state,
        AccessLogOwner::Client(id.as_u32()),
        "remote_write_response",
        1,
    )
    .await;
    assert_eq!(rows[0].request["status"], 202);
    assert_eq!(rows[0].request["attempts"], 9);
    state.remove_client(id).await;
}
#[tokio::test]
async fn terminal_4xx_redirects_and_default429_are_not_retried() {
    for status in [400, 401, 404, 429, 307] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (state, id) = client(listener.local_addr().unwrap().to_string(), None, None).await;
        let send = state.send_to_client(
            id,
            json!({"type":"write_remote_samples","batch":batch()}),
            Duration::from_secs(10),
        );
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            reply(&mut peer, status, b"reserved response").await;
        };
        let (out, _) = tokio::join!(send, receive);
        assert!(matches!(out.unwrap(), ClientSendOutcome::Executed { .. }));
        let rows = logs(
            &state,
            AccessLogOwner::Client(id.as_u32()),
            "remote_write_response",
            1,
        )
        .await;
        assert_eq!(rows[0].request["status"], status);
        assert_eq!(rows[0].request["accepted"], false);
        assert_eq!(rows[0].request["attempts"], 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err()
        );
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn opted_in429_retries_and_disconnect_cancels_backoff_independent_of_parked_handler() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"remote_write_connected","handler":{"type":"manual","timeout_secs":300}})]),Some(json!({"retry_429":true}))).await;
    let pending = state.send_to_client(
        id,
        json!({"type":"write_remote_samples","batch":batch()}),
        Duration::from_secs(10),
    );
    let receive = async {
        for status in [429, 503] {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            reply(&mut peer, status, b"retry").await;
        }
        let busy = state
            .send_to_client(
                id,
                json!({"type":"write_remote_samples","batch":batch()}),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(matches!(busy, ClientSendOutcome::Rejected { .. }));
        let out = state
            .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
            .await
            .unwrap();
        assert!(matches!(out, ClientSendOutcome::Disconnected));
    };
    let (result, _) = tokio::join!(pending, receive);
    assert!(result.is_err());
    disconnected(&state, id).await;
    assert!(!state.has_client_handle(id).await);
    assert!(state.list_intercepts().await.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(250), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn atomic_validation_sends_no_bytes_and_response_queue_capacity_cancels_handler() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"remote_write_connected","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    let out=state.send_to_client(id,json!({"type":"write_remote_samples","batch":{"series":[{"labels":{"bad-name":"x"},"samples":[{"timestamp_ms":1,"value":1}]}]}}),Duration::from_secs(1)).await.unwrap();
    assert!(matches!(out, ClientSendOutcome::Rejected { .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    for _ in 0..=netget::client::prometheus_remote_write::MAX_QUEUED_EVENTS {
        let send = state.send_to_client(
            id,
            json!({"type":"write_remote_samples","batch":batch()}),
            Duration::from_secs(5),
        );
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            reply(&mut peer, 204, b"").await;
        };
        let (out, _) = tokio::join!(send, receive);
        assert!(matches!(out.unwrap(), ClientSendOutcome::Executed { .. }));
    }
    disconnected(&state, id).await;
    assert!(state.list_intercepts().await.is_empty());
    state.remove_client(id).await;
}
#[tokio::test]
async fn followup_depth_and_handler_action_capacity_are_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let action = json!({"type":"write_remote_samples","batch":batch()});
    let handlers = Some(vec![
        json!({"event_pattern":"remote_write_connected","handler":{"type":"static","actions":[action.clone()]}}),
        json!({"event_pattern":"remote_write_response","handler":{"type":"static","actions":[action]}}),
    ]);
    let (state, id) = client(listener.local_addr().unwrap().to_string(), handlers, None).await;
    for _ in 0..netget::client::prometheus_remote_write::MAX_FOLLOWUP_DEPTH {
        let (mut peer, _) = listener.accept().await.unwrap();
        received(&mut peer).await;
        reply(&mut peer, 204, b"").await;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err()
    );
    let out = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(out, ClientSendOutcome::Disconnected));
    state.remove_client(id).await;
    let action =
        serde_json::to_string(&json!({"type":"write_remote_samples","batch":batch()})).unwrap();
    let code=format!("import json,sys\njson.load(sys.stdin)\nprint(json.dumps({{'actions':[json.loads({action:?})]*33}}))");
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"remote_write_connected","handler":{"type":"script","language":"python","code":code}})]),None).await;
    disconnected(&state, id).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn response_body_header_bounds_and_exchange_deadline_do_not_confirm_acceptance() {
    use netget::client::prometheus_remote_write::transport::{self, Config, Origin};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for n in [
        transport::MAX_RESPONSE_BYTES,
        transport::MAX_RESPONSE_BYTES + 1,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            origin: Origin::parse(&listener.local_addr().unwrap().to_string()).unwrap(),
            token: None,
            path: "/api/v1/write".into(),
            retry_429: false,
        };
        let encoded =
            codec::encode_batch(&serde_json::from_value::<WriteBatch>(batch()).unwrap()).unwrap();
        let write = transport::write_once(&config, &encoded);
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            reply(&mut peer, 200, &vec![0xff; n]).await;
        };
        let (out, _) = tokio::join!(write, receive);
        assert_eq!(out.is_ok(), n == transport::MAX_RESPONSE_BYTES);
    }
    for header in [
        (0..65)
            .map(|n| format!("X-{n}: value\r\n"))
            .collect::<String>(),
        format!(
            "X-Large: {}\r\n",
            "x".repeat(transport::MAX_RESPONSE_HEADER_BYTES + 1)
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            origin: Origin::parse(&listener.local_addr().unwrap().to_string()).unwrap(),
            token: None,
            path: "/api/v1/write".into(),
            retry_429: false,
        };
        let encoded = vec![0];
        let write = transport::write_once(&config, &encoded);
        let receive = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            received(&mut peer).await;
            peer.write_all(
                format!("HTTP/1.1 200 OK\r\n{header}Content-Length: 0\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        };
        let (out, _) = tokio::join!(write, receive);
        assert!(out.is_err());
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        origin: Origin::parse(&listener.local_addr().unwrap().to_string()).unwrap(),
        token: None,
        path: "/api/v1/write".into(),
        retry_429: false,
    };
    let encoded = vec![0];
    let began = std::time::Instant::now();
    let write = transport::write_once(&config, &encoded);
    let receive = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        received(&mut peer).await;
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
    };
    let (out, _) = tokio::join!(write, receive);
    assert!(out.is_err());
    assert!(began.elapsed() >= Duration::from_secs(9));
    assert!(began.elapsed() < Duration::from_secs(20));
}
#[tokio::test]
async fn removal_cancels_pending_http_driver_and_releases_parked_intercept() {
    use tokio::io::AsyncReadExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let(state,id)=client(listener.local_addr().unwrap().to_string(),Some(vec![json!({"event_pattern":"remote_write_connected","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    let send = state.send_to_client(
        id,
        json!({"type":"write_remote_samples","batch":batch()}),
        Duration::from_secs(10),
    );
    let receive = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        received(&mut peer).await;
        state.remove_client(id).await;
        let mut bytes = vec![];
        tokio::time::timeout(Duration::from_secs(5), peer.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.is_empty());
    };
    let (out, _) = tokio::join!(send, receive);
    assert!(out.is_err());
    assert!(!state.has_client_handle(id).await);
    assert!(state.list_intercepts().await.is_empty());
}
