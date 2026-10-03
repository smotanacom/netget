use netget::{
    cli::management::ClientForm,
    state::{client_handles::ClientSendOutcome, AccessLogOwner, AppState, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
#[path = "../../helpers/grpc_peer.rs"]
mod peer;

fn silent() -> Value {
    json!({"event_pattern":"*","handler":{"type":"static","actions":[{"type":"wait_for_more"}]}})
}
async fn client(state: &AppState, port: u16, parameters: Value, handlers: Vec<Value>) -> ClientId {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "grpc".into(),
        remote_addr: Some(format!("127.0.0.1:{port}")),
        instruction: Some("Exercise typed streaming.".into()),
        startup_params: Some(parameters),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
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
    .unwrap();
    id
}
async fn send(state: &AppState, id: ClientId, action: Value) -> anyhow::Result<ClientSendOutcome> {
    state
        .send_to_client(id, action, Duration::from_secs(3))
        .await
}
async fn event(
    state: &AppState,
    id: ClientId,
    kind: &str,
    stream_id: u32,
    key: &str,
    value: Value,
) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(log) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .find(|log| {
                    log.event_type == kind
                        && log.request["stream_id"] == stream_id
                        && (key.is_empty() || log.request[key] == value)
                })
            {
                return log.request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("missing {kind} stream {stream_id}, {key}={value}"))
}
fn start(id: u32, method: &str, request: Value) -> Value {
    json!({"type":"grpc_stream_start","stream_id":id,"service":"streams.Session","method":method,"request":request,"gzip":true})
}
async fn queued(state: &AppState, id: ClientId, action: Value) {
    let result = send(state, id, action).await.unwrap();
    assert!(
        matches!(result, ClientSendOutcome::Executed { .. }),
        "{result:?}"
    );
}
async fn gone(state: &AppState, id: ClientId) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.has_client_handle(id).await || !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn grpcio_reflection_client_all_stream_shapes_and_half_close() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    // No proto_schema: the generated C++ peer exposes v1alpha reflection.
    let id = client(&state, peer.port, json!({}), vec![silent()]).await;
    queued(
        &state,
        id,
        start(
            1,
            "Watch",
            json!({"name":"watch","tags":["blue","green"],"counts":{"copies":2}}),
        ),
    )
    .await;
    let ended = event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await;
    assert_eq!(ended["code"], 0);
    assert_eq!(ended["response_count"], 3);
    let message = event(
        &state,
        id,
        "grpc_stream_message_received",
        1,
        "sequence",
        json!(3),
    )
    .await;
    assert_eq!(message["response"]["name"], "watch-2");
    assert_eq!(message["response"]["tags"], json!(["blue", "green"]));
    assert_eq!(message["response"]["counts"], json!({"copies":2}));
    queued(&state, id, start(2, "Collect", json!({"value":2}))).await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        2,
        "input_sequence",
        json!(1),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_send","stream_id":2,"message":{"value":3}}),
    )
    .await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        2,
        "input_sequence",
        json!(2),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_finish","stream_id":2}),
    )
    .await;
    let response = event(
        &state,
        id,
        "grpc_stream_message_received",
        2,
        "",
        Value::Null,
    )
    .await;
    assert_eq!(response["response"]["value"], 5);
    assert_eq!(response["response"]["counts"], json!({"messages":2}));
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 2, "", Value::Null).await["code"],
        0
    );
    queued(
        &state,
        id,
        start(3, "Chat", json!({"name":"one","value":4})),
    )
    .await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        3,
        "input_sequence",
        json!(1),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_send","stream_id":3,"message":{"name":"two","value":8}}),
    )
    .await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        3,
        "input_sequence",
        json!(2),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_finish","stream_id":3}),
    )
    .await;
    for (sequence, name, value) in [(1, "one", 5), (2, "two", 9)] {
        let response = event(
            &state,
            id,
            "grpc_stream_message_received",
            3,
            "sequence",
            json!(sequence),
        )
        .await;
        assert_eq!(response["response"]["name"], name);
        assert_eq!(response["response"]["value"], value);
    }
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 3, "", Value::Null).await["response_count"],
        2
    );
    queued(
        &state,
        id,
        start(4, "Watch", json!({"name":"subscription"})),
    )
    .await;
    event(
        &state,
        id,
        "grpc_stream_message_received",
        4,
        "sequence",
        json!(1),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_cancel","stream_id":4}),
    )
    .await;
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 4, "", Value::Null).await["code"],
        1
    );
    assert!(
        send(&state, id, start(4, "Watch", json!({})))
            .await
            .is_err(),
        "ids are not reused after cancellation"
    );
    assert!(matches!(
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    gone(&state, id).await;
}

#[tokio::test]
async fn client_controls_stay_responsive_with_sixteen_parked_handlers() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = client(
        &state,
        peer.port,
        json!({}),
        vec![
            json!({"event_pattern":"grpc_stream_message_received","handler":{"type":"manual"}}),
            silent(),
        ],
    )
    .await;
    queued(
        &state,
        id,
        start(1, "Watch", json!({"name":"subscription"})),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.len() != 16 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(send(&state, id, start(2, "Watch", json!({})))
        .await
        .unwrap_err()
        .to_string()
        .contains("capacity"));
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_cancel","stream_id":1}),
    )
    .await;
    assert!(matches!(
        send(&state, id, json!({"type":"disconnect"}))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    gone(&state, id).await;
}

#[tokio::test]
async fn client_stream_deadline_idle_and_removal_cancel_owned_work() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = client(
        &state,
        peer.port,
        json!({"stream_timeout_secs":1}),
        vec![silent()],
    )
    .await;
    queued(
        &state,
        id,
        start(1, "Watch", json!({"name":"subscription"})),
    )
    .await;
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await["code"],
        4
    );
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
    let idle = client(
        &state,
        peer.port,
        json!({"idle_timeout_secs":1}),
        vec![silent()],
    )
    .await;
    gone(&state, idle).await;
    let removed = client(
        &state,
        peer.port,
        json!({}),
        vec![
            json!({"event_pattern":"grpc_stream_message_received","handler":{"type":"manual"}}),
            silent(),
        ],
    )
    .await;
    queued(
        &state,
        removed,
        start(1, "Watch", json!({"name":"subscription"})),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(removed).await.unwrap();
    gone(&state, removed).await;
}

#[tokio::test]
async fn invalid_stream_actions_leave_connection_usable() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = client(&state, peer.port, json!({}), vec![silent()]).await;
    for action in [
        start(1, "Echo", json!({})),
        start(1, "Watch", json!({"value":"wrong"})),
        json!({"type":"grpc_stream_start","stream_id":1,"service":"streams.Session","method":"Watch","request":{},"metadata":{"grpc-timeout":"10S"}}),
        json!({"type":"grpc_stream_start","stream_id":1,"service":"streams.Session","method":"Watch","request":{},"metadata":{"secret-bin":"opaque"}}),
    ] {
        assert!(send(&state, id, action).await.is_err());
    }
    assert!(matches!(
        send(&state, id, start(0, "Watch", json!({})))
            .await
            .unwrap(),
        ClientSendOutcome::Rejected { .. }
    ));
    queued(
        &state,
        id,
        start(1, "Watch", json!({"name":"after-refusal"})),
    )
    .await;
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await["code"],
        0
    );
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
}

