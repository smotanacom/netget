//! NBD fixtures: an export policy script, server and client through the shared forms, the
//! pinned libnbd and nbdkit (`tests/server/nbd/install_peers.py`) and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use super::real_server::{InstallHint, RealServer};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); e=i['event']
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
if i['event_type_id']=='nbd_list':
    out({'type':'nbd_list_exports','exports':[{'name':'disk0','description':'boot disk'},{'name':'flaky'}]})
disk={'type':'nbd_export','size':1048576,'description':'boot disk','extents':[{'offset':0,'text':'hello netget'},{'offset':4096,'length':512,'fill':171},{'offset':8192,'hex':'deadbeef'}]}
if e['export'] in ('','disk0'): out(disk)
if e['export']=='flaky':
    disk['extents'].append({'offset':524288,'length':4096,'fill':1})
    disk['errors']=[{'offset':524288,'length':4096,'error':'EIO'}]
    out(disk)
if e['export']=='secret': out({'type':'nbd_reject','reason':'policy'})
out({'type':'nbd_reject','reason':'unknown'})
"#;

/// disk0 (and the default export): 1 MiB, "hello netget" at 0, 512 bytes of 0xAB at 4096,
/// de ad be ef at 8192, zeroes elsewhere. flaky: the same plus 4 KiB of data at 512 KiB whose
/// reads fail with EIO (data, so a sparse copier cannot skip it).
/// secret is refused by policy; anything else is unknown.
pub(crate) fn policy() -> Vec<Value> {
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": POLICY}}),
    ]
}

pub(crate) async fn server_in(state: &AppState) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "nbd".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(policy()),
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

/// An NBD client whose events are answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = [
        "nbd_connected",
        "nbd_read_result",
        "nbd_block_status_result",
        "nbd_flush_result",
    ]
    .iter()
    .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
    .collect();
    let id = ClientForm {
        protocol: "nbd".into(),
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
    .map_err(|_| anyhow::anyhow!("NBD client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<Value> {
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
                break rows.into_iter().map(|e| e.request).collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn tool(var: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must name what tests/server/nbd/install_peers.py built (libnbd 1.24.3, nbdkit 1.48.1); this evidence never skips"))
}

/// Run a libnbd tool to completion: (success, stdout, stderr).
pub(crate) async fn libnbd(var: &str, args: &[&str]) -> (bool, String, String) {
    let out = tokio::process::Command::new(tool(var)).args(args).output();
    let out = tokio::time::timeout(Duration::from_secs(60), out)
        .await
        .expect("the libnbd tool hung")
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// nbdkit serving export "disk": 1 MiB of its data plugin with "hello nbdkit" at 0 and "tail" at
/// 64 KiB; every read fails with EIO while `{dir}/inject` exists.
pub(crate) async fn start_nbdkit() -> super::E2EResult<RealServer> {
    RealServer::builder(
        &tool("NETGET_NBD_NBDKIT"),
        InstallHint {
            brew: "gnutls pkgconf (then tests/server/nbd/install_peers.py)",
            apt: "libgnutls28-dev pkg-config (then tests/server/nbd/install_peers.py)",
        },
    )
    .args([
        "-f".to_owned(),
        "-i".into(),
        "127.0.0.1".into(),
        "-p".into(),
        "{port}".into(),
        "-e".into(),
        "disk".into(),
        "--filter".into(),
        tool("NETGET_NBD_ERROR_FILTER"),
        tool("NETGET_NBD_DATA_PLUGIN"),
        "data=\"hello nbdkit\" @65536 \"tail\"".into(),
        "size=1M".into(),
        "error-pread=EIO".into(),
        "error-pread-rate=100%".into(),
        "error-pread-file={dir}/inject".into(),
    ])
    .start()
    .await
}
