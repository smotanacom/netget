use crate::helpers::connect_rpc_peer as peer;
use serde_json::json;
use std::time::Duration;
async fn raw(port: u16, method: &str, headers: Vec<(&str, &str)>, body: Vec<u8>) -> (u16, Vec<u8>) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut request = client.request(
        method.parse().unwrap(),
        format!(
            "http://127.0.0.1:{port}/streams.Session/{}",
            if headers
                .iter()
                .any(|(key, value)| *key == "content-type" && *value == "application/connect+proto")
            {
                "Watch"
            } else {
                "Echo"
            }
        ),
    );
    request = request.header("connect-protocol-version", "1");
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request.body(body).send().await.unwrap();
    (
        response.status().as_u16(),
        response.bytes().await.unwrap().to_vec(),
    )
}
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut result = vec![0];
    result.extend((payload.len() as u32).to_be_bytes());
    result.extend(payload);
    result
}
fn status(body: &[u8]) -> u8 {
    if body.first() == Some(&b'{') {
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        return (1..=16)
            .find(|code| {
                netget::server::connect_rpc::wire::code_name(tonic::Code::from_i32(*code))
                    == value["code"].as_str().unwrap()
            })
            .unwrap() as u8;
    }
    assert_eq!(body[0], 2);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    assert_eq!(length, body.len() - 5);
    let headers = netget::server::connect_rpc::wire::end_stream(&body[5..]).unwrap();
    tonic::Status::from_header_map(&headers).unwrap().code() as u8
}
#[tokio::test]
async fn independent_connect_node_and_fetch_binary_unary_server_streaming() {
    let state = peer::state().await;
    let (id, port) = peer::server(&state, vec![peer::handler()], json!({}))
        .await
        .unwrap();
    for (scenario, fetch) in [("normal", false), ("gzip", false), ("normal", true)] {
        let result = peer::client(port, scenario, fetch).await.unwrap();
        assert_eq!(result["echo"], 5);
        assert_eq!(result["watch"], 3);
    }
    assert_eq!(
        peer::client(port, "status", false).await.unwrap()["code"],
        7
    );
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(id.as_u32())),
            None,
        )
        .await;
    assert!(logs.iter().any(|log| log.event_type == "grpc_stream_opened"
        && log.request["message"]["counts"]["first"] == 2));
    assert!(logs.iter().any(|log| log.event_type == "grpc_stream_opened"
        && log.request["metadata"]["x-request-note"] == json!(["request:values"])));
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn response_headers_seal_before_later_tick_but_error_metadata_survives() {
    let state = peer::state().await;
    let handlers = vec![
        json!({"event_pattern":"grpc_stream_opened","handler":{"type":"static","actions":[
            {"type":"connect_rpc_metadata","phase":"headers","metadata":{"x-leading":"before"}},
            {"type":"connect_rpc_metadata","phase":"trailers","metadata":{"x-note":"after"}},
            {"type":"grpc_stream_send","message":{"name":"first"}},
            {"type":"grpc_stream_wait","milliseconds":10}]}}),
        json!({"event_pattern":"grpc_stream_tick","handler":{"type":"static","actions":[
            {"type":"connect_rpc_metadata","phase":"headers","metadata":{"x-leading":"late"}},
            {"type":"grpc_stream_finish"}]}}),
    ];
    let (id, port) = peer::server(&state, handlers, json!({})).await.unwrap();
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/streams.Session/Watch"))
        .header("content-type", "application/connect+proto")
        .header("connect-protocol-version", "1")
        .body(frame(&[]))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-leading"], "before");
    let body = response.bytes().await.unwrap();
    let first_len = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    let end = &body[5 + first_len..];
    assert_eq!(status(end), 9);
    let trailers = netget::server::connect_rpc::wire::end_stream(&end[5..]).unwrap();
    assert_eq!(trailers["x-note"], "after");
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn explicit_subset_version_errors_and_unanswered_handler() {
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})],
        json!({}),
    )
    .await
    .unwrap();
    for ct in [
        "application/json",
        "application/connect+json",
        "application/grpc-web+proto",
        "application/grpc",
    ] {
        assert_eq!(
            raw(port, "POST", vec![("content-type", ct)], vec![])
                .await
                .0,
            415
        );
    }
    assert_eq!(
        raw(
            port,
            "GET",
            vec![("content-type", "application/proto")],
            vec![]
        )
        .await
        .0,
        405
    );
    assert_eq!(
        raw(
            port,
            "POST",
            vec![
                ("content-type", "application/proto"),
                ("origin", "https://app.example")
            ],
            vec![]
        )
        .await
        .0,
        403
    );
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/streams.Session/Echo"))
        .header("content-type", "application/proto")
        .body(Vec::new())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let (code, body) = raw(
        port,
        "POST",
        vec![("content-type", "application/proto")],
        vec![],
    )
    .await;
    assert_eq!(code, 500);
    assert_eq!(status(&body), 13);
    let (code, body) = raw(
        port,
        "POST",
        vec![("content-type", "application/connect+proto")],
        frame(&[]),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(status(&body), 13);
    for method in ["Collect", "Chat", "ServerReflectionInfo"] {
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/streams.Session/{method}"))
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .body(Vec::new())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 501);
        assert_eq!(status(&response.bytes().await.unwrap()), 12);
    }
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn incoming_request_limits_plain_and_gzip_independent_peer() {
    let state = peer::state().await;
    let (id, port) = peer::server(&state, vec![peer::handler()], json!({}))
        .await
        .unwrap();
    for scenario in ["request-bounds", "request-bounds-gzip"] {
        assert_eq!(
            peer::client(port, scenario, false).await.unwrap(),
            json!({"accepted":1,"rejected":1})
        );
    }
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn rpc_deadline_cancels_parked_handler_and_incomplete_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
        json!({"rpc_timeout_secs":1}),
    )
    .await
    .unwrap();
    for input in [vec![], vec![0, 0, 0, 0, 10]] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(format!("POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: {}\r\n\r\n",if input.len()==5 && input[4]==10 {15}else{input.len()}).as_bytes()).await.unwrap();
        socket.write_all(&input).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).await
        })
        .await;
        assert!(
            result.is_ok(),
            "RPC hard deadline must close the HTTP/1.1 owner"
        );
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn stopping_server_closes_live_peer_and_clears_manual_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
        json!({}),
    )
    .await
    .unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: 0\r\n\r\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut Vec::new()))
            .await
            .is_ok()
    );
    assert!(state.list_intercepts().await.is_empty());
}
#[tokio::test]
async fn connection_cap_refuses_257th_peer_before_rpc() {
    use tokio::io::AsyncReadExt;
    let state = peer::state().await;
    let (id, port) = peer::server(&state, vec![peer::handler()], json!({}))
        .await
        .unwrap();
    let mut peers = Vec::new();
    for _ in 0..netget::server::connect_rpc::MAX_CONNECTIONS {
        peers.push(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap(),
        );
    }
    let mut extra = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), extra.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with(b"HTTP/1.1 503"));
    drop(peers);
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn rpc_capacity_is_global_and_header_body_failures_do_not_invoke_handler() {
    use tokio::io::AsyncWriteExt;
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![json!({"event_pattern":"*","handler":{"type":"manual"}})],
        json!({}),
    )
    .await
    .unwrap();
    let mut peers = Vec::new();
    for _ in 0..64 {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: 0\r\n\r\n").await.unwrap();
        peers.push(socket);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while state.list_intercepts().await.len() != 64 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        status(
            &raw(
                port,
                "POST",
                vec![("content-type", "application/proto")],
                vec![]
            )
            .await
            .1
        ),
        14
    );
    drop(peers);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Exact-one input and unique grpc-timeout are semantic refusals before model work.
    assert_eq!(
        status(
            &raw(
                port,
                "POST",
                vec![("content-type", "application/connect+proto")],
                [frame(&[]), frame(&[])].concat()
            )
            .await
            .1
        ),
        3
    );
    assert_eq!(
        status(
            &raw(
                port,
                "POST",
                vec![
                    ("content-type", "application/proto"),
                    ("connect-timeout-ms", "1"),
                    ("connect-timeout-ms", "2")
                ],
                vec![]
            )
            .await
            .1
        ),
        3
    );
    assert_eq!(
        status(
            &raw(
                port,
                "POST",
                vec![("content-type", "application/connect+proto")],
                vec![0, 255, 255, 255, 255]
            )
            .await
            .1
        ),
        8
    );
    assert!(state.list_intercepts().await.is_empty());
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn response_backpressure_deadline_cancels_http1_owner() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id,port)=peer::server(&state,vec![json!({"event_pattern":"grpc_stream_opened","handler":{"type":"static","actions":[
        {"type":"grpc_stream_send","message":{"name":"x".repeat(4*1024*1024-5)}},{"type":"grpc_stream_finish"}]}})],json!({"rpc_timeout_secs":1})).await.unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: 0\r\n\r\n").await.unwrap();
    let mut initial = [0u8; 256];
    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut initial))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut output = Vec::new();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut output))
            .await
            .is_ok()
    );
    assert!(state.list_intercepts().await.is_empty());
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn empty_unary_responses_release_admission_on_reused_connection() {
    let state = peer::state().await;
    let (id, port) = peer::server(&state, empty_unary_handler(), json!({}))
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let mut remote = None;
    // A leaked response guard exhausts the global 64-RPC admission budget on
    // this still-live connection. Every request and protobuf reply is empty.
    for call in 0..66 {
        let response = client
            .post(format!("http://127.0.0.1:{port}/streams.Session/Echo"))
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .header("content-length", "0")
            .body(Vec::new())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "empty unary call {call}");
        if call == 0 {
            eprintln!(
                "empty unary response HTTP framing: {:?}",
                response.headers()
            );
        }
        assert!(response.bytes().await.unwrap().is_empty());
        let server = state.get_server(id).await.unwrap();
        assert_eq!(server.connections.len(), 1, "one reused HTTP/1 connection");
        let address = server.connections.values().next().unwrap().remote_addr;
        assert_eq!(*remote.get_or_insert(address), address);
    }
    state.remove_server(id).await.unwrap();
}

