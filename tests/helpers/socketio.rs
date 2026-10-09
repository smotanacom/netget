//! Socket.IO fixtures: a chat policy, server and client through the shared forms, the pinned
//! python-socketio and socket.io-client peers and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const CHAT_SCRIPT: &str = r#"import json,sys
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']
if kind=='socketio_connect':
    ns=e['namespace']
    if ns=='/admin':
        acts=[{'type':'socketio_accept'}] if (e.get('auth') or {}).get('token')=='secret' else [{'type':'socketio_reject','message':'not authorized'}]
    else:
        acts=[{'type':'socketio_accept'},{'type':'socketio_emit','event':'welcome','args':['netget',ns]}]
elif kind=='socketio_event':
    ev=e['event']; a=e['args']
    if ev=='chat message':
        acts=[{'type':'socketio_emit','event':'chat message','args':['broadcast']+a,'to':'namespace'}]
        if e['ack_requested']: acts.append({'type':'socketio_ack','args':['delivered']+a})
    elif ev=='ask me': acts=[{'type':'socketio_emit','event':'ping me','args':['are you there?'],'ack':True}]
    elif ev=='join': acts=[{'type':'socketio_join','room':a[0]},{'type':'socketio_emit','event':'joined','args':[a[0]],'to':'room:'+a[0]}]
    elif ev=='bye': acts=[{'type':'socketio_disconnect_socket'}]
    else: acts=[]
elif kind=='socketio_ack_received':
    acts=[{'type':'socketio_emit','event':'pong received','args':e['args']}]
else:
    acts=[]
print(json.dumps({'actions':acts}))
"#;

/// A chat room: / accepts everyone and greets them, /admin needs auth token "secret", chat
/// messages are broadcast and acknowledged, "ask me" makes the server ask the client to
/// acknowledge, "join" joins a room, "bye" disconnects the socket.
pub(crate) fn chat_policy() -> Vec<Value> {
    ["socketio_connect", "socketio_event", "socketio_ack_received", "socketio_disconnect"]
        .iter()
        .map(|e| json!({"event_pattern": e, "handler": {"type": "script", "language": "python", "code": CHAT_SCRIPT}}))
        .collect()
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "socketio".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (id, addr)
}

pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers: Vec<Value> = [
        "socketio_connected",
        "socketio_connect_error",
        "socketio_event",
        "socketio_ack_received",
        "socketio_disconnected",
    ]
    .iter()
    .map(|e| json!({"event_pattern": e, "handler": {"type": "static", "actions": []}}))
    .collect();
    let id = ClientForm {
        protocol: "socketio".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("Socket.IO client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if rows.len() >= count {
                rows.sort_by_key(|e| e.id);
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn python() -> String {
    std::env::var("NETGET_SOCKETIO_PYTHON").expect("NETGET_SOCKETIO_PYTHON must name the Python from tests/server/socketio/install_peers.py (python-socketio 5.17.0); this evidence never skips")
}
pub(crate) fn node_modules() -> String {
    std::env::var("NETGET_SOCKETIO_NODE_MODULES").expect("NETGET_SOCKETIO_NODE_MODULES must name the node_modules from tests/server/socketio/install_peers.py (socket.io-client 4.8.4); this evidence never skips")
}
pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/socketio/peer.py")
}
pub(crate) fn js_peer() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/socketio/js/peer.cjs")
}
