//! RPKI-RTR fixtures: a cache and a router built through the same forms as the dashboard,
//! waits on the access log, and the pinned independent peers.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) const SESSION: u16 = 4242;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

/// A cache whose data is serial 7 = {192.0.2.0/24-24 AS64496, 2001:db8::/32-48 AS64497} and
/// serial 8 = serial 7 with 192.0.2.0/24 withdrawn and 198.51.100.0/22-24 AS64498 announced.
pub(crate) fn cache_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"rpki_rtr_reset_query","handler":{"type":"static","actions":[{"type":"rpki_rtr_response","serial":7,"records":[
            {"prefix":"192.0.2.0/24","max_length":24,"asn":64496},
            {"prefix":"2001:db8::/32","max_length":48,"asn":64497}
        ]}]}}),
        json!({"event_pattern":"rpki_rtr_serial_query","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "if e['router_serial']==7:\n",
            "    a={'type':'rpki_rtr_response','serial':8,'records':[{'prefix':'192.0.2.0/24','max_length':24,'asn':64496,'announcement':False},{'prefix':'198.51.100.0/22','max_length':24,'asn':64498}]}\n",
            "elif e['router_serial']==8:\n",
            "    a={'type':'rpki_rtr_response','serial':8,'records':[]}\n",
            "else:\n",
            "    a={'type':'rpki_rtr_response','cache_reset':True}\n",
            "print(json.dumps({'actions':[a]}))\n"
        )}}),
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
        protocol: "rpki_rtr".into(),
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
    handlers: Vec<Value>,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "rpki_rtr".into(),
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
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("RPKI-RTR client did not connect"))??;
    Ok(id)
}

/// Record every client event without a model.
pub(crate) fn quiet_router() -> Vec<Value> {
    vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})]
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(20), async {
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

pub(crate) fn count(state_logs: &[netget::state::app_state::AccessLogEntry], kind: &str) -> usize {
    state_logs.iter().filter(|e| e.event_type == kind).count()
}

pub(crate) async fn wait_until<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn peer(var: &str, what: &str) -> PathBuf {
    PathBuf::from(std::env::var(var).unwrap_or_else(|_| {
        panic!("{var} must name {what} from tests/server/rpki_rtr/install_peers.py; this evidence never skips")
    }))
}

pub(crate) fn rtrdump() -> PathBuf {
    peer("NETGET_RTRDUMP", "StayRTR 0.6.4 rtrdump")
}
pub(crate) fn stayrtr() -> PathBuf {
    peer("NETGET_STAYRTR", "StayRTR 0.6.4 stayrtr")
}
pub(crate) fn rtrclient() -> PathBuf {
    peer("NETGET_RTRCLIENT", "RTRlib 0.8.0 rtrclient")
}
