use super::common::*;
use netget::cli::management::{ClientForm, ServerForm};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
#[tokio::test]
async fn netget_pair_fresh_connections_input_encoding_and_endpoint_rejection() {
    let state = state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let certs = tempfile::tempdir().unwrap();
    let (pem, _, _, key) = certificate();
    let cert = certs.path().join("cert.pem");
    let keyfile = certs.path().join("key.pem");
    std::fs::write(&cert, &pem).unwrap();
    std::fs::write(&keyfile, key).unwrap();
    let (tx, _) = mpsc::unbounded_channel();
    let sid=ServerForm{protocol:"gemini".into(),host:Some("127.0.0.1".into()),port:Some(0),startup_params:Some(json!({"cert_path":cert,"key_path":keyfile})),event_handlers:Some(vec![json!({"event_pattern":"gemini_request","handler":{"type":"static","actions":[{"type":"send_gemtext","lines":[{"type":"heading1","text":"NetGet capsule"},{"type":"link","url":"/next","text":"Next"},{"type":"preformatted","alt":"code","text":"=> literal"}]}]}})]),..Default::default()}.create(&state,tx).await.unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(addr) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let id = client(&state, addr.to_string(), &pem, json!([])).await;
    for _ in 0..2 {
        let response = request(
            &state,
            id,
            json!({"url":format!("gemini://localhost:{}/",addr.port()),"input":"café + &%"}),
        )
        .await;
        assert_eq!(response["lines"][0]["text"], "NetGet capsule");
        assert_eq!(response["lines"][2]["text"], "=> literal");
    }
    let entries = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(sid.as_u32())),
            None,
        )
        .await;
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.event_type == "gemini_request" && e.request["query"] == "café + &%")
            .count(),
        2
    );
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"gemini_request","url":"gemini://wrong.example/"}),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        rejected,
        netget::state::client_handles::ClientSendOutcome::Rejected { .. }
    ));
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn disconnect_interrupts_stalled_text_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (pem, cert, key, _) = certificate();
    let acceptor = acceptor(cert, key);
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(s).await.unwrap();
        let mut r = BufReader::new(tls);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        r.get_mut()
            .write_all(b"20 text/plain\r\nnever completes")
            .await
            .unwrap();
        r.get_mut().flush().await.unwrap();
        tx.send(()).unwrap();
        let mut rest = Vec::new();
        let _ = r.read_to_end(&mut rest).await;
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        &pem,
        json!([{"type":"gemini_request","url":format!("gemini://localhost:{}/",addr.port())}]),
    )
    .await;
    rx.await.unwrap();
    let result = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(
        result,
        netget::state::client_handles::ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn removal_interrupts_stalled_tls_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        tx.send(()).unwrap();
        let mut received = Vec::new();
        s.read_to_end(&mut received).await.unwrap();
    });
    let state = state();
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "gemini".into(),
        remote_addr: Some(addr.to_string()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    rx.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    state.remove_client(id).await;
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn fragmented_tls_response_reaches_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (pem, cert, key, _) = certificate();
    let acceptor = acceptor(cert, key);
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(s).await.unwrap();
        let mut r = BufReader::new(tls);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        for data in ["2", "0 text/plain\r", "\nComplete ✓"] {
            r.get_mut().write_all(data.as_bytes()).await.unwrap();
        }
        r.get_mut().shutdown().await.unwrap();
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        &pem,
        json!([{"type":"gemini_request","url":format!("gemini://localhost:{}/",addr.port())}]),
    )
    .await;
    assert_eq!(response(&state, id, 0).await["text"], "Complete ✓");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
