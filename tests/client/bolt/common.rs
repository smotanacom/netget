#![allow(dead_code)]
use netget::{
    cli::management::ClientForm,
    state::{AccessLogOwner, AppState, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
pub fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}
pub fn static_handler(event: &str, actions: Value) -> Value {
    json!({"event_pattern":event,"handler":{"type":"static","actions":actions}})
}
pub async fn refused_start(state: &AppState, addr: String, params: Value) -> String {
    let (tx, _) = mpsc::unbounded_channel();
    ClientForm {
        protocol: "bolt".into(),
        remote_addr: Some(addr),
        startup_params: Some(params),
        event_handlers: Some(vec![static_handler("*", json!([]))]),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect_err("startup must refuse before client becomes available")
    .to_string()
}
pub async fn client(
    state: &AppState,
    addr: String,
    params: Value,
    handlers: Vec<Value>,
) -> ClientId {
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "bolt".into(),
        remote_addr: Some(addr),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(id).await {
            if state.get_client(id).await.is_some_and(|client| {
                matches!(client.status, netget::state::ClientStatus::Error(_))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    id
}
pub async fn connected_client(state: &AppState, addr: String) -> ClientId {
    let id = client(state, addr, json!({}), vec![static_handler("*", json!([]))]).await;
    event(state, id, "bolt_connected", 0).await;
    id
}
pub async fn latest(state: &AppState, id: ClientId) -> u64 {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| e.id)
        .max()
        .unwrap_or(0)
}
pub async fn event(state: &AppState, id: ClientId, name: &str, after: u64) -> (u64, Value) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .into_iter()
                .filter(|e| e.id > after && e.event_type == name)
                .min_by_key(|e| e.id)
            {
                return (e.id, e.request.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match result {
        Ok(event) => event,
        Err(_) => panic!(
            "Bolt event {name} after {after}; events/errors: {:?}",
            state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .take(12)
                .map(|e| (e.id, &e.event_type, e.request.get("error")))
                .collect::<Vec<_>>()
        ),
    }
}
pub async fn send(state: &AppState, id: ClientId, action: Value) {
    let outcome = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            netget::state::client_handles::ClientSendOutcome::Sent { .. }
        ),
        "{outcome:?}"
    );
}
pub async fn rejected(state: &AppState, id: ClientId, action: Value, needle: &str) {
    let result = state
        .send_to_client(id, action, Duration::from_secs(2))
        .await
        .unwrap();
    match result {
        netget::state::client_handles::ClientSendOutcome::Rejected { error } => {
            assert!(error.contains(needle), "{error}")
        }
        other => panic!("{other:?}"),
    }
}
pub async fn failed(state: &AppState, id: ClientId, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let netget::state::ClientStatus::Error(e) =
                state.get_client(id).await.unwrap().status
            {
                assert!(e.contains(needle), "{e}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

use netget::server::bolt::{
    messages as m,
    packstream::{self, Dechunker, Value as Wire},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
pub struct MockPeer {
    pub address: String,
    pub mode: Arc<Mutex<String>>,
    pub seen: Arc<Mutex<Vec<Wire>>>,
    pub closed: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl MockPeer {
    pub async fn start(minor: u8, mode: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("bolt://{}", listener.local_addr().unwrap());
        let mode = Arc::new(Mutex::new(mode.to_owned()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let (task_mode, task_seen, task_closed) = (mode.clone(), seen.clone(), closed.clone());
        let task = tokio::spawn(async move {
            let (mut io, _) = listener.accept().await.unwrap();
            let mut hs = [0u8; 20];
            if io.read_exact(&mut hs).await.is_err() {
                task_closed.store(true, Ordering::SeqCst);
                return;
            }
            assert_eq!(&hs[..4], &m::MAGIC);
            assert_eq!(&hs[4..], &netget::client::bolt::api::PROPOSALS);
            if task_mode.lock().unwrap().as_str() == "handshake-drip" {
                for byte in [0, 0, minor, 5] {
                    if io.write_all(&[byte]).await.is_err() {
                        task_closed.store(true, Ordering::SeqCst);
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
            } else if io.write_all(&[0, 0, minor, 5]).await.is_err() {
                task_closed.store(true, Ordering::SeqCst);
                return;
            }
            let mut decoder = Dechunker::new(packstream::MAX_MESSAGE_BYTES);
            let mut buffer = [0u8; 16384];
            let mut cursor = 1;
            loop {
                let bytes = loop {
                    if let Some(bytes) = decoder.next_message().unwrap() {
                        break bytes;
                    }
                    let n = io.read(&mut buffer).await.unwrap_or_default();
                    if n == 0 {
                        task_closed.store(true, Ordering::SeqCst);
                        return;
                    }
                    decoder.push(&buffer[..n]);
                };
                let request = packstream::decode(&bytes).unwrap();
                task_seen.lock().unwrap().push(request.clone());
                let Wire::Struct { tag, fields } = request else {
                    panic!("typed request")
                };
                let mode = task_mode.lock().unwrap().clone();
                let responses = match tag {
                    m::HELLO if mode == "hello-hang" => continue,
                    m::HELLO => vec![m::success(vec![
                        ("server", Wire::string("Neo4j/fixture")),
                        ("connection_id", Wire::string("bolt-owned")),
                    ])],
                    m::GOODBYE => {
                        task_closed.store(true, Ordering::SeqCst);
                        return;
                    }
                    m::LOGON | m::LOGOFF | m::RESET | m::BEGIN | m::ROLLBACK | m::COMMIT => {
                        vec![m::success(vec![])]
                    }
                    m::RUN => {
                        cursor = 1;
                        if mode == "hang" {
                            continue;
                        }
                        if mode == "noop" {
                            for _ in 0..30 {
                                if io.write_all(&[0, 0]).await.is_err() {
                                    task_closed.store(true, Ordering::SeqCst);
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                            continue;
                        }
                        if mode == "bad-run" {
                            vec![m::success(vec![("fields", Wire::string("not a list"))])]
                        } else {
                            vec![m::success(vec![
                                ("fields", Wire::List(vec![Wire::string("x")])),
                                ("qid", Wire::Int(7)),
                                ("t_first", Wire::Int(2)),
                            ])]
                        }
                    }
                    m::PULL => {
                        assert_eq!(fields[0].get("qid").and_then(Wire::as_int), Some(7));
                        let n = fields[0].get("n").and_then(Wire::as_int).unwrap() as usize;
                        if mode == "oversize" {
                            let bytes = vec![b'x'; packstream::MAX_MESSAGE_BYTES + 1];
                            if io.write_all(&packstream::chunk(&bytes)).await.is_err() {
                                task_closed.store(true, Ordering::SeqCst);
                                return;
                            }
                            continue;
                        }
                        let mut records = Vec::new();
                        let limit = if mode == "short-more" {
                            0
                        } else if mode == "page-bytes" {
                            n
                        } else if mode == "too-many" {
                            n + 1
                        } else {
                            n.min((4 - cursor) as usize)
                        };
                        for _ in 0..limit {
                            records.push(m::record(if mode == "page-bytes" {
                                vec![Wire::string(
                                    "x".repeat(netget::client::bolt::api::MAX_TEXT),
                                )]
                            } else if mode == "wrong-width" {
                                vec![Wire::Int(cursor), Wire::Int(cursor)]
                            } else {
                                vec![Wire::Int(cursor)]
                            }));
                            cursor += 1;
                        }
                        records.push(if mode == "failure-after-record" {
                            m::failure(
                                minor,
                                "Neo.ClientError.Statement.SyntaxError",
                                "selected failure",
                            )
                        } else if mode == "ignored" {
                            m::ignored()
                        } else if mode == "bad-summary" {
                            m::success(vec![("has_more", Wire::string("true"))])
                        } else if cursor < 4 {
                            m::success(vec![("has_more", Wire::Bool(true))])
                        } else {
                            m::success(vec![
                                ("type", Wire::string("r")),
                                ("bookmark", Wire::string("opaque-bookmark")),
                            ])
                        });
                        records
                    }
                    m::DISCARD if mode == "discard-more" => {
                        vec![m::success(vec![("has_more", Wire::Bool(true))])]
                    }
                    m::DISCARD => vec![m::success(vec![("type", Wire::string("r"))])],
                    _ => panic!("unexpected request tag"),
                };
                for response in responses {
                    if io
                        .write_all(&packstream::chunk(&packstream::to_bytes(&response)))
                        .await
                        .is_err()
                    {
                        task_closed.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            }
        });
        Self {
            address,
            mode,
            seen,
            closed,
            task,
        }
    }
    pub fn set(&self, mode: &str) {
        *self.mode.lock().unwrap() = mode.into()
    }
    pub async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
pub async fn wait_closed(peer: &MockPeer) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !peer.closed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap()
}
pub async fn authenticated_mock(state: &AppState, peer: &MockPeer) -> ClientId {
    let id = connected_client(state, peer.address.clone()).await;
    send(state, id, json!({"type":"bolt_login"})).await;
    event(state, id, "bolt_authentication", 0).await;
    id
}
pub async fn receipt(state: &AppState, id: ClientId, action: Value, name: &str) -> Value {
    let after = latest(state, id).await;
    send(state, id, action).await;
    event(state, id, name, after).await.1
}

impl Drop for MockPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
