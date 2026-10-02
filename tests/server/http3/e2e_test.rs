use crate::helpers::quic_peer::*;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
use std::time::Duration;
fn echo() -> serde_json::Value {
    json!({"event_pattern":"http3_request_received","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'send_http3_response','status':201,'headers':{'x-peer':'netget','content-type':'application/json'},'body':json.dumps(e),'trailers':{'x-response-tail':'yes'}}]}))"}})
}
#[tokio::test]
async fn http3_server_independent_aioquic_client_and_netget_pair() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(&state, "http3", &cert, json!({}), vec![echo()]).await;
    let results=external_client("http3",addr.port(),&cert,json!([{"method":"GET","path":"/hello?q=1"},{"method":"POST","path":"/post","headers":{"x-test":"peer","te":"trailers"},"body":"body"}])).await;
    assert_eq!(results.as_array().unwrap().len(), 2);
    for result in results.as_array().unwrap() {
        let headers = result["headers"].as_array().unwrap();
        assert!(headers.contains(&json!([":status", "201"])));
        assert!(headers.contains(&json!(["x-response-tail", "yes"])));
    }
    let request: serde_json::Value =
        serde_json::from_str(results[1]["body"].as_str().unwrap()).unwrap();
    assert_eq!(request["method"], "POST");
    assert_eq!(request["path"], "/post");
    assert_eq!(request["body"], "body");
    assert_eq!(request["headers"]["te"], "trailers");
    let client = client(
        &state,
        "http3",
        addr.to_string(),
        cert.trust(),
        vec![empty_handler()],
    )
    .await;
    let response=state.send_to_client(client,json!({"type":"send_http3_request","method":"POST","path":"/pair","body":"paired","trailers":{"x-request-tail":"pair"}}),Duration::from_secs(6)).await.unwrap();
    assert!(matches!(response,ClientSendOutcome::Executed{detail} if detail.contains("-> 201")));
    wait_log(&state, client, "x-response-tail").await;
    state.remove_client(client).await;
    state.remove_server(id).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(socket) = std::net::UdpSocket::bind(addr) {
                drop(socket);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn http3_invalid_incoming_te_resets_only_its_request() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(&state, "http3", &cert, json!({}), vec![echo()]).await;
    let params = netget::protocol::StartupParams::new(
        cert.trust(),
        netget::utils::quic::client_parameters(),
    )
    .unwrap();
    let (_endpoint, connection) =
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
            .await
            .unwrap();
    let (_driver, mut sender) = h3::client::builder()
        .send_grease(false)
        .build::<_, _, bytes::Bytes>(h3_quinn::Connection::new(connection.0.clone()))
        .await
        .unwrap();
    for trailers in [false, true] {
        let mut request = http::Request::builder()
            .uri("https://localhost/invalid-te")
            .body(())
            .unwrap();
        if !trailers {
            request
                .headers_mut()
                .insert("te", http::HeaderValue::from_static("gzip"));
        }
        let mut stream = sender.send_request(request).await.unwrap();
        if trailers {
            let mut fields = http::HeaderMap::new();
            fields.insert("te", http::HeaderValue::from_static("trailers"));
            stream.send_trailers(fields).await.unwrap();
        }
        stream.finish().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(2), stream.recv_response())
                .await
                .unwrap()
                .is_err()
        );
    }
    assert!(
        state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Server(id.as_u32())),
                None
            )
            .await
            .is_empty(),
        "invalid TE must not reach a handler"
    );
    let request = http::Request::builder()
        .uri("https://localhost/valid-te")
        .header("te", "Trailers")
        .body(())
        .unwrap();
    let mut stream = sender.send_request(request).await.unwrap();
    stream.finish().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), stream.recv_response())
            .await
            .unwrap()
            .unwrap()
            .status(),
        201
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn http3_server_rejects_oversized_decoded_request_fields() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(&state, "http3", &cert, json!({}), vec![echo()]).await;
    // Static QPACK references are compact on the wire but expand beyond the
    // 32 KiB decoded field-section limit. aioquic owns the actual encoding.
    let result = external_client(
        "http3",
        addr.port(),
        &cert,
        json!([{"path":"/headers-bound","headers":{"accept":vec!["*/*";900]}}]),
    )
    .await;
    assert!(result[0]["headers"]
        .as_array()
        .unwrap()
        .contains(&json!([":status", "431"])));
    assert!(result[0]["body"].as_str().unwrap().is_empty());
    assert!(
        state
            .list_access_logs_for(
                Some(netget::state::AccessLogOwner::Server(id.as_u32())),
                None
            )
            .await
            .is_empty(),
        "oversized fields must not reach a handler"
    );
    state.remove_server(id).await;
}
#[tokio::test]
async fn http3_server_timeout_and_removal_cancel_partial_body() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(
        &state,
        "http3",
        &cert,
        json!({"exchange_timeout_secs":1}),
        vec![empty_handler()],
    )
    .await;
    let params = netget::protocol::StartupParams::new(
        cert.trust(),
        netget::utils::quic::client_parameters(),
    )
    .unwrap();
    let (_ep, conn) = netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
        .await
        .unwrap();
    let (_driver, mut sender) = h3::client::builder()
        .build::<_, _, bytes::Bytes>(h3_quinn::Connection::new(conn.0.clone()))
        .await
        .unwrap();
    let mut request = sender
        .send_request(
            http::Request::builder()
                .method("POST")
                .uri("https://localhost/partial")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    request
        .send_data(bytes::Bytes::from_static(b"unfinished"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), request.recv_response())
            .await
            .unwrap()
            .is_err()
    );
    let mut request = sender
        .send_request(
            http::Request::builder()
                .uri("https://localhost/removal")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    request
        .send_data(bytes::Bytes::from_static(b"unfinished"))
        .await
        .unwrap();
    state.remove_server(id).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(3), request.recv_response())
            .await
            .unwrap()
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(socket) = std::net::UdpSocket::bind(addr) {
                drop(socket);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn http3_server_connection_idle_limits_and_alpn() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(
        &state,
        "http3",
        &cert,
        json!({"max_connections":1,"idle_timeout_secs":1}),
        vec![empty_handler()],
    )
    .await;
    let params = netget::protocol::StartupParams::new(
        cert.trust(),
        netget::utils::quic::client_parameters(),
    )
    .unwrap();
    let (_ep, conn) = netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
        .await
        .unwrap();
    assert!(
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(3), conn.0.closed())
        .await
        .unwrap();
    assert!(
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"netget-quic", 0)
            .await
            .is_err()
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn http3_server_request_stream_and_body_bounds() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(
        &state,
        "http3",
        &cert,
        json!({"max_streams":2,"exchange_timeout_secs":3}),
        vec![empty_handler()],
    )
    .await;
    let params = netget::protocol::StartupParams::new(
        cert.trust(),
        netget::utils::quic::client_parameters(),
    )
    .unwrap();
    let (_ep, conn) = netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
        .await
        .unwrap();
    let (_driver, mut sender) = h3::client::builder()
        .send_grease(false)
        .build::<_, _, bytes::Bytes>(h3_quinn::Connection::new(conn.0.clone()))
        .await
        .unwrap();
    let mut first = sender
        .send_request(
            http::Request::builder()
                .method("POST")
                .uri("https://localhost/first")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    first
        .send_data(bytes::Bytes::from_static(b"unfinished"))
        .await
        .unwrap();
    let mut second = sender
        .send_request(
            http::Request::builder()
                .uri("https://localhost/second")
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    second
        .send_data(bytes::Bytes::from_static(b"unfinished"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(200),
            sender.send_request(
                http::Request::builder()
                    .uri("https://localhost/third")
                    .body(())
                    .unwrap()
            )
        )
        .await
        .is_err(),
        "third stream must wait for credit"
    );
    first.stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
    first.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
    drop(first);
    let mut third = tokio::time::timeout(
        Duration::from_secs(2),
        sender.send_request(
            http::Request::builder()
                .method("POST")
                .uri("https://localhost/body-bound")
                .body(())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let send = third
        .send_data(bytes::Bytes::from(vec![b'x'; 8 * 1024 * 1024 + 1]))
        .await;
    if send.is_ok() {
        let _ = third.finish().await;
    }
    let error = tokio::time::timeout(Duration::from_secs(4), third.recv_response())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, h3::error::StreamError::RemoteTerminate { .. }),
        "must reset oversized body: {error}"
    );
    state.remove_server(id).await;
}

#[test]
fn http3_response_semantic_bounds() {
    use netget::llm::actions::protocol_trait::Server;
    let protocol = netget::server::http3::actions::Http3Protocol::new();
    // x-large overhead is 39 bytes, and :status is 42 bytes. The full decoded
    // section may fit the advertised bound exactly, but must not exceed it.
    assert!(protocol.execute_action(json!({"type":"send_http3_response","status":200,"headers":{"x-large":"x".repeat(32768-39-42)}})).is_ok());
    for action in [
        json!({"type":"send_http3_response","status":100}),
        json!({"type":"send_http3_response","status":204,"body":"forbidden"}),
        json!({"type":"send_http3_response","status":205,"body":"forbidden"}),
        json!({"type":"send_http3_response","status":200,"body":"x".repeat(8*1024*1024+1)}),
        json!({"type":"send_http3_response","status":200,"headers":{"x-large":"x".repeat(32769)}}),
        // The regular fields fit exactly; :status must count toward the limit.
        json!({"type":"send_http3_response","status":200,"headers":{"x-large":"x".repeat(32768-39)}}),
        json!({"type":"send_http3_response","status":200,"headers":{"x-large":"x".repeat(32768-39-42+1)}}),
        json!({"type":"send_http3_response","status":200,"trailers":{"x-large":"x".repeat(32769)}}),
        json!({"type":"send_http3_response","status":200,"headers":{"connection":"close"}}),
        json!({"type":"send_http3_response","status":200,"headers":{"te":"trailers"}}),
        json!({"type":"send_http3_response","status":200,"trailers":{"te":"trailers"}}),
    ] {
        assert!(protocol.execute_action(action).is_err());
    }
}

#[tokio::test]
async fn http3_server_handshake_deadline_returns_connection_slot() {
    let state = state();
    let cert = Certificate::new();
    let (id, addr) = server(
        &state,
        "http3",
        &cert,
        json!({"max_connections":1,"handshake_timeout_secs":1,"idle_timeout_secs":10}),
        vec![empty_handler()],
    )
    .await;
    let params = netget::protocol::StartupParams::new(
        cert.trust(),
        netget::utils::quic::client_parameters(),
    )
    .unwrap();
    let relay = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let roots = {
        let mut roots = rustls::RootCertStore::empty();
        let pem = std::fs::read(cert.cert()).unwrap();
        for c in rustls_pemfile::certs(&mut pem.as_slice()) {
            roots.add(c.unwrap()).unwrap();
        }
        roots
    };
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
    )));
    let _stalled = endpoint
        .connect(relay.local_addr().unwrap(), "localhost")
        .unwrap();
    let mut packet = vec![0; 65535];
    let (count, _) = tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut packet))
        .await
        .unwrap()
        .unwrap();
    relay.send_to(&packet[..count], addr).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), relay.recv_from(&mut packet))
        .await
        .unwrap()
        .unwrap();
    assert!(
        netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (_ep, conn) = netget::utils::quic::connect(&addr.to_string(), Some(&params), b"h3", 4)
        .await
        .unwrap();
    conn.0.close(0u32.into(), b"done");
    state.remove_server(id).await;
}
