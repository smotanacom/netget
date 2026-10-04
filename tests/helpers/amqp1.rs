//! AMQP 1.0 fixtures: a container policy script, server and client through the shared forms, the
//! pinned rhea and go-amqp peers (`tests/server/amqp1/install_peers.py`) and access-log waits.
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

const POLICY: &str = r#"import json,sys
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
if kind=='amqp1_connect':
    ok = e['mechanism'] in ('ANONYMOUS','none') or (e.get('user')=='alice' and e.get('password')=='secret')
    out({'type':'amqp1_accept'} if ok else {'type':'amqp1_reject','condition':'amqp:unauthorized-access','description':'bad credentials'})
if kind=='amqp1_attach':
    out({'type':'amqp1_reject','condition':'amqp:unauthorized-access','description':'forbidden address'} if e['address']=='forbidden' else {'type':'amqp1_accept'})
if kind=='amqp1_credit':
    out({'type':'amqp1_send','message':{'body':'fresh news','properties':{'subject':'news'}}} if e['address']=='news' else {'type':'amqp1_ignore'})
m=e['message']; b=m.get('body')
if e['address']=='orders':
    if isinstance(b,str):
        try: b=json.loads(b)
        except Exception: pass
    if isinstance(b,dict) and 'order' in b:
        out({'type':'amqp1_accept'},{'type':'amqp1_send','address':'orders.confirmed','message':{'body':{'order':b['order'],'status':'confirmed'},'properties':{'subject':'confirmation'}}})
    out({'type':'amqp1_reject','condition':'amqp:precondition-failed','description':'no order id'})
out({'type':'amqp1_accept'})
"#;

/// alice/secret (or ANONYMOUS) may connect; "forbidden" may not be attached; a message to orders
/// with an order id (as a value, or JSON text in a data body) is accepted and confirmed on
/// orders.confirmed, without one it is rejected; a receiver on news is given "fresh news";
/// anything else is accepted.
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
        protocol: "amqp1".into(),
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
    let handlers = vec![json!({"event_pattern": "*", "handler": {"type":"static","actions":[]}})];
    let id = ClientForm {
        protocol: "amqp1".into(),
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
    .map_err(|_| anyhow::anyhow!("AMQP 1.0 client did not connect"))??;
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
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must be set from tests/server/amqp1/install_peers.py (rhea 3.0.5, go-amqp 1.7.0); this evidence never skips"))
}

pub(crate) fn script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/server/amqp1/js")
        .join(name)
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

/// The rhea broker on a probed port.
pub(crate) async fn start_broker() -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        "node",
        super::real_server::InstallHint {
            brew: "node (then tests/server/amqp1/install_peers.py)",
            apt: "nodejs (then tests/server/amqp1/install_peers.py)",
        },
    )
    .env("NODE_PATH", &peer("NETGET_AMQP1_NODE_MODULES"))
    .args([
        script("broker.cjs").display().to_string(),
        "{port}".to_owned(),
    ])
    .ready_when_log_matches("listening")
    .start()
    .await
}