#[tokio::test]
async fn grpcio_message_exact_bound_plus_one_plain_and_gzip() {
    use prost::Message;
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = client(&state, peer.port, json!({}), vec![silent()]).await;
    let descriptor = peer::pool()
        .unwrap()
        .get_message_by_name("streams.Message")
        .unwrap();
    for (base, gzip) in [(10, false), (20, true)] {
        for (offset, length, code) in [(0, 4 * 1024 * 1024 - 5, 0), (1, 4 * 1024 * 1024 - 4, 8)] {
            let action = json!({"type":"grpc_stream_start","stream_id":base+offset,"service":"streams.Session","method":"Watch","request":{"name":"response-bound","value":offset},"gzip":gzip});
            queued(&state, id, action).await;
            let ended = event(
                &state,
                id,
                "grpc_stream_ended",
                base + offset,
                "",
                Value::Null,
            )
            .await;
            assert_eq!(ended["code"], code);
            if code == 0 {
                let response = event(
                    &state,
                    id,
                    "grpc_stream_message_received",
                    base,
                    "",
                    Value::Null,
                )
                .await;
                assert_eq!(response["response"]["name"].as_str().unwrap().len(), length);
            }
        }
        let request = json!({"name":"r".repeat(4*1024*1024-20),"tags":["request-bound"]});
        let message = netget::server::grpc::stream_codec::from_json(&request, &descriptor).unwrap();
        assert_eq!(message.encoded_len(), 4 * 1024 * 1024);
        let action = json!({"type":"grpc_stream_start","stream_id":base+2,"service":"streams.Session","method":"Watch","request":request,"gzip":gzip});
        queued(&state, id, action).await;
        assert_eq!(
            event(&state, id, "grpc_stream_ended", base + 2, "", Value::Null).await["code"],
            0
        );
        assert_eq!(
            event(
                &state,
                id,
                "grpc_stream_message_received",
                base + 2,
                "",
                Value::Null
            )
            .await["response"]["value"],
            4 * 1024 * 1024 - 20
        );
        let overflow = json!({"type":"grpc_stream_start","stream_id":base+3,"service":"streams.Session","method":"Watch","request":{"name":"r".repeat(4*1024*1024-19),"tags":["request-bound"]},"gzip":gzip});
        assert!(send(&state, id, overflow)
            .await
            .unwrap_err()
            .to_string()
            .contains("4 MiB"));
    }
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
}

