//! WAMP fixtures: a router policy script, server and client through the shared forms, a raw
//! WebSocket session for wire tests, and the pinned nexus / autobahn peers
//! (`tests/server/wamp/install_peers.py`).
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use futures::{SinkExt, StreamExt};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const POLICY: &str = r#"import json,sys
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
if kind=='wamp_hello':
    out({'type':'wamp_abort','reason':'wamp.error.no_such_realm','message':'realm '+e['realm']+' is closed'} if e['realm']=='blocked' else {'type':'wamp_welcome','authrole':'user'})
if e['procedure']=='com.example.time': out({'type':'wamp_result','args':['2026-10-04T12:00:00Z'],'kwargs':{'zone':(e['args'] or ['utc'])[0]}})
out({'type':'wamp_error','error':'com.example.error.forbidden','args':['not for you']})
"#;

/// A router policy: every realm but "blocked" is admitted; com.example.time answers a fixed
/// time and echoes its first argument as the zone; any other router call is forbidden.
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "wamp".into(),
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

/// A WAMP client; invocations are answered by `invocation_handler` (and everything else with
/// nothing), so otherwise only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
    invocation_handler: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern": "wamp_invocation", "handler": invocation_handler}),
        json!({"event_pattern": "*", "handler": {"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "wamp".into(),
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
    .map_err(|_| anyhow::anyhow!("WAMP client did not join"))??;
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

pub(crate) fn peer(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set from tests/server/wamp/install_peers.py (nexus 3.3.0, autobahn 24.4.2); this evidence never skips"))
}

pub(crate) fn lines(out: &str) -> Vec<Value> {
    out.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub(crate) fn step<'a>(lines: &'a [Value], name: &str) -> &'a Value {
    lines
        .iter()
        .find(|v| v["step"] == name)
        .unwrap_or_else(|| panic!("no step {name} in {lines:?}"))
}

/// The nexus router (realm1, with its local callee, publisher and subscriber) on a probed port.
pub(crate) async fn start_nexus_router() -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        &peer("NETGET_WAMP_NEXUS"),
        super::real_server::InstallHint {
            brew: "go (then tests/server/wamp/install_peers.py)",
            apt: "golang-go (then tests/server/wamp/install_peers.py)",
        },
    )
    .args(["router", "{port}"])
    .ready_when_log_matches("listening")
    .start()
    .await
}

pub(crate) type RawWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A raw WebSocket to the router offering `subprotocol`.
pub(crate) async fn raw(
    addr: SocketAddr,
    subprotocol: &str,
) -> Result<RawWs, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/").into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        HeaderValue::from_str(subprotocol).unwrap(),
    );
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

pub(crate) async fn send(ws: &mut RawWs, v: Value) {
    ws.send(Message::Text(v.to_string())).await.unwrap();
}

/// The next WAMP message, or None when the router closed.
pub(crate) async fn recv(ws: &mut RawWs) -> Option<Value> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => return Some(serde_json::from_str(&t).unwrap()),
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                _ => continue,
            }
        }
    })
    .await
    .expect("no WAMP message within 10 s")
}

/// HELLO realm1 and expect WELCOME; the session ID.
pub(crate) async fn hello(ws: &mut RawWs, realm: &str) -> Value {
    send(ws, json!([1, realm, {"roles": {"caller": {}, "callee": {}, "publisher": {}, "subscriber": {}}}])).await;
    recv(ws).await.expect("an answer to HELLO")
}
