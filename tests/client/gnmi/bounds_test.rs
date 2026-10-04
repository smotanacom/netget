use crate::helpers::gnmi_peer as peer;
use netget::state::{client_handles::ClientSendOutcome, AppState, ClientId, ClientStatus};
use serde_json::{json, Value};
use std::time::Duration;
async fn queued(state: &AppState, id: ClientId, action: Value) {
    assert!(matches!(
        peer::send(state, id, action).await.unwrap(),
        ClientSendOutcome::Executed { .. }
    ));
}
fn get(id: u32, name: &str, encoding: &str, size: Option<usize>) -> Value {
    let mut path = peer::path(name);
    if let Some(size) = size {
        path["elem"][0]["key"]["size"] = json!(size.to_string());
    }
    json!({"type":"gnmi_get","call_id":id,"gzip":true,"request":{"path":[path],"encoding":encoding}})
}
fn subscribe(id: u32, mode: &str, name: &str) -> Value {
    json!({"type":"gnmi_subscribe","call_id":id,"request":{"mode":mode,"subscription":[{"path":peer::path(name)}],"encoding":"PROTO"}})
}
async fn disconnected(state: &AppState, id: ClientId) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .get_client(id)
                .await
                .is_some_and(|c| c.status == ClientStatus::Disconnected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!state.has_client_handle(id).await);
}
#[tokio::test]
async fn json_ietf_ascii_and_proto_response_values_are_decoded() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(&state, sdk.port, peer::wait_handlers(), json!({}))
        .await
        .unwrap();
    for (call, encoding, kind) in [
        (1, "PROTO", "uint"),
        (2, "JSON", "json"),
        (3, "JSON_IETF", "json_ietf"),
        (4, "ASCII", "ascii"),
    ] {
        queued(&state, id, get(call, "system", encoding, None)).await;
        assert_eq!(
            peer::log(&state, id, "gnmi_client_ended", call).await["code"],
            0
        );
        let response = peer::log(&state, id, "gnmi_client_response", call).await;
        assert_eq!(
            response["response"]["notification"][0]["update"][0]["value"]["kind"],
            kind
        );
    }
    state.remove_client(id).await;
}
#[tokio::test]
async fn exact_gzip_response_and_json_bounds_plus_one_opaque_and_nonfinite() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(&state, sdk.port, peer::wait_handlers(), json!({}))
        .await
        .unwrap();
    for (call, name, encoding, size, code) in [
        (1, "message-bound", "PROTO", Some(1024 * 1024), 0),
        (2, "message-bound", "PROTO", Some(1024 * 1024 + 1), 8),
        (3, "json-bound", "JSON", Some(65536), 0),
        (4, "json-bound", "JSON", Some(65537), 8),
        (5, "opaque", "PROTO", None, 12),
        (6, "nonfinite", "PROTO", None, 3),
    ] {
        queued(&state, id, get(call, name, encoding, size)).await;
        assert_eq!(
            peer::log(&state, id, "gnmi_client_ended", call).await["code"],
            code,
            "scenario {name}, {size:?}"
        );
    }
    state.remove_client(id).await;
}
#[tokio::test]
async fn independent_subscriptions_once_poll_stream_updates_only_and_bad_sync() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(&state, sdk.port, peer::wait_handlers(), json!({}))
        .await
        .unwrap();
    for (call, mode, name, code, count) in [
        (1, "ONCE", "system", 0, 2),
        (2, "STREAM", "system", 0, 5),
        (3, "ONCE", "duplicate-sync", 3, 2),
        (4, "ONCE", "missing-sync", 3, 1),
        (5, "STREAM", "response-count", 8, 256),
    ] {
        queued(&state, id, subscribe(call, mode, name)).await;
        let end = peer::log(&state, id, "gnmi_client_ended", call).await;
        assert_eq!(end["code"], code);
        assert_eq!(end["response_count"], count);
    }
    let mut only = subscribe(6, "ONCE", "system");
    only["request"]["updates_only"] = json!(true);
    queued(&state, id, only).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 6).await["response_count"],
        1
    );
    queued(&state, id, subscribe(7, "POLL", "system")).await;
    peer::log(&state, id, "gnmi_client_sync", 7).await;
    queued(&state, id, json!({"type":"gnmi_poll","call_id":7})).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Client(id.as_u32())),
                    None,
                )
                .await
                .iter()
                .any(|e| {
                    e.event_type == "gnmi_client_sync"
                        && e.request["call_id"] == 7
                        && e.request["sequence"] == 4
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    queued(&state, id, json!({"type":"gnmi_cancel","call_id":7})).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 7).await["code"],
        1
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn sixteen_active_calls_remain_cancellable_and_capacity_recovers() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(&state, sdk.port, peer::wait_handlers(), json!({}))
        .await
        .unwrap();
    for call in 1..=16 {
        queued(&state, id, get(call, "parked", "PROTO", None)).await;
    }
    assert!(peer::send(&state, id, get(17, "system", "PROTO", None))
        .await
        .is_err());
    for call in 1..=16 {
        queued(&state, id, json!({"type":"gnmi_cancel","call_id":call})).await;
        assert_eq!(
            peer::log(&state, id, "gnmi_client_ended", call).await["code"],
            1
        );
    }
    queued(&state, id, get(17, "system", "PROTO", None)).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 17).await["code"],
        0
    );
    state.remove_client(id).await;
}
#[tokio::test]
async fn parked_calls_have_whole_deadlines_and_idle_closes_owned_socket() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(
        &state,
        sdk.port,
        peer::wait_handlers(),
        json!({"rpc_timeout_secs":1,"idle_timeout_secs":1}),
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    queued(&state, id, get(1, "parked", "PROTO", None)).await;
    let end = peer::log(&state, id, "gnmi_client_ended", 1).await;
    // Tonic's pre-header timeout maps expiry to CANCELLED and can win just
    // before our whole-RPC timer; retain the actual status without relabeling.
    assert!([json!(1), json!(4)].contains(&end["code"]));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(2));
    println!(
        "gNMI parked RPC ended after {elapsed:?}, code={}",
        end["code"]
    );
    disconnected(&state, id).await;
    state.remove_client(id).await;
}
#[tokio::test]
async fn local_deadline_cancels_peer_ignoring_grpc_timeout_and_releases_call() {
    use bytes::Bytes;
    use prost::Message;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (reset_tx, reset_rx) = tokio::sync::oneshot::channel();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (request, mut response) = connection.accept().await.unwrap().unwrap();
        assert!(request.headers().contains_key("grpc-timeout"));
        // Successful headers finish tonic's own header-only timeout. Park the
        // body and ignore grpc-timeout to isolate NetGet's whole-RPC timer.
        let mut parked = response.send_response(
            http::Response::builder().status(200).header("content-type", "application/grpc").body(()).unwrap(), false
        ).unwrap();
        let reset = futures::future::poll_fn(|cx| parked.poll_reset(cx));
        tokio::pin!(reset);
        tokio::select! {
            reason = &mut reset => {
                assert_eq!(reason.unwrap(), h2::Reason::CANCEL);
                reset_tx.send(()).unwrap();
            },
            request = connection.accept() => panic!("unexpected second request before reset: {request:?}"),
        }
        let (request, mut response) = connection.accept().await.unwrap().unwrap();
        assert_eq!(request.uri().path(), "/gnmi.gNMI/Capabilities");
        let mut body = response.send_response(
            http::Response::builder().status(200).header("content-type", "application/grpc").body(()).unwrap(), false
        ).unwrap();
        let encoded = netget::server::gnmi::proto::gnmi::CapabilityResponse {
            supported_encodings: vec![2], g_nmi_version: "0.10.0".into(), ..Default::default()
        }.encode_to_vec();
        let mut frame = vec![0];
        frame.extend((encoded.len() as u32).to_be_bytes());
        frame.extend(encoded);
        body.send_data(Bytes::from(frame), false).unwrap();
        let mut trailers = http::HeaderMap::new();
        trailers.insert("grpc-status", "0".parse().unwrap());
        body.send_trailers(trailers).unwrap();
        while connection.accept().await.is_some() {}
    });
    let state = peer::state().await;
    let id = peer::client_id(
        &state,
        port,
        peer::wait_handlers(),
        json!({"rpc_timeout_secs":1}),
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    queued(&state, id, json!({"type":"gnmi_capabilities","call_id":1})).await;
    let end = peer::log(&state, id, "gnmi_client_ended", 1).await;
    assert_eq!(end["code"], 4);
    assert_eq!(end["response_count"], 0);
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(2));
    tokio::time::timeout(Duration::from_secs(2), reset_rx)
        .await
        .unwrap()
        .unwrap();
    queued(&state, id, json!({"type":"gnmi_capabilities","call_id":2})).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 2).await["code"],
        0
    );
    println!("gNMI local deadline against timeout-ignoring peer: {elapsed:?}; RST_STREAM CANCEL and next call succeeded");
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(3), tasks.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn client_drop_cancels_subscription_and_parked_event_handlers() {
    let sdk = peer::Peer::server(false).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(
        &state,
        sdk.port,
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
        json!({}),
    )
    .await
    .unwrap();
    queued(&state, id, subscribe(1, "STREAM", "live")).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state.list_intercepts().await.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        peer::send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    disconnected(&state, id).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if state.list_intercepts().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn verified_custom_ca_tls_and_hostname_or_untrusted_certificate_refusal() {
    let sdk = peer::Peer::server(true).await.unwrap();
    let state = peer::state().await;
    let id = peer::client_id(
        &state,
        sdk.port,
        peer::wait_handlers(),
        json!({"use_tls":true,"server_name":"localhost","ca_file":sdk.ca_file}),
    )
    .await
    .unwrap();
    queued(&state, id, json!({"type":"gnmi_capabilities","call_id":1})).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 1).await["code"],
        0
    );
    assert_eq!(
        state
            .get_client(id)
            .await
            .unwrap()
            .connection
            .unwrap()
            .protocol_info
            .get("tls_verified"),
        Some(&json!(true))
    );
    state.remove_client(id).await;
    for params in [
        json!({"use_tls":true,"server_name":"wrong.example","ca_file":sdk.ca_file}),
        json!({"use_tls":true,"server_name":"localhost"}),
    ] {
        let (sender, _) = tokio::sync::mpsc::unbounded_channel();
        let result = netget::cli::management::ClientForm {
            protocol: "gnmi".into(),
            remote_addr: Some(format!("127.0.0.1:{}", sdk.port)),
            instruction: Some(String::new()),
            startup_params: Some(params),
            event_handlers: Some(peer::wait_handlers()),
            ..Default::default()
        }
        .create(
            &state,
            netget::llm::OllamaClient::new("http://127.0.0.1:1"),
            sender,
        )
        .await;
        assert!(
            result.is_err(),
            "invalid certificate must fail within create(), before publishing a client handle"
        );
    }
}
#[tokio::test]
async fn bracketed_ipv6_authority_uses_literal_socket_address() {
    let state = peer::state().await;
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let server = netget::cli::management::ServerForm {
        protocol: "gnmi".into(),
        host: Some("::1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params: Some(json!({})),
        event_handlers: Some(peer::handlers()),
        ..Default::default()
    }
    .create(&state, sender)
    .await
    .unwrap();
    let port = state
        .get_server(server)
        .await
        .unwrap()
        .local_addr
        .unwrap()
        .port();
    let (sender, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ClientForm {
        protocol: "gnmi".into(),
        remote_addr: Some(format!("[::1]:{port}")),
        instruction: Some(String::new()),
        startup_params: Some(json!({})),
        event_handlers: Some(peer::wait_handlers()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        sender,
    )
    .await
    .unwrap();
    queued(&state, id, json!({"type":"gnmi_capabilities","call_id":1})).await;
    assert_eq!(
        peer::log(&state, id, "gnmi_client_ended", 1).await["code"],
        0
    );
    state.remove_client(id).await;
    state.remove_server(server).await.unwrap();
}
