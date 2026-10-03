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
        protocol: "nostr".into(),
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
        protocol: "nostr".into(),
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
    event(state, id, "nostr_connected", 0).await;
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
            "Nostr event {name} after {after}; events/errors: {:?}",
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
            netget::state::client_handles::ClientSendOutcome::Executed { .. }
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

use futures::{SinkExt, StreamExt};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};
use tokio_tungstenite::{
    tungstenite::{protocol::Role, Message},
    WebSocketStream,
};
pub const SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000059";
pub const OTHER_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000060";
pub fn signed(content: &str) -> Value {
    netget::server::nostr::wire::RelayKey::from_hex(OTHER_SECRET)
        .unwrap()
        .sign(
            1700000000,
            1,
            vec![vec!["t".into(), "film".into()]],
            content.into(),
        )
        .to_json()
}
pub struct MockPeer {
    pub address: String,
    seen: Arc<Mutex<Vec<Value>>>,
    outgoing: mpsc::Sender<Message>,
    mode: Arc<Mutex<String>>,
    pub closed: Arc<AtomicBool>,
    pub info_seen: Arc<AtomicBool>,
    pub info_closed: Arc<AtomicBool>,
    task: JoinHandle<()>,
}
impl Drop for MockPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl MockPeer {
    pub fn seen(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
    pub fn mode(&self, mode: &str) {
        *self.mode.lock().unwrap() = mode.into();
    }
    pub async fn frame(&self, value: Value) {
        self.outgoing
            .send(Message::Text(value.to_string()))
            .await
            .unwrap();
    }
    pub async fn raw(&self, message: Message) {
        self.outgoing.send(message).await.unwrap();
    }
    pub async fn stop(self) {
        self.task.abort();
    }
}
pub async fn wait(condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("peer-observed condition");
}
pub async fn mock_peer(mode: &str) -> MockPeer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("ws://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mode = Arc::new(Mutex::new(mode.to_owned()));
    let closed = Arc::new(AtomicBool::new(false));
    let info_seen = Arc::new(AtomicBool::new(false));
    let info_closed = Arc::new(AtomicBool::new(false));
    let (outgoing, mut frames) = mpsc::channel::<Message>(128);
    let (s, m, c, is, ic) = (
        seen.clone(),
        mode.clone(),
        closed.clone(),
        info_seen.clone(),
        info_closed.clone(),
    );
    let task = tokio::spawn(async move {
        let mut children = JoinSet::new();
        let (writer, mut received) = mpsc::channel(128);
        let mut output: Option<mpsc::Sender<Message>> = None;
        loop {
            tokio::select! {
                connection=listener.accept()=>{
                    let (mut stream,_)=connection.unwrap();
                    let (s,m,c,is,ic,writer)=(s.clone(),m.clone(),c.clone(),is.clone(),ic.clone(),writer.clone());
                    children.spawn(async move {
                        let mut head=Vec::new();
                        loop {let mut b=[0];if stream.read(&mut b).await.unwrap()==0 {return};head.push(b[0]);if head.ends_with(b"\r\n\r\n") {break;}assert!(head.len()<16384);}
                        let request=String::from_utf8(head.clone()).unwrap();
                        if request.to_ascii_lowercase().contains("accept: application/nostr+json") {
                            is.store(true,Ordering::SeqCst);
                            let mode=m.lock().unwrap().clone();
                            if mode=="info_stall" {
                                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length:100\r\nConnection:close\r\n\r\n{").await.unwrap();
                                let mut b=[0];let _=stream.read(&mut b).await;ic.store(true,Ordering::SeqCst);return;
                            }
                            let body=if mode=="info_bad" {"{\"supported_nips\":[\"1\"]}".into()} else {json!({"name":"owned mock","supported_nips":[1,11,42],"limitation":{"auth_required":true,"max_subscriptions":20},"unknown":{"not":"negotiated"}}).to_string()};
                            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length:{}\r\nConnection:close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();return;
                        }
                        let head=netget::server::nostr::http::parse_request_head(&head).unwrap();
                        let key=netget::server::nostr::http::validate_upgrade(&head).unwrap();
                        stream.write_all(netget::server::nostr::http::accept_response(&key).as_bytes()).await.unwrap();
                        let mut ws=WebSocketStream::from_raw_socket(stream,Role::Server,None).await;
                        let (reply,mut outbound)=mpsc::channel::<Message>(128);
                        writer.send(reply).await.unwrap();
                        loop {tokio::select! {
                            frame=outbound.recv()=>{let Some(frame)=frame else {return};if ws.send(frame).await.is_err() {c.store(true,Ordering::SeqCst);return;}},
                            frame=ws.next()=>{
                                let Some(Ok(frame))=frame else {c.store(true,Ordering::SeqCst);return;};
                                match frame {
                                    Message::Text(text)=>{
                                        let value:Value=serde_json::from_str(&text).unwrap();s.lock().unwrap().push(value.clone());
                                        let mode=m.lock().unwrap().clone();
                                        if value[0]=="EVENT" && mode!="no_ok" {
                                            let reason=if mode=="reject" {"blocked: owned rejection"} else {""};
                                            ws.send(Message::Text(json!(["OK",value[1]["id"],mode!="reject",reason]).to_string())).await.unwrap();
                                        } else if value[0]=="REQ" && mode!="silent" {ws.send(Message::Text(json!(["EOSE",value[1]]).to_string())).await.unwrap();}
                                    }
                                    Message::Pong(bytes)=>s.lock().unwrap().push(json!({"pong":bytes})),
                                    Message::Close(_)=>{c.store(true,Ordering::SeqCst);return;},
                                    _=>{},
                                }
                            }
                        }}
                    });
                },
                sender=received.recv()=>{output=sender;},
                frame=frames.recv(),if output.is_some()=>{let Some(frame)=frame else{return};let _=output.as_ref().unwrap().send(frame).await;},
                _=children.join_next(),if !children.is_empty()=>{},
            }
        }
    });
    MockPeer {
        address,
        seen,
        outgoing,
        mode,
        closed,
        info_seen,
        info_closed,
        task,
    }
}