#[tokio::test]
async fn grpcio_tls_custom_ca_verified_hostname_and_untrusted_negatives() {
    let peer = peer::Peer::tls().await.unwrap();
    let state = peer::state().await;
    let parameters = json!({"use_tls":true,"server_name":"localhost","ca_file":peer.ca_file});
    let id = client(&state, peer.port, parameters.clone(), vec![silent()]).await;
    queued(&state, id, start(1, "Watch", json!({"name":"verified"}))).await;
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await["code"],
        0
    );
    let connection = state.get_client(id).await.unwrap().connection.unwrap();
    assert_eq!(connection.connected_addr.unwrap().port(), peer.port);
    assert!(connection.local_addr.unwrap().port() > 0);
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
    // Direct connection API preserves the transport errors for these negatives.
    for mut parameters in [
        parameters,
        json!({"use_tls":true,"server_name":"localhost"}),
    ] {
        if parameters.get("ca_file").is_some() {
            parameters["server_name"] = json!("wrong.example");
        }
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let result = netget::client::grpc::GrpcClient::connect_with_llm_actions(
            format!("127.0.0.1:{}", peer.port),
            netget::llm::OllamaClient::new("http://127.0.0.1:1"),
            std::sync::Arc::new(state.clone()),
            tx,
            id,
            Some(
                netget::protocol::StartupParams::new(
                    parameters,
                    netget::llm::actions::protocol_trait::Protocol::get_startup_parameters(
                        &netget::client::grpc::GrpcClientProtocol::new(),
                    ),
                )
                .unwrap(),
            ),
        )
        .await;
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn netget_pair_reflection_and_all_stream_shapes() {
    let state = peer::state().await;
    let (server, port) = peer::netget_server(&state, vec![peer::semantic_handler()], json!({}))
        .await
        .unwrap();
    let id = client(&state, port, json!({}), vec![silent()]).await;
    queued(&state, id, start(1, "Watch", json!({"name":"watch"}))).await;
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await["response_count"],
        3
    );
    queued(&state, id, start(2, "Collect", json!({"value":2}))).await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        2,
        "input_sequence",
        json!(1),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_send","stream_id":2,"message":{"value":3}}),
    )
    .await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        2,
        "input_sequence",
        json!(2),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_finish","stream_id":2}),
    )
    .await;
    assert_eq!(
        event(
            &state,
            id,
            "grpc_stream_message_received",
            2,
            "",
            Value::Null
        )
        .await["response"]["value"],
        5
    );
    queued(
        &state,
        id,
        start(3, "Chat", json!({"name":"pair","value":7})),
    )
    .await;
    event(
        &state,
        id,
        "grpc_stream_input_ready",
        3,
        "input_sequence",
        json!(1),
    )
    .await;
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_finish","stream_id":3}),
    )
    .await;
    assert_eq!(
        event(
            &state,
            id,
            "grpc_stream_message_received",
            3,
            "",
            Value::Null
        )
        .await["response"]["value"],
        8
    );
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 3, "", Value::Null).await["code"],
        0
    );
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
    state.remove_server(server).await.unwrap();
}

#[tokio::test]
async fn independent_reflection_rejects_descriptor_bounds_and_changes() {
    use netget::llm::actions::protocol_trait::Protocol;
    let state = peer::state().await;
    for (mode, needle) in [
        ("too-many-files", "128 files"),
        ("oversized-descriptor", "4 MiB"),
        ("name-expansion", "filename/package too long"),
        ("changed-duplicate", "changed an immutable descriptor"),
    ] {
        let peer = peer::Peer::reflection(mode).await.unwrap();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let parameters = netget::protocol::StartupParams::new(
            json!({}),
            netget::client::grpc::GrpcClientProtocol::new().get_startup_parameters(),
        )
        .unwrap();
        let result = netget::client::grpc::GrpcClient::connect_with_llm_actions(
            format!("127.0.0.1:{}", peer.port),
            netget::llm::OllamaClient::new("http://127.0.0.1:1"),
            std::sync::Arc::new(state.clone()),
            tx,
            ClientId::new(1),
            Some(parameters),
        )
        .await;
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains(needle), "{mode}: {error:#}");
        assert!(!state.has_client_handle(ClientId::new(1)).await);
    }
}

