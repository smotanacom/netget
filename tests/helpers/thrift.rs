//! Thrift fixtures: the Users IDL, a handler policy script, server and client through the shared
//! forms, the pinned thriftpy2 and Apache Thrift peers (`tests/server/thrift/install_peers.py`)
//! and access-log waits.
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

pub(crate) fn idl_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/thrift/users.thrift")
}

pub(crate) fn idl() -> String {
    std::fs::read_to_string(idl_path()).unwrap()
}

const POLICY: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; m=e['method']; a=e['args']
def out(x): print(json.dumps({'actions':[x]})); sys.exit()
if e['oneway']: out({'type':'thrift_ignore'})
if m=='add':
    if a.get('b',0)>=1000: out({'type':'thrift_error','message':'b is too large'})
    out({'type':'thrift_return','value':a['a']+a['b']})
if m=='get_user':
    if a['id']==404: out({'type':'thrift_throw','exception':'missing','value':{'message':'no user 404','id':404}})
    out({'type':'thrift_return','value':{'id':a['id'],'name':'Ada','role':'ADMIN','tags':['x','y']}})
if m=='find':
    out({'type':'thrift_return','value':[{'id':i,'name':a['prefix']+str(i),'role':r,'tags':[]} for i,r in enumerate(sorted(a['roles']),1)]})
if m=='touch': out({'type':'thrift_return'})
out({'type':'thrift_error','message':'unexpected'})
"#;

/// add returns a+b unless b >= 1000 (an application error); get_user(404) throws NotFound, any
/// other id is Ada (ADMIN); find returns one user per role named prefix+n; touch returns; a
/// oneway call is ignored.
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "thrift_call", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

pub(crate) async fn server_in(state: &AppState, handlers: Vec<Value>) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "thrift".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(json!({"idl": idl()})),
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

/// A Thrift client whose events are all answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["thrift_connected", "thrift_result"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "thrift".into(),
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
    .map_err(|_| anyhow::anyhow!("Thrift client did not connect"))??;
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

/// The peer venv's python, from `tests/server/thrift/install_peers.py`.
pub(crate) fn peer_python() -> String {
    std::env::var("NETGET_THRIFT_PYTHON").expect("NETGET_THRIFT_PYTHON must name the venv python from tests/server/thrift/install_peers.py (thriftpy2 0.7.1, thrift 0.25.0); this evidence never skips")
}

pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/thrift/peer.py")
}

pub(crate) fn lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Run a peer client mode to completion; its JSON lines, or a panic with everything it printed.
pub(crate) async fn run_peer(args: &[&str]) -> Vec<Value> {
    let run = tokio::process::Command::new(peer_python())
        .arg(peer_script())
        .args(args)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(90), run)
        .await
        .expect("the Thrift peer did not finish")
        .expect("the Thrift peer did not start");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "peer {args:?} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    lines(&stdout)
}

/// thriftpy2 serving Users on a probed port.
pub(crate) async fn start_server(
    transport: &str,
    protocol: &str,
) -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        &peer_python(),
        super::real_server::InstallHint {
            brew: "python (then tests/server/thrift/install_peers.py)",
            apt: "python3 (then tests/server/thrift/install_peers.py)",
        },
    )
    .args([
        peer_script().display().to_string(),
        "serve".into(),
        idl_path().display().to_string(),
        "{port}".into(),
        transport.into(),
        protocol.into(),
    ])
    .ready_when_log_matches("listening on")
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
}
