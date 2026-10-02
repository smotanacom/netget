use super::common::*;
use netget::cli::management::ServerForm;
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::mpsc,
};
#[tokio::test]
async fn netget_server_pair_covers_body_framing_and_shared_tube_state() {
    let state = state();
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let sid=ServerForm{protocol:"beanstalkd".into(),port:Some(0),host:Some("127.0.0.1".into()),event_handlers:Some(vec![
 json!({"event_pattern":"beanstalkd_put","handler":{"type":"static","actions":[{"type":"insert_beanstalkd_job","job_id":44}]}}),
 json!({"event_pattern":"beanstalkd_reserve","handler":{"type":"static","actions":[{"type":"reserve_beanstalkd_job","job_id":44,"body":"pair ✓\r\nDELETED"}]}}),
 json!({"event_pattern":"beanstalkd_job_command","handler":{"type":"static","actions":[{"type":"send_beanstalkd_status","status":"DELETED"}]}}),
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
        request(&state, id, json!({"operation":"use","tube":"images"})).await["tube"],
        "images"
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"put","body":"pair ✓\r\nDELETED"})
        )
        .await["id"],
        44
    );
    assert_eq!(
        request(&state, id, json!({"operation":"reserve"})).await["body"],
        "pair ✓\r\nDELETED"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"delete","id":44})).await["status"],
        "DELETED"
    );
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn disconnect_interrupts_stalled_reservation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        assert_eq!(line, "reserve-with-timeout 25\r\n");
        tx.send(()).unwrap();
        line.clear();
        assert_eq!(r.read_line(&mut line).await.unwrap(), 0);
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!([{"type":"beanstalkd_request","operation":"reserve","timeout_secs":25}]),
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
async fn fragmented_reply_and_final_event_survive_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        let (s, _) = listener.accept().await.unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).await.unwrap();
        for chunk in ["US", "ING def", "ault\r\n"] {
            r.get_mut().write_all(chunk.as_bytes()).await.unwrap();
        }
    });
    let state = state();
    let id = client(
        &state,
        addr.to_string(),
        json!([{"type":"beanstalkd_request","operation":"list_tube_used"}]),
    )
    .await;
    assert_eq!(response(&state, id, 0).await["tube"], "default");
    fixture.await.unwrap();
    state.remove_client(id).await;
}