fn empty_unary_handler() -> Vec<serde_json::Value> {
    vec![
        json!({"event_pattern":"grpc_stream_opened","handler":{"type":"static","actions":[
        {"type":"grpc_stream_send","message":{}},{"type":"grpc_stream_finish"}]}}),
    ]
}

#[tokio::test]
async fn first_byte_and_idle_connection_deadlines_close_silent_peers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![
            json!({"event_pattern":"grpc_stream_opened","handler":{"type":"static","actions":[
            {"type":"grpc_stream_send","message":{"name":"idle","value":1}},
            {"type":"grpc_stream_finish"}]}}),
        ],
        json!({}),
    )
    .await
    .unwrap();
    let mut silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let first = tokio::time::timeout(
        netget::server::connect_rpc::FIRST_BYTE_TIMEOUT + Duration::from_secs(3),
        silent.read(&mut [0u8; 1]),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first, 0);
    let (empty_id, empty_port) = peer::server(&state, empty_unary_handler(), json!({}))
        .await
        .unwrap();
    // The shared watcher polls every idle/20 (six seconds here). Completion of
    // this real unary response touches activity after the first tick was armed;
    // cover one poll interval without weakening the assertion of the idle bound.
    let poll_slack = (netget::server::connect_rpc::IDLE_TIMEOUT / 20)
        .clamp(Duration::from_millis(250), Duration::from_secs(15));
    let check_idle = |port, kind| async move {
        let mut idle = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let start = std::time::Instant::now();
        idle.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        let mut result = Vec::new();
        tokio::time::timeout(
            netget::server::connect_rpc::IDLE_TIMEOUT + poll_slack + Duration::from_secs(3),
            (&mut idle).take(32769).read_to_end(&mut result),
        )
        .await
        .unwrap()
        .unwrap();
        eprintln!(
            "{kind} unary connection closed after {:?}; HTTP response: {}",
            start.elapsed(),
            String::from_utf8_lossy(&result)
        );
        assert!(result.len() <= 32768);
        assert!(result.starts_with(b"HTTP/1.1 200"));
        assert!(
            start.elapsed() >= netget::server::connect_rpc::IDLE_TIMEOUT,
            "accepted {kind} unary response must survive until the actual idle bound"
        );
    };
    tokio::join!(
        check_idle(port, "nonempty"),
        check_idle(empty_port, "empty")
    );
    state.remove_server(empty_id).await.unwrap();
    state.remove_server(id).await.unwrap();
}