#[tokio::test]
async fn independent_stream_input_and_response_counts_are_bounded() {
    let peer = peer::Peer::server().await.unwrap();
    let state = peer::state().await;
    let id = client(&state, peer.port, json!({}), vec![silent()]).await;
    queued(
        &state,
        id,
        start(1, "Watch", json!({"name":"response-count"})),
    )
    .await;
    let ended = event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await;
    assert_eq!(ended["code"], 8);
    assert_eq!(ended["response_count"], 256);
    queued(&state, id, start(2, "Collect", json!({"value":1}))).await;
    for sequence in 1..=256 {
        event(
            &state,
            id,
            "grpc_stream_input_ready",
            2,
            "input_sequence",
            json!(sequence),
        )
        .await;
        if sequence < 256 {
            queued(
                &state,
                id,
                json!({"type":"grpc_stream_send","stream_id":2,"message":{"value":1}}),
            )
            .await;
        }
    }
    assert!(send(
        &state,
        id,
        json!({"type":"grpc_stream_send","stream_id":2,"message":{"value":1}})
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("256 input"));
    queued(
        &state,
        id,
        json!({"type":"grpc_stream_finish","stream_id":2}),
    )
    .await;
    assert_eq!(
        event(
            &state,
            id,
            "grpc_stream_message_received",
            2,
            "",
            Value::Null
        )
        .await["response"]["counts"]["messages"],
        256
    );
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 2, "", Value::Null).await["code"],
        0
    );
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
}

#[tokio::test]
async fn startup_ca_exact_bound_and_nonregular_paths_fail_without_blocking() {
    use netget::llm::actions::protocol_trait::Protocol;
    let peer = peer::Peer::tls().await.unwrap();
    let state = peer::state().await;
    let directory = tempfile::tempdir().unwrap();
    let ca = directory.path().join("bounded.pem");
    let mut bytes = std::fs::read(peer.ca_file.as_ref().unwrap()).unwrap();
    bytes.resize(1024 * 1024, b'\n');
    std::fs::write(&ca, &bytes).unwrap();
    let id = client(
        &state,
        peer.port,
        json!({"use_tls":true,"server_name":"localhost","ca_file":ca}),
        vec![silent()],
    )
    .await;
    queued(&state, id, start(1, "Watch", json!({"name":"exact-ca"}))).await;
    assert_eq!(
        event(&state, id, "grpc_stream_ended", 1, "", Value::Null).await["code"],
        0
    );
    send(&state, id, json!({"type":"disconnect"}))
        .await
        .unwrap();
    gone(&state, id).await;
    bytes.push(b'\n');
    std::fs::write(&ca, bytes).unwrap();
    let schema = directory.path().join("oversized.pb");
    std::fs::write(&schema, vec![0u8; 4 * 1024 * 1024 + 1]).unwrap();
    let mut probes = vec![
        (
            json!({"use_tls":true,"ca_file":ca}),
            "CA must be a regular file at most 1 MiB",
        ),
        (
            json!({"use_tls":true,"ca_file":directory.path()}),
            "CA must be a regular file at most 1 MiB",
        ),
        (
            json!({"proto_schema":schema}),
            "schema file must be regular and at most 4 MiB",
        ),
    ];
    #[cfg(unix)]
    {
        for (filename, ca_file) in [("pipe.pem", true), ("pipe.pb", false)] {
            let path = directory.path().join(filename);
            assert!(tokio::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .await
                .unwrap()
                .success());
            probes.push(if ca_file {
                (
                    json!({"use_tls":true,"ca_file":path}),
                    "CA must be a regular file at most 1 MiB",
                )
            } else {
                (
                    json!({"proto_schema":path}),
                    "schema file must be regular and at most 4 MiB",
                )
            });
        }
    }
    for (parameters, needle) in probes {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let parameters = netget::protocol::StartupParams::new(
            parameters,
            netget::client::grpc::GrpcClientProtocol::new().get_startup_parameters(),
        )
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            netget::client::grpc::GrpcClient::connect_with_llm_actions(
                format!("127.0.0.1:{}", peer.port),
                netget::llm::OllamaClient::new("http://127.0.0.1:1"),
                std::sync::Arc::new(state.clone()),
                tx,
                id,
                Some(parameters),
            ),
        )
        .await
        .expect("nonregular path must fail before a blocking open");
        assert!(format!("{:#}", result.unwrap_err()).contains(needle));
        assert!(!state.has_client_handle(id).await);
    }
}
