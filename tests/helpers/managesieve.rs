//! ManageSieve fixtures: a script-keeping policy, server and client through the shared forms,
//! the pinned Dovecot/Pigeonhole and sievelib (`tests/server/managesieve/install_peers.py`) and
//! access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use super::real_server::{InstallHint, RealServer};
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
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']; STATE='__STATE__'
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
def no(m, code=None):
    a={'type':'managesieve_no','message':m}
    if code: a['code']=code
    out(a)
try: db=json.load(open(STATE))
except Exception: db={'scripts':{},'active':None}
def save(): json.dump(db,open(STATE,'w'))
if k=='managesieve_auth':
    out({'type':'managesieve_ok'} if (e['user'],e['password'])==('alice','secret') else {'type':'managesieve_no','message':'bad credentials'})
c=e['command']; s=db['scripts']; n=e.get('name')
if c=='LISTSCRIPTS': out({'type':'managesieve_scripts','scripts':[{'name':x,'active':x==db['active']} for x in sorted(s)]})
if c=='GETSCRIPT':
    if n not in s: no('There is no script by that name','NONEXISTENT')
    out({'type':'managesieve_script','script':s[n]})
if c in ('PUTSCRIPT','CHECKSCRIPT'):
    if 'bogus' in e['script']: no("line 1: error: unknown command 'bogus'")
    if c=='PUTSCRIPT':
        s[n]=e['script']; save()
    out({'type':'managesieve_ok'})
if c=='SETACTIVE':
    if n and n not in s: no('There is no script by that name','NONEXISTENT')
    db['active']=n or None; save(); out({'type':'managesieve_ok'})
if c=='DELETESCRIPT':
    if n not in s: no('There is no script by that name','NONEXISTENT')
    if n==db['active']: no('You may not delete an active script','ACTIVE')
    del s[n]; save(); out({'type':'managesieve_ok'})
if c=='RENAMESCRIPT':
    if n not in s: no('There is no script by that name','NONEXISTENT')
    if e['new_name'] in s: no('A script with that name already exists','ALREADYEXISTS')
    s[e['new_name']]=s.pop(n)
    if db['active']==n: db['active']=e['new_name']
    save(); out({'type':'managesieve_ok'})
if c=='HAVESPACE':
    if e['size']>100000: no('Scripts are limited to 100000 bytes','QUOTA/MAXSIZE')
    out({'type':'managesieve_ok'})
no('unexpected')
"#;

/// alice/secret may log in; scripts live in a JSON file; a script containing "bogus" is refused
/// with a syntax error; the active script cannot be deleted; HAVESPACE over 100000 is
/// QUOTA/MAXSIZE; the usual NONEXISTENT and ALREADYEXISTS.
pub(crate) fn policy(state_file: &std::path::Path) -> Vec<Value> {
    let code = POLICY.replace("__STATE__", &state_file.display().to_string());
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": code}}),
    ]
}

pub(crate) async fn server_in(state: &AppState, handlers: Vec<Value>) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "managesieve".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
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

/// A ManageSieve client whose events are answered with nothing, so only injected actions run.
pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["managesieve_connected", "managesieve_response"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "managesieve".into(),
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
    .map_err(|_| anyhow::anyhow!("ManageSieve client did not connect"))??;
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
    std::env::var(var).unwrap_or_else(|_| panic!("{var} must name what tests/server/managesieve/install_peers.py installed (Dovecot 2.4.5 with Pigeonhole, sievelib 1.5.0); this evidence never skips"))
}

pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/managesieve/peer.py")
}

/// sievelib's run against a server: its JSON lines by step.
pub(crate) async fn sievelib(
    port: u16,
    user: &str,
    password: &str,
) -> std::collections::HashMap<String, Value> {
    let run = tokio::process::Command::new(tool("NETGET_MANAGESIEVE_PYTHON"))
        .arg(peer_script())
        .args([port.to_string(), user.into(), password.into()])
        .output();
    let out = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("sievelib hung")
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| (v["step"].as_str().unwrap_or_default().to_owned(), v))
        .collect()
}

/// Dovecot 2.4.5 serving ManageSieve on `port` for any user with password "secret". Its runtime
/// directory is a short path under /tmp, since its UNIX sockets must fit sun_path.
pub(crate) async fn start_dovecot() -> super::E2EResult<(RealServer, tempfile::TempDir)> {
    let run = tempfile::Builder::new().prefix("ms-").tempdir_in("/tmp")?;
    let user = String::from_utf8(std::process::Command::new("id").arg("-un").output()?.stdout)?
        .trim()
        .to_owned();
    let group = String::from_utf8(std::process::Command::new("id").arg("-gn").output()?.stdout)?
        .trim()
        .to_owned();
    let r = run.path().display().to_string();
    let config = format!(
        r#"dovecot_config_version = 2.4.0
dovecot_storage_version = 2.4.0
base_dir = {r}/base
state_dir = {r}/state
log_path = /dev/stderr
info_log_path = /dev/stderr
protocols = sieve
listen = 127.0.0.1
default_internal_user = {user}
default_internal_group = {group}
default_login_user = {user}
default_vsz_limit = 1024G
mail_driver = maildir
mail_home = {r}/home/%{{user}}
mail_path = ~/Maildir
auth_mechanisms = plain
auth_allow_cleartext = yes
ssl = no
passdb static {{
  password = secret
}}
userdb static {{
  fields {{
    uid = {user}
    gid = {group}
    home = {r}/home/%{{user}}
  }}
}}
service managesieve-login {{
  chroot =
  inet_listener sieve {{
    port = {{port}}
  }}
}}
service anvil {{
  chroot =
}}
sieve_script personal {{
  driver = file
  path = ~/sieve
  active_path = ~/.dovecot.sieve
}}
"#
    );
    let server = RealServer::builder(
        &tool("NETGET_MANAGESIEVE_DOVECOT"),
        InstallHint {
            brew: "openssl@3 pkgconf (then tests/server/managesieve/install_peers.py)",
            apt: "libssl-dev pkg-config (then tests/server/managesieve/install_peers.py)",
        },
    )
    .config_file("dovecot.conf", &config)
    .args(["-F", "-c", "{dir}/dovecot.conf"])
    .ready_when_log_matches("starting up")
    .start()
    .await?;
    Ok((server, run))
}