#[tokio::test]
async fn over_cap_upload_closes_without_waiting_for_declared_eof() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![peer::handler()],
        json!({"rpc_timeout_secs":30}),
    )
    .await
    .unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: 1000000000\r\n\r\n").await.unwrap();
    let start = std::time::Instant::now();
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        socket.write_all(&vec![0; 4 * 1024 * 1024 + 7]),
    )
    .await
    .unwrap();
    // Neither EOF nor the declared remaining billion-byte body is supplied. A reset is
    // also a valid close: the request exceeded the bound before its HTTP input ended.
    let result =
        tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut Vec::new())).await;
    assert!(
        result.is_ok(),
        "oversized input must close without an unbounded drain"
    );
    assert!(start.elapsed() < Duration::from_secs(6));
    assert!(state.list_intercepts().await.is_empty());
    state.remove_server(id).await.unwrap();
}

#[tokio::test]
async fn header_count_and_buffer_bounds_are_enforced_before_model_work() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id, port) = peer::server(&state, vec![peer::handler()], json!({}))
        .await
        .unwrap();
    for (extra, padding, expected) in [(60, 0, 500), (61, 0, 431), (0, 32768, 431)] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let mut headers="POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/proto\r\nConnect-Protocol-Version: 1\r\nContent-Length: 0\r\n".to_owned();
        for index in 0..extra {
            let _ = index;
            headers.push_str("User-Agent: a\r\n");
        }
        if padding > 0 {
            headers.push_str(&format!("x-pad: {}\r\n", "a".repeat(padding)));
        }
        headers.push_str("\r\n");
        socket.write_all(headers.as_bytes()).await.unwrap();

        let mut reply = [0u8; 128];
        let count = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert!(
            reply[..count].starts_with(format!("HTTP/1.1 {expected}").as_bytes()),
            "extra={extra} padding={padding}: {}",
            String::from_utf8_lossy(&reply[..count])
        );
    }
    state.remove_server(id).await.unwrap();
}
