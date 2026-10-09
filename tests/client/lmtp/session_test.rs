//! The LMTP client against a scripted fixture that asserts every byte it receives: LHLO on
//! connect, the handler's message as MAIL/RCPT/DATA with dot-stuffing and generated headers,
//! one delivery reply read per accepted recipient (arriving in fragments), RSET after a
//! transaction nobody accepted, injected sends and QUIT.
use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

pub async fn start(remote: String, params: Value, connected: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let llm = netget::llm::OllamaClient::new("http://127.0.0.1:1");
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "lmtp".into(),
        remote_addr: Some(remote),
        instruction: Some("Deliver test mail".into()),
        startup_params: Some(params),
        event_handlers: Some(vec![
            json!({"event_pattern":"lmtp_connected","handler":{"type":"static","actions":connected}}),
            json!({"event_pattern":"lmtp_result","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(&state, llm, tx)
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(entry) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| e.contains(needle))
            {
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing {needle:?}"))
}

async fn read(r: &mut BufReader<TcpStream>) -> String {
    let mut s = String::new();
    tokio::time::timeout(Duration::from_secs(10), r.read_line(&mut s))
        .await
        .expect("fixture read deadline")
        .unwrap();
    s
}

#[tokio::test]
async fn transactions_injection_and_per_recipient_replies() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        r.get_mut()
            .write_all(b"220 fixture LMTP ready\r\n")
            .await
            .unwrap();
        assert_eq!(read(&mut r).await, "LHLO client.test\r\n");
        r.get_mut()
            .write_all(b"250-fixture\r\n250-PIPELINING\r\n250 SIZE 100000\r\n")
            .await
            .unwrap();
        assert_eq!(read(&mut r).await, "MAIL FROM:<s@example.test>\r\n");
        r.get_mut().write_all(b"250 2.1.0 ok\r\n").await.unwrap();
        for (rcpt, reply) in [
            ("a@example.test", "250 2.1.5 ok\r\n"),
            ("b@example.test", "550 5.1.1 no such user\r\n"),
            ("c@example.test", "250 2.1.5 ok\r\n"),
        ] {
            assert_eq!(read(&mut r).await, format!("RCPT TO:<{rcpt}>\r\n"));
            r.get_mut().write_all(reply.as_bytes()).await.unwrap();
        }
        assert_eq!(read(&mut r).await, "DATA\r\n");
        r.get_mut().write_all(b"354 go ahead\r\n").await.unwrap();
        let mut message = String::new();
        loop {
            let line = read(&mut r).await;
            if line == ".\r\n" {
                break;
            }
            message.push_str(&line);
        }
        for header in [
            "From: <s@example.test>\r\n",
            "To: <a@example.test>, <b@example.test>, <c@example.test>\r\n",
            "Subject: Quarterly\r\n",
            "X-Priority: 1\r\n",
            "Message-ID: <",
            "@client.test>\r\n",
            "Date: ",
        ] {
            assert!(
                message.contains(header),
                "{header:?} missing from {message:?}"
            );
        }
        assert!(
            message.ends_with("\r\nfirst line\r\n..leading dot\r\nlast line\r\n"),
            "{message:?}"
        );
        // Two delivery replies, one per accepted recipient, split across writes.
        for fragment in [
            "250 2.0.0 a deliv",
            "ered\r\n452 4.2.2 c ",
            "mailbox full\r\n",
        ] {
            r.get_mut().write_all(fragment.as_bytes()).await.unwrap();
            r.get_mut().flush().await.unwrap();
        }
        // An injected send whose only recipient is refused: the client resets.
        assert_eq!(read(&mut r).await, "MAIL FROM:<>\r\n");
        r.get_mut().write_all(b"250 2.1.0 ok\r\n").await.unwrap();
        assert_eq!(read(&mut r).await, "RCPT TO:<d@example.test>\r\n");
        r.get_mut()
            .write_all(b"550 5.1.1 unknown d\r\n")
            .await
            .unwrap();
        assert_eq!(read(&mut r).await, "RSET\r\n");
        r.get_mut()
            .write_all(b"250 2.0.0 flushed\r\n")
            .await
            .unwrap();
        assert_eq!(read(&mut r).await, "QUIT\r\n");
        r.get_mut().write_all(b"221 2.0.0 bye\r\n").await.unwrap();
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    });
    let (state, id) = start(
        address.to_string(),
        json!({"lhlo_domain":"client.test"}),
        json!([{"type":"lmtp_send","from":"s@example.test",
            "to":["a@example.test","b@example.test","c@example.test"],
            "subject":"Quarterly","headers":{"X-Priority":"1"},
            "body":"first line\n.leading dot\nlast line"}]),
    )
    .await;
    let result = wait_log(&state, id, "4.2.2 c mailbox full").await;
    assert!(
        result.contains(r#""delivered":["a@example.test"]"#),
        "{result}"
    );
    assert!(result.contains("5.1.1 no such user"), "{result}");
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"lmtp_send","from":"bad sender","to":["x@example.test"],"body":""}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        matches!(rejected, ClientSendOutcome::Rejected { .. }),
        "{rejected:?}"
    );
    let sent = state
        .send_to_client(
            id,
            json!({"type":"lmtp_send","from":"","to":["d@example.test"],"body":"bounce"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    wait_log(&state, id, "5.1.1 unknown d").await;
    let quit = state
        .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(quit, ClientSendOutcome::Disconnected), "{quit:?}");
    fixture.await.unwrap();
    state.remove_client(id).await;
}

#[tokio::test]
async fn a_malformed_reply_ends_the_session() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        r.get_mut().write_all(b"220 fixture\r\n").await.unwrap();
        read(&mut r).await;
        r.get_mut().write_all(b"250 fixture\r\n").await.unwrap();
        assert_eq!(read(&mut r).await, "MAIL FROM:<s@example.test>\r\n");
        r.get_mut().write_all(b"hello there\r\n").await.unwrap();
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).await.unwrap();
        rest
    });
    let (state, id) = start(
        address.to_string(),
        json!({}),
        json!([{"type":"lmtp_send","from":"s@example.test","to":["a@example.test"],"body":"x"}]),
    )
    .await;
    let rest = tokio::time::timeout(Duration::from_secs(10), fixture)
        .await
        .expect("client closes after a malformed reply")
        .unwrap();
    assert!(rest.is_empty(), "nothing after a malformed reply: {rest:?}");
    state.remove_client(id).await;
}

#[tokio::test]
async fn a_server_that_refuses_lhlo_fails_the_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(stream);
        r.get_mut().write_all(b"220 smtp-only\r\n").await.unwrap();
        read(&mut r).await;
        r.get_mut()
            .write_all(b"500 5.5.1 unknown command\r\n")
            .await
            .unwrap();
    });
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let error = ClientForm {
        protocol: "lmtp".into(),
        remote_addr: Some(address.to_string()),
        instruction: Some("x".into()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await;
    match error {
        Err(e) => assert!(format!("{e:#}").contains("refused LHLO"), "{e:#}"),
        Ok(id) => {
            let status = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let status = format!("{:?}", state.get_client(id).await.map(|c| c.status));
                    if status.contains("refused LHLO") {
                        break status;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("connect failure is reported");
            assert!(status.contains("500"), "{status}");
        }
    }
}
