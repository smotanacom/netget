use crate::helpers::prometheus_remote_write::{batch, logs, server};
use netget::{
    server::prometheus_remote_write::codec::{self, WriteBatch},
    state::AccessLogOwner,
};
use serde_json::json;
use std::time::Duration;
fn body() -> Vec<u8> {
    codec::encode_batch(&serde_json::from_value::<WriteBatch>(batch()).unwrap()).unwrap()
}
async fn post(
    addr: std::net::SocketAddr,
    path: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut header_map = reqwest::header::HeaderMap::new();
    for (name, value) in [
        ("Content-Type", "application/x-protobuf"),
        ("Content-Encoding", "snappy"),
        ("User-Agent", "independent-fixture/1"),
        ("X-Prometheus-Remote-Write-Version", "0.1.0"),
    ] {
        header_map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            reqwest::header::HeaderValue::from_str(value).unwrap(),
        );
    }
    for (name, value) in headers {
        header_map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            reqwest::header::HeaderValue::from_str(value).unwrap(),
        );
    }
    let request = client
        .post(format!("http://{addr}{path}"))
        .headers(header_map)
        .body(body);
    request.send().await.unwrap()
}
#[tokio::test]
async fn default_collects_typed_float_stale_unicode_without_model_or_store() {
    let (state, id, addr, mut rx) = server(None, None).await;
    let response = post(addr, "/api/v1/write", body(), &[]).await;
    assert_eq!(response.status(), 204);
    assert_eq!(
        response.headers()["X-Prometheus-Remote-Write-Version"],
        "0.1.0"
    );
    assert!(response.bytes().await.unwrap().is_empty());
    let rows = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "remote_write_request",
        1,
    )
    .await;
    assert_eq!(rows[0].request["series"], batch()["series"]);
    assert_eq!(rows[0].request["durable_storage"], false);
    assert_eq!(rows[0].request["sample_count"], 2);
    let text = serde_json::to_string(&rows[0].request).unwrap();
    assert!(!text.contains("raw"));
    assert!(!text.contains("header_data"));
    let mut statuses = String::new();
    while let Ok(line) = rx.try_recv() {
        statuses.push_str(&line);
    }
    assert!(statuses.contains("decision=accept"));
    state.remove_server(id).await;
}
#[tokio::test]
async fn version_route_method_auth_mime_and_bad_compression_fail_closed_then_recover() {
    let (state, id, addr, _) =
        server(None, Some(json!({"auth_token":"secret","path":"/receive"}))).await;
    let auth = [("Authorization", "bEaReR secret")];
    assert_eq!(
        post(addr, "/api/v1/write", body(), &auth).await.status(),
        404
    );
    assert_eq!(
        post(addr, "/receive?extra=x", body(), &auth).await.status(),
        400
    );
    assert_eq!(post(addr, "/receive", body(), &[]).await.status(), 401);
    assert_eq!(
        post(
            addr,
            "/receive",
            body(),
            &[("Authorization", "Bearer wrong")]
        )
        .await
        .status(),
        401
    );
    for header in [
        (
            "Content-Type",
            "application/x-protobuf; proto=io.prometheus.write.v2.Request",
        ),
        ("X-Prometheus-Remote-Write-Version", "2.0.0"),
        ("Content-Encoding", "gzip"),
    ] {
        let mut headers = auth.to_vec();
        headers.push(header);
        assert_eq!(post(addr, "/receive", body(), &headers).await.status(), 415);
    }
    assert_eq!(
        post(addr, "/receive", vec![0xff], &auth).await.status(),
        400
    );
    assert_eq!(
        post(addr, "/receive", vec![0; codec::MAX_BODY_BYTES + 1], &auth)
            .await
            .status(),
        413
    );
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/receive"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 405);
    assert_eq!(response.headers()["allow"], "POST");
    let response = post(
        addr,
        "/receive",
        body(),
        &[
            ("Authorization", "bEaReR secret"),
            (
                "Content-Type",
                "Application/X-Protobuf; PROTO=\"prometheus.WriteRequest\"",
            ),
        ],
    )
    .await;
    assert_eq!(response.status(), 204);
    let rows = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "remote_write_request",
        1,
    )
    .await;
    assert_eq!(rows.len(), 1);
    assert!(!serde_json::to_string(&rows).unwrap().contains("secret"));
    state.remove_server(id).await;
}
#[tokio::test]
async fn successful_empty_common_and_explicit_accept_reject_outcomes() {
    for actions in [
        vec![],
        vec![json!({"type":"set_memory","value":"seen"})],
        vec![json!({"type":"accept_remote_write_samples"})],
    ] {
        let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"remote_write_request","handler":{"type":"static","actions":actions}})]),None).await;
        assert_eq!(post(addr, "/api/v1/write", body(), &[]).await.status(), 204);
        state.remove_server(id).await;
    }
    for status in [400, 429, 500, 503] {
        let action = json!({"type":"reject_remote_write_samples","status":status,"message":"fixture rejection"});
        let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"remote_write_request","handler":{"type":"static","actions":[action]}})]),None).await;
        let response = post(addr, "/api/v1/write", body(), &[]).await;
        assert_eq!(response.status().as_u16(), status);
        assert_eq!(response.text().await.unwrap(), "fixture rejection");
        state.remove_server(id).await;
    }
}
#[tokio::test]
async fn failed_action_and_backend_error_override_valid_accept_without_rollback_claim() {
    let code="import json,sys\njson.load(sys.stdin)\nprint(json.dumps({'actions':[{'type':'set_memory','value':'seen'},{'type':'accept_remote_write_samples'},{'type':'unknown_failed_action'}]}))";
    let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"remote_write_request","handler":{"type":"script","language":"python","code":code}})]),None).await;
    assert_eq!(post(addr, "/api/v1/write", body(), &[]).await.status(), 503);
    assert_eq!(state.get_memory(id).await.unwrap(), "seen");
    let rows = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "remote_write_handler_failed",
        1,
    )
    .await;
    assert_eq!(
        rows[0].response[0]["decision"],
        "fail_closed_handler_action_error"
    );
    state.remove_server(id).await;
    let (state, id, addr, _) = server(None, Some(json!({"llm_fallback":true}))).await;
    state
        .set_ollama_model(Some("fixture-no-discovery".into()))
        .await;
    assert_eq!(post(addr, "/api/v1/write", body(), &[]).await.status(), 503);
    state.remove_server(id).await;
}
#[tokio::test]
async fn parked_handler_server_removal_releases_owned_socket_and_intercept() {
    let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"remote_write_request","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    let request = tokio::spawn(post(addr, "/api/v1/write", body(), &[]));
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    drop(listener);
    // The reqwest fixture owns its task too; cancellation can close without an HTTP reply.
    request.abort();
    let _ = request.await;
}
#[tokio::test]
async fn header_and_body_deadlines_close_stalled_peers_but_preserve_parked_handlers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let(state,id,addr,_)=server(Some(vec![json!({"event_pattern":"remote_write_request","handler":{"type":"manual","timeout_secs":300}})]),None).await;
    let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut partial_header = tokio::net::TcpStream::connect(addr).await.unwrap();
    partial_header
        .write_all(b"POST /api/v1/write HTTP/1.1\r\n")
        .await
        .unwrap();
    let mut partial_body = tokio::net::TcpStream::connect(addr).await.unwrap();
    partial_body
        .write_all(b"POST /api/v1/write HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10\r\n\r\n")
        .await
        .unwrap();
    let parked = tokio::spawn(post(addr, "/api/v1/write", body(), &[]));
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.list_intercepts().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let began = std::time::Instant::now();
    let read = |mut stream: tokio::net::TcpStream| async move {
        let mut bytes = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(45), stream.read_to_end(&mut bytes))
            .await
            .expect("declared30s deadline must close stalled peer");
        bytes
    };
    let (_, _, body_response) =
        tokio::join!(read(silent), read(partial_header), read(partial_body));
    assert!(began.elapsed() >= Duration::from_secs(28));
    assert!(String::from_utf8_lossy(&body_response).starts_with("HTTP/1.1 408"));
    assert!(!parked.is_finished());
    assert_eq!(state.list_intercepts().await.len(), 1);
    state.remove_server(id).await;
    parked.abort();
    let _ = parked.await;
}
#[tokio::test]
async fn live_connection_and_header_caps_refuse_then_recover() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (state, id, addr, _) = server(None, None).await;
    let mut held = vec![];
    for _ in 0..netget::server::accept_bounded::DEFAULT_MAX_CONNECTIONS {
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket
            .write_all(b"POST /api/v1/write HTTP/1.1\r\n")
            .await
            .unwrap();
        held.push(socket);
    }
    let mut extra = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut bytes = vec![];
    tokio::time::timeout(Duration::from_secs(10), extra.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 503"));
    drop(held);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if post(addr, "/api/v1/write", body(), &[]).await.status() == 204 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    for header in [
        format!(
            "{}",
            (0..65)
                .map(|n| format!("X-{n}: value\r\n"))
                .collect::<String>()
        ),
        format!(
            "X-Large: {}\r\n",
            "x".repeat(netget::server::prometheus_remote_write::MAX_HEADER_BYTES + 1)
        ),
    ] {
        let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
        socket.write_all(format!("POST /api/v1/write HTTP/1.1\r\nHost: localhost\r\n{header}Content-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
        let mut bytes = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut bytes))
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 2"));
    }
    assert_eq!(post(addr, "/api/v1/write", body(), &[]).await.status(), 204);
    state.remove_server(id).await;
}
