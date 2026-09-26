//! The in-flight bound: a model (or an operator) that pipelines requests at a server that does
//! not answer is refused once `MAX_IN_FLIGHT` requests are waiting, rather than growing the FIFO
//! that pairs replies with requests without limit.
//!
//! The "server" is a bare listener that accepts and never answers. That is the point: no real
//! memcached would hold 128 replies back, so this bound can only be shown against one that does.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features memcached --test client -- memcached::in_flight_test --test-threads=100

use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;

#[tokio::test]
async fn requests_past_the_in_flight_bound_are_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept, read and discard forever; never answer.
    let silent = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut sink = vec![0u8; 64 * 1024];
        loop {
            use tokio::io::AsyncReadExt;
            match sock.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let client_id = ClientForm {
        protocol: "memcached".to_string(),
        remote_addr: Some(addr.to_string()),
        instruction: Some("test client".to_string()),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx,
    )
    .await
    .expect("create memcached client");
    for _ in 0..1_000 {
        if state.has_client_handle(client_id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut sent = 0;
    let mut refused = None;
    for i in 0..200 {
        let outcome = state
            .send_to_client(
                client_id,
                json!({"type": "memcached_get", "keys": [format!("k{i}")]}),
                Duration::from_secs(10),
            )
            .await
            .expect("send_to_client");
        match outcome {
            ClientSendOutcome::Sent { .. } => sent += 1,
            ClientSendOutcome::Rejected { error } => {
                refused = Some(error);
                break;
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }
    assert_eq!(
        sent, 128,
        "exactly MAX_IN_FLIGHT requests go out unanswered"
    );
    let refused = refused.expect("the request past the bound must be refused");
    assert!(
        refused.contains("already waiting for a reply"),
        "refusal should say why: {refused}"
    );
    silent.abort();
}
