use netget::{
    cli::management::ServerForm,
    state::{AppState, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};
async fn start(
    actions: Option<Value>,
    auth: bool,
) -> (
    AppState,
    ServerId,
    SocketAddr,
    mpsc::UnboundedReceiver<String>,
) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, rx) = mpsc::unbounded_channel();
    let event = if auth { "nut_auth" } else { "nut_request" };
    let id = ServerForm {
        protocol: "nut".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer the UPS request".into()),
        event_handlers: actions
            .map(|a| vec![json!({"event_pattern":event,"handler":{"type":"static","actions":a}})]),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, addr, rx)
}
async fn line(reader: &mut BufReader<TcpStream>) -> String {
    let mut s = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut s))
        .await
        .unwrap()
        .unwrap();
    s
}
async fn decision(logs: &mut mpsc::UnboundedReceiver<String>, expected: &str) {
    let text = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let text = logs.recv().await.unwrap();
            if text.contains("decision=") {
                break text;
            }
        }
    })
    .await
    .unwrap();
    assert!(text.contains(&format!("decision={expected}")), "{text}");
    assert!(
        !text.contains("secret") && !text.contains("127.0.0.1:1"),
        "{text}"
    );
}
#[tokio::test]
async fn terminal_decisions_distinguish_answers_refusals_silence_invalid_and_backend_failure() {
    for (actions, expected, tag, closed) in [
        (
            Some(json!([{"type":"nut_reply","value":"OL"}])),
            "VAR ups ups.status \"OL\"\n",
            "model_answer",
            false,
        ),
        (
            Some(json!([{"type":"nut_reply","error":"UNKNOWN-UPS"}])),
            "ERR UNKNOWN-UPS\n",
            "model_reject",
            false,
        ),
        (Some(json!([])), "ERR DATA-STALE\n", "model_silent", true),
        (
            Some(json!([{"type":"nut_reply","ok":true}])),
            "ERR DATA-STALE\n",
            "fail_closed_invalid_reply",
            true,
        ),
        (
            Some(json!([{"type":"nut_reply","value":"OL"},{"type":"nut_reply","value":"OB"}])),
            "ERR DATA-STALE\n",
            "fail_closed_invalid_reply",
            true,
        ),
        (
            Some(json!([{"type":"nut_reply","error":"INVALID-ERROR"}])),
            "ERR DATA-STALE\n",
            "fail_closed_invalid_reply",
            true,
        ),
        (None, "ERR DATA-STALE\n", "fail_closed_llm_error", true),
    ] {
        let (state, id, addr, mut logs) = start(actions, false).await;
        let mut reader = BufReader::new(TcpStream::connect(addr).await.unwrap());
        reader
            .get_mut()
            .write_all(b"GET VAR ups ups.status\n")
            .await
            .unwrap();
        assert_eq!(line(&mut reader).await, expected);
        decision(&mut logs, tag).await;
        if closed {
            assert_eq!(line(&mut reader).await, "");
        }
        state.remove_server(id).await;
    }
}
#[tokio::test]
async fn auth_decisions_and_silence_stay_distinct_without_logging_credentials() {
    for (actions, reply, tag) in [
        (
            json!([{"type":"nut_auth_decision","allowed":true}]),
            "OK\n",
            "model_answer",
        ),
        (
            json!([{"type":"nut_auth_decision","allowed":false}]),
            "ERR ACCESS-DENIED\n",
            "model_reject",
        ),
        (json!([]), "ERR ACCESS-DENIED\n", "model_silent"),
        (
            json!([{"type":"nut_auth_decision","allowed":"true"}]),
            "ERR ACCESS-DENIED\n",
            "fail_closed_invalid_reply",
        ),
    ] {
        let (state, id, addr, mut logs) = start(Some(actions), true).await;
        let mut reader = BufReader::new(TcpStream::connect(addr).await.unwrap());
        reader
            .get_mut()
            .write_all(b"USERNAME user\nPASSWORD secret\n")
            .await
            .unwrap();
        assert_eq!(line(&mut reader).await, "OK\n");
        assert_eq!(line(&mut reader).await, reply);
        decision(&mut logs, tag).await;
        state.remove_server(id).await;
    }
}
