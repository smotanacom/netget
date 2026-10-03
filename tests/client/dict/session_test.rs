use super::common::*;
use netget::cli::management::{ClientForm, ServerForm};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
#[tokio::test]
async fn netget_pair_definitions_matches_discovery_and_quit() {
    let state = state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let sid=ServerForm{protocol:"dict".into(),port:Some(0),host:Some("127.0.0.1".into()),event_handlers:Some(vec![
        json!({"event_pattern":"dict_define","handler":{"type":"static","actions":[{"type":"send_dict_definitions","word":"hello","definitions":[{"database":"test","database_description":"Test dictionary","text":".dot\nDefinition ✓"}]}]}}),
        json!({"event_pattern":"dict_match","handler":{"type":"static","actions":[{"type":"send_dict_matches","matches":[{"database":"test","word":"hello"}]}]}}),
        json!({"event_pattern":"dict_show","handler":{"type":"static","actions":[{"type":"send_dict_databases","databases":[{"name":"test","description":"Test dictionary"}]}]}}),
    ]),..Default::default()}.create(&state,tx).await.unwrap();
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
    let id = client(&state, addr.to_string(), json!([])).await;
    assert_eq!(
        request(&state, id, json!({"operation":"define","word":"hello"})).await["definitions"][0]
            ["text"],
        ".dot\nDefinition ✓"
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"match","word":"hel","strategy":"prefix"})
        )
        .await["entries"][0]["word"],
        "hello"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"databases"})).await["entries"][0]["name"],
        "test"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"quit"})).await["code"],
        221
    );
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn pending_reply_rejects_request_and_disconnect_interrupts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        r.get_mut()
            .write_all(b"220 Fixture <test@localhost>\r\n")
            .await
            .unwrap();
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "SHOW DB\r\n");
        tx.send(()).unwrap();
        line.clear();
        assert_eq!(r.read_line(&mut line).await.unwrap(), 0);
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!([{"type":"dict_request","operation":"databases"}]),
    )
    .await;
    rx.await.unwrap();
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"dict_request","operation":"status"}),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        rejected,
        netget::state::client_handles::ClientSendOutcome::Rejected { .. }
    ));
    let disconnected = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(
        disconnected,
        netget::state::client_handles::ClientSendOutcome::Disconnected
    ));
    tokio::time::timeout(Duration::from_secs(1), fixture)
        .await
        .unwrap()
        .unwrap();
    state.remove_client(id).await;
}
#[tokio::test]
async fn removal_during_stalled_greeting_closes_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        tx.send(()).unwrap();
        let mut line = String::new();
        assert_eq!(r.read_line(&mut line).await.unwrap(), 0);
    });
    let state = state();
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "dict".into(),
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
async fn fragmented_final_reply_delivered_before_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        r.get_mut()
            .write_all(b"220 Fixture <test@localhost>\r\n")
            .await
            .unwrap();
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        for chunk in ["114 server", " info\r\n..dot\r\n.", "\r\n250 ok\r\n"] {
            r.get_mut().write_all(chunk.as_bytes()).await.unwrap();
        }
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!([{"type":"dict_request","operation":"server"}]),
    )
    .await;
    assert_eq!(response(&state, id, 0).await["text"], ".dot");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
