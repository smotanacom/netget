//! Shared NETCONF fixtures: an owned host key, a server and a client built through the same
//! forms the dashboard and MCP use, and waits on the access log.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) const DEMO: &str = "urn:netget:netconf-peer";

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

pub(crate) struct HostKey {
    pub path: PathBuf,
    /// OpenSSH fingerprint, `SHA256:<base64>`.
    pub sha256: String,
    /// Public key blob in base64, as ncclient's `hostkey_b64` takes it.
    pub b64: String,
    _dir: tempfile::TempDir,
}

pub(crate) fn host_key() -> HostKey {
    use russh_keys::PublicKeyBase64;
    let dir = tempfile::tempdir().unwrap();
    let key = russh_keys::key::KeyPair::generate_ed25519().unwrap();
    let public = key.clone_public_key().unwrap();
    let path = dir.path().join("host_ed25519");
    let mut pem = Vec::new();
    russh_keys::encode_pkcs8_pem(&key, &mut pem).unwrap();
    std::fs::write(&path, pem).unwrap();
    HostKey {
        path,
        sha256: format!("SHA256:{}", public.fingerprint()),
        b64: public.public_key_base64(),
        _dir: dir,
    }
}

/// Answers for the fixture device: one demo label, admin/secret only.
pub(crate) fn device_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"netconf_auth","handler":{"type":"script","language":"python","code":"import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'netconf_auth_decision','allowed':e['username']=='admin' and e['password']=='secret'}]}))"}}),
        json!({"event_pattern":"netconf_rpc","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "op=e['operation']\n",
            "demo='<demo xmlns=\"urn:netget:netconf-peer\"><label owner=\"fixture\">fixture α</label></demo>'\n",
            "if op in ('get','get-config'):\n",
            "    a={'type':'netconf_rpc_reply','data_xml':demo}\n",
            "elif op=='edit-config':\n",
            "    a={'type':'netconf_rpc_reply','ok':True} if 'label' in e.get('config_xml','') else {'type':'netconf_rpc_reply','errors':[{'error_type':'application','error_tag':'invalid-value','error_message':'label required'}]}\n",
            "elif op=='lock':\n",
            "    a={'type':'netconf_rpc_reply','errors':[{'error_type':'protocol','error_tag':'lock-denied','error_message':'held by session 99','error_info_xml':'<session-id>99</session-id>'}]}\n",
            "elif e.get('custom'):\n",
            "    a={'type':'netconf_rpc_reply','output_xml':'<uptime xmlns=\"urn:netget:netconf-peer\">42</uptime>'}\n",
            "else:\n",
            "    a={'type':'netconf_rpc_reply','ok':True}\n",
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
        protocol: "netconf".into(),
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

/// Create a client; `Err` carries the connect failure.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    handlers: Vec<Value>,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "netconf".into(),
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
    let connected = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if state.has_client_handle(id).await {
                break Ok(());
            }
            match state.get_client(id).await.map(|c| c.status) {
                Some(ClientStatus::Error(e)) => break Err(anyhow::anyhow!(e)),
                None | Some(ClientStatus::Disconnected) => {
                    break Err(anyhow::anyhow!("client went away"))
                }
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("NETCONF client did not connect"))?;
    connected.map(|_| id)
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

pub(crate) fn python() -> String {
    std::env::var("NETGET_NETCONF_PYTHON").expect(
        "NETGET_NETCONF_PYTHON must name the Python from tests/server/netconf/install_peers.py \
         (ncclient 0.7.1 / netconf 2.1.0); this evidence never skips",
    )
}

pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/netconf/peer.py")
}
