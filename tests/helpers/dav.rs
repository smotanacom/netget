//! CalDAV/CardDAV fixtures: a login-and-store policy, server and client through the shared
//! forms, the pinned python caldav / vdirsyncer / Radicale peers and access-log waits.
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

const STORE_SCRIPT: &str = r#"import json,sys,hashlib
d=json.load(sys.stdin); kind=d['event_type_id']; e=d['event']
P='__PREFIX__'; STATE='__STATE__'
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
def err(status,pre=None): out({'type':P+'_error','status':status,'precondition':pre})
if kind==P+'_login':
    out({'type':P+('_login_accept' if (e['user_name'],e['password'])==('alice','secret') else '_login_reject')})
try: db=json.load(open(STATE))
except Exception: db={'collections':{'__DEFAULT__':{'displayname':'Default','objects':{}}}}
op=e['operation']; colls=db['collections']
def save(): json.dump(db,open(STATE,'w'))
def etag(data): return '"'+hashlib.sha1(data.encode()).hexdigest()[:16]+'"'
if op=='list_collections': out({'type':P+'_collections','collections':[{'name':k,'displayname':v['displayname']} for k,v in colls.items()]})
if op=='proppatch': out({'type':P+'_done'})
name=e.get('collection'); c=colls.get(name)
if op=='make_collection':
    if c is not None: err(403,'resource-must-be-null')
    colls[name]={'displayname':e.get('displayname') or name,'objects':{}}; save(); out({'type':P+'_done'})
if c is None: err(404)
objs=c['objects']; n=e.get('name')
if op=='list_objects': out({'type':P+'_objects','objects':[{'name':k,'etag':etag(v),'data':v} for k,v in objs.items()]})
cur=objs.get(n) if n else None
if op=='get':
    if cur is None: err(404)
    out({'type':P+'_object','data':cur,'etag':etag(cur)})
im=e.get('if_match'); inm=e.get('if_none_match')
if op=='put':
    if inm=='*' and cur is not None: err(412)
    if im and (cur is None or (im!='*' and etag(cur)!=im)): err(412)
    for k,v in objs.items():
        if k!=n and any(l.strip()=='UID:'+e['uid'] for l in v.splitlines()): err(409,'no-uid-conflict')
    objs[n]=e['data']; save(); out({'type':P+'_stored','etag':etag(e['data']),'created':cur is None})
if op=='delete':
    if n is None:
        del colls[name]; save(); out({'type':P+'_done'})
    if cur is None: err(404)
    if im and etag(cur)!=im: err(412)
    del objs[n]; save(); out({'type':P+'_done'})
err(403)
"#;

/// A complete DAV store in `state` (a JSON file): alice/secret logs in; collections start with
/// one named `default_collection`; put honours If-Match / If-None-Match and refuses a UID used
/// by another object (409 no-uid-conflict); list_objects always includes data.
pub(crate) fn store_policy(
    prefix: &str,
    state: &std::path::Path,
    default_collection: &str,
) -> Vec<Value> {
    let code = STORE_SCRIPT
        .replace("__PREFIX__", prefix)
        .replace("__STATE__", &state.display().to_string())
        .replace("__DEFAULT__", default_collection);
    vec![
        json!({"event_pattern": format!("{prefix}_login"), "handler": {"type":"script","language":"python","code": code}}),
        json!({"event_pattern": format!("{prefix}_request"), "handler": {"type":"script","language":"python","code": code}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    protocol: &str,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: protocol.into(),
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
    protocol: &str,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern": format!("{protocol}_connected"),"handler":{"type":"static","actions":[]}}),
        json!({"event_pattern": format!("{protocol}_response"),"handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: protocol.into(),
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
    .map_err(|_| anyhow::anyhow!("DAV client did not connect"))??;
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

/// The peers' venv bin directory (python, vdirsyncer).
pub(crate) fn peer_bin(tool: &str) -> String {
    let dir = std::env::var("NETGET_DAV_BIN").expect("NETGET_DAV_BIN must name the venv bin directory from tests/server/caldav/install_peers.py (caldav 3.3.1, vdirsyncer 0.21.0, Radicale 3.8.1); this evidence never skips");
    format!("{dir}/{tool}")
}

pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/caldav/peer.py")
}

/// Radicale, unchanged, with plain htpasswd user alice/secret and owner-only rights.
pub(crate) async fn start_radicale() -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        &peer_bin("python"),
        super::real_server::InstallHint { brew: "python (then tests/server/caldav/install_peers.py)", apt: "python3 (then tests/server/caldav/install_peers.py)" },
    )
    .config_file("users", "alice:secret\n")
    .config_file(
        "config",
        "[server]\nhosts = 127.0.0.1:{port}\n[auth]\ntype = htpasswd\nhtpasswd_filename = {dir}/users\nhtpasswd_encryption = plain\n[storage]\nfilesystem_folder = {dir}/collections\n[rights]\ntype = owner_only\n",
    )
    .args(["-m", "radicale", "--config", "{dir}/config"])
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
}

/// A vdirsyncer configuration pairing a local directory with `url`.
pub(crate) fn vdirsyncer_config(
    dir: &std::path::Path,
    kind: &str,
    url: &str,
    ext: &str,
) -> PathBuf {
    let conf = format!(
        "[general]\nstatus_path = \"{d}/status\"\n\n[pair p]\na = \"local\"\nb = \"remote\"\ncollections = [\"from b\"]\nconflict_resolution = \"b wins\"\n\n[storage local]\ntype = \"filesystem\"\npath = \"{d}/local\"\nfileext = \".{ext}\"\n\n[storage remote]\ntype = \"{kind}\"\nurl = \"{url}\"\nusername = \"alice\"\npassword = \"secret\"\n",
        d = dir.display()
    );
    let path = dir.join("config");
    std::fs::write(&path, conf).unwrap();
    path
}

/// Run vdirsyncer with `args`, answering yes to every question it asks.
pub(crate) async fn vdirsyncer(config: &std::path::Path, args: &[&str]) -> (bool, String) {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(peer_bin("vdirsyncer"))
        .arg("--config")
        .arg(config)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let _ = input.write_all(b"y\ny\ny\ny\n").await;
    drop(input);
    let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .expect("vdirsyncer timed out")
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}
