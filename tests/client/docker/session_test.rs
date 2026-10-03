use super::common::*;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};
async fn head(peer: &mut TcpStream) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut data = Vec::new();
        while !data.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            peer.read_exact(&mut byte).await.unwrap();
            data.push(byte[0]);
            assert!(data.len() < 32768);
        }
        String::from_utf8(data).unwrap()
    })
    .await
    .unwrap()
}
async fn response(peer: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    peer.write_all(format!("HTTP/1.1 {status} Response\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",body.len()).as_bytes()).await.unwrap();
    peer.write_all(body).await.unwrap();
}
async fn negotiate(listener: &TcpListener, version: &str) {
    let (mut peer, _) = listener.accept().await.unwrap();
    let request = head(&mut peer).await;
    assert!(request.starts_with("HEAD /_ping HTTP/1.1\r\n"), "{request}");
    response(
        &mut peer,
        200,
        &format!("API-Version: {version}\r\nOSType: linux\r\nDocker-Experimental: false\r\n"),
        b"",
    )
    .await;
}
async fn closed(peer: &mut TcpStream) {
    let mut data = Vec::new();
    if let Err(e) = peer.read_to_end(&mut data).await {
        assert!(
            matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            "{e}"
        );
    }
}
#[tokio::test]
async fn netget_pair_covers_all_read_resources_native_query_and_missing_inspect() {
    use netget::llm::actions::protocol_trait::Protocol;
    let state = state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let examples = netget::server::docker::actions::DockerProtocol::new().get_startup_examples();
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sid = netget::cli::management::ServerForm {
        protocol: "docker".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        event_handlers: Some(
            serde_json::from_value(examples.script_mode["event_handlers"].clone()).unwrap(),
        ),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(a) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = connected_client(&state, addr.to_string()).await;
    for op in [
        "ping",
        "version",
        "info",
        "containers",
        "container",
        "images",
        "networks",
        "volumes",
    ] {
        let mut action = json!({"operation":op});
        if op == "container" {
            action["container_id"] = json!("web");
        }
        if op == "containers" {
            action["all"] = json!(true);
            action["filters"] = json!({"label":["owner=a&b=two"]});
        }
        let result = request(&state, id, action).await;
        assert_eq!(result["api_version"], "1.47");
        assert_eq!(result["operation"], op);
        let data = &result["data"];
        match op {
            "container" => {
                assert_eq!(data["config"]["image"], "nginx:1.27");
                assert_eq!(data["state"]["status"], "running");
                assert!(data["network_settings"].is_object());
            }
            "containers" => {
                assert_eq!(data[0]["ports"][0]["public_port"], 8080);
                assert_eq!(data[0]["names"][0], "/web");
            }
            "images" => assert_eq!(data[0]["repo_tags"][0], "nginx:1.27"),
            "volumes" => assert_eq!(data["volumes"], json!([])),
            _ => {}
        }
    }
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(sid.as_u32())),
            None,
        )
        .await;
    let list = logs
        .iter()
        .find(|e| e.event_type == "docker_api_request" && e.request["resource"] == "containers")
        .unwrap();
    assert_eq!(list.request["query"]["all"], "true");
    assert_eq!(
        serde_json::from_str::<Value>(list.request["query"]["filters"].as_str().unwrap()).unwrap(),
        json!({"label":["owner=a&b=two"]})
    );
    let after = latest(&state, id).await;
    send(
        &state,
        id,
        json!({"operation":"container","container_id":"missing"}),
    )
    .await;
    let error = event(&state, id, "docker_request_error", after).await.1;
    assert_eq!(error["status"], 404);
    assert_eq!(error["error"], "No such container: missing");
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn negotiation_selects_lower_daemon_max_and_rejects_bad_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        negotiate(&listener, "1.41").await;
        let (mut peer, _) = listener.accept().await.unwrap();
        assert!(head(&mut peer)
            .await
            .starts_with("GET /v1.41/volumes HTTP/1.1"));
        response(
            &mut peer,
            200,
            "Content-Type: application/json\r\n",
            br#"{"Volumes":null,"Warnings":null}"#,
        )
        .await;
    });
    let state = state();
    let id = connected_client(&state, addr.to_string()).await;
    let result = request(&state, id, json!({"operation":"volumes"})).await;
    assert_eq!(result["api_version"], "1.41");
    assert!(result["data"]["volumes"].is_null());
    fixture.await.unwrap();
    state.remove_client(id).await;
    for headers in [
        "API-Version: 1.20\r\n",
        "API-Version: 1.47\r\nAPI-Version: 1.46\r\n",
        "API-Version: 1.+47\r\n",
        "",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = headers.to_owned();
        let fixture = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            response(&mut peer, 200, &h, b"").await;
        });
        let error = refused_start(&state, addr.to_string(), json!({})).await;
        assert!(
            error.contains("API") || error.contains("api-version"),
            "{error}"
        );
        fixture.await.unwrap();
    }
}
#[tokio::test]
async fn fragmented_responses_and_http_schema_failures_are_recoverable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cases = vec![
        (
            404,
            "Content-Type: application/json\r\n",
            br#"{"message":"No such container: fixture"}"#.to_vec(),
            "No such container",
        ),
        (
            302,
            "Location: http://127.0.0.1:1\r\n",
            b"redirect".to_vec(),
            "HTTP 302",
        ),
        (
            200,
            "Content-Type: text/html\r\n",
            b"[]".to_vec(),
            "application/json",
        ),
        (
            200,
            "Content-Type: application/json\r\nContent-Encoding: gzip\r\n",
            b"[]".to_vec(),
            "Content-Encoding",
        ),
        (
            200,
            "Content-Type: application/json\r\n",
            b"{}".to_vec(),
            "schema",
        ),
    ];
    let expected: Vec<_> = cases.iter().map(|(s, _, _, n)| (*s, *n)).collect();
    let fixture = tokio::spawn(async move {
        negotiate(&listener, "1.55").await;
        for (status, headers, body, _) in cases {
            let (mut peer, _) = listener.accept().await.unwrap();
            assert!(head(&mut peer).await.starts_with("GET /v1.47/images/json"));
            response(&mut peer, status, headers, &body).await;
        }
        let (mut peer, _) = listener.accept().await.unwrap();
        head(&mut peer).await;
        peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
        for byte in b"[]" {
            peer.write_all(format!("1\r\n{}\r\n", *byte as char).as_bytes())
                .await
                .unwrap();
        }
        peer.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let state = state();
    let id = connected_client(&state, addr.to_string()).await;
    for (status, needle) in expected {
        let after = latest(&state, id).await;
        send(&state, id, json!({"operation":"images"})).await;
        let e = event(&state, id, "docker_request_error", after).await.1;
        assert_eq!(e["status"], status);
        assert!(e["error"].as_str().unwrap().contains(needle), "{e}");
    }
    assert_eq!(
        request(&state, id, json!({"operation":"images"})).await["data"],
        json!([])
    );
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn disconnect_and_removal_cancel_manual_handler_and_stalled_body() {
    for remove in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = oneshot::channel();
        let fixture = tokio::spawn(async move {
            negotiate(&listener, "1.47").await;
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n[").await.unwrap();
            ready_tx.send(()).unwrap();
            closed(&mut peer).await;
        });
        let state = state();
        let id = client(
            &state,
            addr.to_string(),
            json!({}),
            vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":60}})],
        )
        .await;
        assert_eq!(state.client_task_count(id).await, 2);
        send(&state, id, json!({"operation":"images"})).await;
        ready_rx.await.unwrap();
        rejected(
            &state,
            id,
            json!({"type":"docker_request","operation":"info"}),
            "pending",
        )
        .await;
        if remove {
            state.remove_client(id).await;
        } else {
            let outcome = state
                .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
                .await
                .unwrap();
            assert!(matches!(
                outcome,
                netget::state::client_handles::ClientSendOutcome::Disconnected
            ));
        }
        tokio::time::timeout(Duration::from_secs(1), fixture)
            .await
            .unwrap()
            .unwrap();
        state.remove_client(id).await;
        assert_eq!(state.client_task_count(id).await, 0);
    }
}
#[tokio::test]
async fn whole_request_deadline_covers_partial_body_and_closes_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        negotiate(&listener, "1.47").await;
        let (mut peer, _) = listener.accept().await.unwrap();
        head(&mut peer).await;
        peer.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n[",
        )
        .await
        .unwrap();
        closed(&mut peer).await;
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!({"request_timeout_secs":1}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let after = latest(&state, id).await;
    send(&state, id, json!({"operation":"images"})).await;
    let error = event(&state, id, "docker_request_error", after).await.1;
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("whole Docker request deadline"));
    assert!(error["status"].is_null());
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn content_length_and_chunked_body_limits_report_no_partial_success() {
    for chunked in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            negotiate(&listener, "1.47").await;
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            if chunked {
                peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                let chunk = vec![b' '; 65536];
                for _ in 0..66 {
                    if peer.write_all(b"10000\r\n").await.is_err() {
                        break;
                    }
                    if peer.write_all(&chunk).await.is_err() {
                        break;
                    }
                    if peer.write_all(b"\r\n").await.is_err() {
                        break;
                    }
                }
            } else {
                peer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",netget::client::docker::schema::MAX_BODY+1).as_bytes()).await.unwrap();
            }
            let mut data = Vec::new();
            let _ = peer.read_to_end(&mut data).await;
        });
        let state = state();
        let id = connected_client(&state, addr.to_string()).await;
        let after = latest(&state, id).await;
        send(&state, id, json!({"operation":"images"})).await;
        let error = event(&state, id, "docker_request_error", after).await.1;
        assert!(
            error["error"].as_str().unwrap().contains(if chunked {
                "bounded Docker body"
            } else {
                "body limit"
            }),
            "{error}"
        );
        assert!(error.get("data").is_none());
        tokio::time::timeout(Duration::from_secs(2), fixture)
            .await
            .unwrap()
            .unwrap();
        state.remove_client(id).await;
    }
}
#[tokio::test]
async fn parked_dispatcher_event_queue_stays_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        negotiate(&listener, "1.47").await;
        for _ in 0..=netget::client::docker::QUEUE_CAPACITY {
            let (mut peer, _) = listener.accept().await.unwrap();
            head(&mut peer).await;
            response(&mut peer, 200, "Content-Type: application/json\r\n", b"[]").await;
        }
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!({}),
        vec![json!({"event_pattern":"*","handler":{"type":"manual","timeout_secs":60}})],
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5),async{while !state.list_intercepts().await.iter().any(|e|matches!(e.owner,netget::state::intercepts::InterceptOwner::Client(owner) if owner==id)){tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
    for _ in 0..=netget::client::docker::QUEUE_CAPACITY {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let outcome = state
                    .send_to_client(
                        id,
                        json!({"type":"docker_request","operation":"images"}),
                        Duration::from_secs(1),
                    )
                    .await
                    .unwrap();
                match outcome {
                    netget::state::client_handles::ClientSendOutcome::Executed { .. } => break,
                    netget::state::client_handles::ClientSendOutcome::Rejected { error } => {
                        assert!(error.contains("pending"), "{error}")
                    }
                    other => panic!("{other:?}"),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    failed(&state, id, "event queue full").await;
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn followup_limit_preserves_new_injection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        negotiate(&listener, "1.47").await;
        for n in 0..=netget::client::docker::MAX_FOLLOWUPS {
            let (mut peer, _) = listener.accept().await.unwrap();
            let h = head(&mut peer).await;
            assert!(
                h.starts_with(if n == netget::client::docker::MAX_FOLLOWUPS {
                    "GET /v1.47/networks "
                } else {
                    "GET /v1.47/images/json "
                }),
                "{h}"
            );
            response(&mut peer, 200, "Content-Type: application/json\r\n", b"[]").await;
        }
    });
    let state = state();
    let id=client(&state,addr.to_string(),json!({}),vec![static_handler("docker_connected",json!([{"type":"docker_request","operation":"images"}])),json!({"event_pattern":"docker_response","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\njson.dump({'actions':[{'type':'docker_request','operation':'images'}] if e['operation']=='images' else []},sys.stdout)"}})]).await;
    let mut after = 0;
    for _ in 0..netget::client::docker::MAX_FOLLOWUPS {
        after = event(&state, id, "docker_response", after).await.0;
    }
    send(&state, id, json!({"operation":"networks"})).await;
    assert_eq!(
        event(&state, id, "docker_response", after).await.1["operation"],
        "networks"
    );
    fixture.await.unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn refused_origins_and_startup_values_fail_before_any_read_request() {
    let state = state();
    for endpoint in [
        "https://127.0.0.1:2376",
        "http://user:secret@127.0.0.1:2375",
        "http://127.0.0.1:2375/path",
        "http://127.0.0.1:2375/?x=1",
        "unix://relative.sock",
    ] {
        let error = refused_start(&state, endpoint.into(), json!({})).await;
        assert!(
            error.contains("Docker") || error.contains("Unix"),
            "{error}"
        );
    }
    for params in [
        json!({"api_version":"1.23"}),
        json!({"api_version":"1.48"}),
        json!({"api_version":1.47}),
        json!({"request_timeout_secs":0}),
        json!({"request_timeout_secs":31}),
        json!({"request_timeout_secs":"1"}),
    ] {
        let error = refused_start(&state, "127.0.0.1:1".into(), params).await;
        assert!(
            error.contains("api_version") || error.contains("request_timeout_secs"),
            "{error}"
        );
    }
}
#[tokio::test]
async fn stalled_negotiation_has_a_whole_deadline_and_reaps_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        head(&mut peer).await;
        closed(&mut peer).await;
    });
    let state = state();
    let error = refused_start(&state, addr.to_string(), json!({"request_timeout_secs":1})).await;
    assert!(error.contains("negotiation deadline"), "{error}");
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
}
