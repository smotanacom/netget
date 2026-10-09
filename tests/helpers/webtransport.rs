//! WebTransport fixtures: a handler policy, server and client through the shared forms, the
//! pinned aioquic 1.3.0 peer (`tests/server/webtransport/peer.py`) and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

pub(crate) const SERVER_POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if k=='webtransport_session_request':
    p=e['path']
    if p in ('/echo','/pair'): out({'type':'webtransport_accept','headers':{'x-netget':'yes'}})
    if p=='/forbidden': out({'type':'webtransport_reject','status':403})
    if p=='/busy': out({'type':'webtransport_reject','status':429})
    out()
if k=='webtransport_stream':
    d=e['data']
    if e['direction']=='unidirectional': out({'type':'webtransport_open_uni','data':'uni:'+d})
    if e['encoding']=='hex': out({'type':'webtransport_reply','data':d,'encoding':'hex'})
    if d=='ask me': out({'type':'webtransport_reply','data':'asking'},{'type':'webtransport_open_bi','data':'question?'})
    if d=='bye': out({'type':'webtransport_close','code':7,'reason':'done'})
    if d=='silence': out()
    out({'type':'webtransport_reply','data':'pong:'+d})
if k=='webtransport_datagram': out({'type':'webtransport_send_datagram','data':'dg:'+e['data']})
if k=='webtransport_stream_reply': out({'type':'webtransport_send_datagram','data':'got '+e['data']})
out()
"#;

pub(crate) fn script(code: &str) -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": code}}),
    ]
}

pub(crate) async fn server_with(
    state: &AppState,
    handlers: Option<Vec<Value>>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "webtransport".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: handlers,
        startup_params: Some(params),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(20), async {
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

pub(crate) async fn server_in(state: &AppState, params: Value) -> (ServerId, SocketAddr) {
    server_with(state, Some(script(SERVER_POLICY)), params).await
}

/// The SHA-256 the server published for its certificate.
pub(crate) async fn certificate_sha256(state: &AppState, id: ServerId) -> String {
    state.get_server(id).await.unwrap().protocol_data["certificate_sha256"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    handlers: Vec<Value>,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "webtransport".into(),
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
    tokio::time::timeout(Duration::from_secs(20), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("WebTransport client did not connect"))??;
    Ok(id)
}

pub(crate) async fn wait_for(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    want: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .find(|e| e.event_type == kind && want(&e.request))
            {
                break e.request;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for a matching {kind} access-log entry"))
}

pub(crate) fn python() -> String {
    std::env::var("NETGET_AIOQUIC_PYTHON").expect("NETGET_AIOQUIC_PYTHON must name a Python with tests/helpers/aioquic-requirements.txt installed (aioquic 1.3.0); this evidence never skips")
}

pub(crate) fn peer_script() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/webtransport/peer.py"
    )
    .into()
}

/// Run the peer to completion and parse the last JSON line it printed.
pub(crate) async fn peer(args: &[&str]) -> Value {
    let run = tokio::process::Command::new(python())
        .arg(peer_script())
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .unwrap_or_else(|_| panic!("peer.py {args:?} did not finish"))
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "peer.py {args:?} failed:\n{stdout}\n{stderr}"
    );
    serde_json::from_str(stdout.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("peer.py {args:?} printed no result ({e}):\n{stdout}\n{stderr}"))
}

/// A fresh certificate for the peer or the server; returns its sha-256.
pub(crate) async fn make_cert(dir: &Path) -> String {
    peer(&["cert", dir.to_str().unwrap()]).await["sha256"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// aioquic's echo server (see peer.py), with what it prints collected line by line.
pub(crate) struct EchoServer {
    pub port: u16,
    child: tokio::process::Child,
    lines: mpsc::UnboundedReceiver<Value>,
    pub seen: Vec<Value>,
}

impl EchoServer {
    pub(crate) async fn start(dir: &Path) -> Self {
        let mut child = tokio::process::Command::new(python())
            .arg(peer_script())
            .args(["server", dir.to_str().unwrap()])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("start peer.py server");
        let (tx, mut lines) = mpsc::unbounded_channel();
        let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = out.next_line().await {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        let ready = tokio::time::timeout(Duration::from_secs(30), lines.recv())
            .await
            .expect("peer.py server did not start")
            .expect("peer.py server exited");
        assert_eq!(ready["ready"], true, "{ready}");
        Self {
            port: ready["port"].as_u64().unwrap() as u16,
            child,
            lines,
            seen: vec![],
        }
    }

    /// Wait for a printed line matching `want`; earlier lines stay in `seen`.
    pub(crate) async fn wait_for(&mut self, want: impl Fn(&Value) -> bool) -> Value {
        if let Some(v) = self.seen.iter().find(|v| want(v)) {
            return v.clone();
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let v = self.lines.recv().await.expect("peer.py server exited");
                self.seen.push(v.clone());
                if want(&v) {
                    break v;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("peer.py server never printed the line; saw {:?}", self.seen))
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
