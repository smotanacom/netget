//! DICOM fixtures: an SCP policy backed by a JSON file, server and client through the shared
//! forms, the pinned pynetdicom peer (`tests/server/dicom/install_peers.py`) and access-log waits.
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

const SCP_SCRIPT: &str = r#"import json,sys
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']; STATE='__STATE__'
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
try: db=json.load(open(STATE))
except Exception: db=[]
if kind=='dicom_associate':
    if e['calling_ae']=='BLOCKED': out({'type':'dicom_reject','reason':'calling_ae_not_recognized'})
    if e['calling_ae']=='BROKEN': out({'type':'dicom_store_status','status':'success'})
    out({'type':'dicom_accept'})
if kind=='dicom_store':
    ds=e['dataset']
    if ds.get('00100020',{}).get('Value',[''])[0]=='REJECT': out({'type':'dicom_store_status','status':'out_of_resources','comment':'archive full'})
    db.append(ds); json.dump(db,open(STATE,'w')); out({'type':'dicom_store_status','status':'success'})
if kind=='dicom_find':
    out({'type':'dicom_find_matches','matches':db})
"#;

/// An archive in `state` (a JSON list): every calling AE but BLOCKED (rejected) and BROKEN (an
/// answer that is not an association decision) is accepted; an instance whose PatientID is
/// REJECT is refused out_of_resources; every C-FIND is answered with everything stored, which
/// the server then matches and projects.
pub(crate) fn scp_policy(state: &std::path::Path) -> Vec<Value> {
    let code = SCP_SCRIPT.replace("__STATE__", &state.display().to_string());
    ["dicom_associate", "dicom_store", "dicom_find"]
        .iter()
        .map(|e| json!({"event_pattern": e, "handler": {"type":"script","language":"python","code": code}}))
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
        protocol: "dicom".into(),
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

/// A DICOM client whose events are all answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["dicom_associated", "dicom_response"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "dicom".into(),
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
    .map_err(|_| anyhow::anyhow!("DICOM client did not associate"))??;
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

/// The peer venv's python, from `tests/server/dicom/install_peers.py`.
pub(crate) fn peer_python() -> String {
    std::env::var("NETGET_DICOM_PYTHON").expect("NETGET_DICOM_PYTHON must name the venv python from tests/server/dicom/install_peers.py (pynetdicom 3.0.4, pydicom 3.0.2); this evidence never skips")
}

pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/dicom/peer.py")
}

/// pynetdicom as an SCP titled PACS on a probed port.
pub(crate) async fn start_scp() -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        &peer_python(),
        super::real_server::InstallHint {
            brew: "python (then tests/server/dicom/install_peers.py)",
            apt: "python3 (then tests/server/dicom/install_peers.py)",
        },
    )
    .args([
        peer_script().display().to_string(),
        "scp".into(),
        "{port}".into(),
    ])
    .ready_when_log_matches("listening on")
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
}
