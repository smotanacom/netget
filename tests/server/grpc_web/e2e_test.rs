use crate::helpers::grpcweb_peer as peer;
use serde_json::json;
use std::time::Duration;
async fn raw(port: u16, method: &str, headers: Vec<(&str, &str)>, body: Vec<u8>) -> (u16, Vec<u8>) {
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut request = client.request(
        method.parse().unwrap(),
        format!("http://127.0.0.1:{port}/streams.Session/Echo"),
    );
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
    assert_eq!(body[0], 128);
    let length = u32::from_be_bytes(body[1..5].try_into().unwrap()) as usize;
    assert_eq!(length, body.len() - 5);
    netget::server::grpc_web::wire::status(
        &netget::server::grpc_web::wire::trailers(&body[5..]).unwrap(),
    )
    .unwrap()
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
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn explicit_subset_cors_and_fail_closed_wire_status() {
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})],
        json!({"allow_origin":"https://app.example"}),
    )
    .await
    .unwrap();
    for ct in [
        "application/grpc-web-text",
        "application/grpc-web-text+proto",
        "application/grpc",
        "application/grpc-web+json",
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
            "POST",
            vec![
                ("content-type", "application/grpc-web"),
                ("accept", "application/grpc-web-text+proto")
            ],
            vec![]
        )
        .await
        .0,
        415
    );
    assert_eq!(
        raw(
            port,
            "GET",
            vec![("content-type", "application/grpc-web")],
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
                ("content-type", "application/grpc-web"),
                ("origin", "https://other.example")
            ],
            vec![]
        )
        .await
        .0,
        403
    );
    let response = reqwest::Client::new()
        .request(
            reqwest::Method::OPTIONS,
            format!("http://127.0.0.1:{port}/streams.Session/Echo"),
        )
        .header("origin", "https://app.example")
        .header("access-control-request-method", "POST")
        .header(
            "access-control-request-headers",
            "content-type,x-grpc-web,grpc-timeout",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://app.example"
    );
    assert!(!response
        .headers()
        .contains_key("access-control-allow-credentials"));
    let (code, body) = raw(
        port,
        "POST",
        vec![
            ("content-type", "application/grpc-web+proto"),
            ("origin", "https://app.example"),
        ],
        frame(&[]),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(status(&body), 13);
    for method in ["Collect", "Chat", "ServerReflectionInfo"] {
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/streams.Session/{method}"))
            .header("content-type", "application/grpc-web+proto")
            .body(frame(&[]))
            .send()
            .await
            .unwrap();
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
    for input in [frame(&[]), vec![0, 0, 0, 0, 10]] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(format!("POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web+proto\r\nContent-Length: {}\r\n\r\n",if input.len()==5 && input[4]==10 {15}else{input.len()}).as_bytes()).await.unwrap();
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
    socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0").await.unwrap();
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
    for _ in 0..netget::server::grpc_web::MAX_CONNECTIONS {
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
        socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0").await.unwrap();
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
                vec![("content-type", "application/grpc-web")],
                frame(&[])
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
                vec![("content-type", "application/grpc-web")],
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
                    ("content-type", "application/grpc-web"),
                    ("grpc-timeout", "1S"),
                    ("grpc-timeout", "2S")
                ],
                frame(&[])
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
                vec![("content-type", "application/grpc-web")],
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
    socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0").await.unwrap();
    let mut initial = [0u8; 256];
    tokio::time::timeout(Duration::from_secs(2), socket.read(&mut initial))
        .await
        .unwrap()
        .unwrap();
    // Deliberately keep the response unread past the 1s RPC deadline to test output backpressure.
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
async fn first_byte_and_idle_connection_deadlines_close_silent_peers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let state = peer::state().await;
    let (id, port) = peer::server(
        &state,
        vec![peer::handler()],
        json!({"allow_origin":"https://app.example"}),
    )
    .await
    .unwrap();
    let mut silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let first = tokio::time::timeout(
        netget::server::grpc_web::FIRST_BYTE_TIMEOUT + Duration::from_secs(3),
        silent.read(&mut [0u8; 1]),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first, 0);
    let mut idle = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let start = std::time::Instant::now();
    idle.write_all(b"OPTIONS /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n")
        .await
        .unwrap();
    let mut result = Vec::new();
    tokio::time::timeout(
        netget::server::grpc_web::IDLE_TIMEOUT + Duration::from_secs(3),
        idle.read_to_end(&mut result),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.starts_with(b"HTTP/1.1 204"));
    assert!(
        start.elapsed() >= netget::server::grpc_web::IDLE_TIMEOUT,
        "valid keep-alive preflight must survive until the actual idle bound"
    );
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
    socket.write_all(b"POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web\r\nContent-Length: 1000000000\r\n\r\n").await.unwrap();
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
    for (extra, padding, expected) in [(61, 0, 200), (62, 0, 431), (0, 32768, 431)] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let mut headers="POST /streams.Session/Echo HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/grpc-web\r\nContent-Length: 5\r\n".to_owned();
        for index in 0..extra {
            headers.push_str(&format!("x-test-{index}: a\r\n"));
        }
        if padding > 0 {
            headers.push_str(&format!("x-pad: {}\r\n", "a".repeat(padding)));
        }
        headers.push_str("\r\n");
        socket.write_all(headers.as_bytes()).await.unwrap();
        let _ = socket.write_all(&[0; 5]).await;
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
