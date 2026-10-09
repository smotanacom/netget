//! JMAP fixtures: a small mail-store handler, server and client through the shared forms, jmapc
//! (`tests/server/jmap/peer.py`) and a Stalwart server provisioned on loopback
//! (`tests/server/jmap/install_peers.py`).
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::Path, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

/// Two mailboxes and two emails; Email/set creates m-new and cannot find e9; changes are
/// known since s1 only.
pub(crate) const STORE: &str = r#"import json,sys
i=json.load(sys.stdin); e=i['event']; m=e['method']; a=e['arguments']
def out(*x): print(json.dumps({'actions':list(x)})); sys.exit()
def resp(args): out({'type':'jmap_response','arguments':args})
def mbox(i,n,r,t,u): return {'id':i,'name':n,'role':r,'sortOrder':1,'totalEmails':t,'unreadEmails':u,'totalThreads':t,'unreadThreads':u,'isSubscribed':True,'parentId':None}
MB={'inbox':mbox('inbox','Inbox','inbox',2,1),'archive':mbox('archive','Archive','archive',0,0)}
EM={'e1':{'id':'e1','threadId':'t1','mailboxIds':{'inbox':True},'subject':'Welcome','receivedAt':'2026-10-01T09:00:00Z','keywords':{}},
    'e2':{'id':'e2','threadId':'t2','mailboxIds':{'inbox':True},'subject':'Invoice','receivedAt':'2026-10-02T09:00:00Z','keywords':{'$seen':True}},
    'm-new':{'id':'m-new','threadId':'t9','mailboxIds':{'inbox':True},'subject':'Draft','receivedAt':'2026-10-03T09:00:00Z','keywords':{'$draft':True}}}
def pick(o,props): return o if props is None else {k:v for k,v in o.items() if k=='id' or k in props}
if m=='Mailbox/query':
    f=a.get('filter') or {}
    ids=[k for k,v in MB.items() if not f.get('role') or v['role']==f['role']]
    resp({'queryState':'mq1','canCalculateChanges':False,'position':0,'ids':ids,'total':len(ids)})
if m=='Mailbox/get':
    ids=a.get('ids') or list(MB)
    resp({'state':'m1','list':[MB[x] for x in ids if x in MB],'notFound':[x for x in ids if x not in MB]})
if m=='Email/query':
    inbox=(a.get('filter') or {}).get('inMailbox')=='inbox'
    resp({'queryState':'eq1','canCalculateChanges':True,'position':0,'ids':['e2','e1'] if inbox else [],'total':2 if inbox else 0})
if m=='Email/get':
    ids=a.get('ids') or []
    resp({'state':'s1','list':[pick(EM[x],a.get('properties')) for x in ids if x in EM],'notFound':[x for x in ids if x not in EM]})
if m=='Email/set':
    up=a.get('update') or {}
    resp({'oldState':'s1','newState':'s2','created':{k:{'id':'m-new','threadId':'t9','blobId':'b9','size':120} for k in (a.get('create') or {})},
          'updated':{k:None for k in up if k in EM},'notUpdated':{k:{'type':'notFound'} for k in up if k not in EM},'destroyed':[]})
if m=='Email/changes':
    if a.get('sinceState')=='s1': resp({'oldState':'s1','newState':'s2','hasMoreChanges':False,'created':['m-new'],'updated':['e1'],'destroyed':[]})
    out({'type':'jmap_method_error','error_type':'cannotCalculateChanges','description':'sinceState is too old'})
out({'type':'jmap_method_error','error_type':'forbidden','description':m+' is not offered here'})
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
        protocol: "jmap".into(),
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

pub(crate) fn alice() -> Value {
    json!({"accounts": [{"id": "a1", "name": "alice@example.com"}], "users": {"alice@example.com": "secret"}})
}

pub(crate) async fn server_in(state: &AppState, params: Value) -> (ServerId, SocketAddr) {
    server_with(state, Some(script(STORE)), params).await
}

/// The self-signed certificate the server published, written where a client can trust it.
pub(crate) async fn certificate(state: &AppState, id: ServerId, dir: &Path) -> String {
    let pem = state.get_server(id).await.unwrap().protocol_data["certificate_pem"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = dir.join("jmap-ca.pem");
    std::fs::write(&path, pem).unwrap();
    path.to_str().unwrap().to_owned()
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
        protocol: "jmap".into(),
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
    .map_err(|_| anyhow::anyhow!("JMAP client did not connect"))??;
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

/// jmapc against a server; its JSON result.
pub(crate) async fn jmapc(host: &str, user: &str, password: &str, ca: &str) -> Value {
    let python = std::env::var("NETGET_JMAP_PYTHON").expect("NETGET_JMAP_PYTHON must name the Python from tests/server/jmap/install_peers.py (jmapc 0.3.0); this evidence never skips");
    let run = tokio::process::Command::new(python)
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/server/jmap/peer.py"
        ))
        .args([host, user, password, ca])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .expect("jmapc did not finish")
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "jmapc failed:\n{stdout}\n{stderr}");
    serde_json::from_str(stdout.lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("jmapc printed no result ({e}):\n{stdout}\n{stderr}"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_port(port: u16) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Stalwart did not start listening");
}

/// Stalwart 0.16.24 holding alice@test.local, serving JMAP over HTTP on 127.0.0.1 only.
pub(crate) struct Stalwart {
    pub port: u16,
    pub user: String,
    pub password: String,
    child: tokio::process::Child,
    _dir: tempfile::TempDir,
}

impl Stalwart {
    /// Stalwart 0.16 keeps listeners and accounts in its store, set through its own JMAP
    /// registry API. That API is only reachable before any listener exists in recovery mode,
    /// whose listener binds `[::]` on the port given (Stalwart offers no other address), so
    /// provisioning uses a fresh random port and one-time admin password for the seconds it
    /// takes. The provisioned server binds 127.0.0.1 only and resolves DNS against 127.0.0.1,
    /// so it reaches nothing (its web-UI download and spam-rule updates go nowhere).
    pub(crate) async fn start() -> Self {
        let bin = std::env::var("NETGET_JMAP_STALWART").expect("NETGET_JMAP_STALWART must name the stalwart 0.16.24 binary from tests/server/jmap/install_peers.py; this evidence never skips");
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        std::fs::write(
            &config,
            json!({"@type": "RocksDb", "path": dir.path().join("data")}).to_string(),
        )
        .unwrap();
        let admin = format!("p{}", free_port() as u64 * 7919 + std::process::id() as u64);
        let recovery = free_port();
        let port = free_port();
        let mut setup = tokio::process::Command::new(&bin)
            .arg("--config")
            .arg(&config)
            .env("STALWART_RECOVERY_MODE", "true")
            .env("STALWART_RECOVERY_MODE_PORT", recovery.to_string())
            .env("STALWART_RECOVERY_ADMIN", format!("admin:{admin}"))
            .env("STALWART_HOSTNAME", "mail.test")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start stalwart in recovery mode");
        wait_port(recovery).await;
        let http = reqwest::Client::new();
        let call = |calls: Value| {
            let http = http.clone();
            let admin = admin.clone();
            async move {
                let r: Value = http
                    .post(format!("http://127.0.0.1:{recovery}/jmap/"))
                    .basic_auth("admin", Some(admin))
                    .json(&json!({"using": ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"], "methodCalls": calls}))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                r["methodResponses"].clone()
            }
        };
        let session: Value = http
            .get(format!("http://127.0.0.1:{recovery}/jmap/session"))
            .basic_auth("admin", Some(&admin))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let acc = session["primaryAccounts"]["urn:stalwart:jmap"]
            .as_str()
            .unwrap()
            .to_owned();
        let r = call(json!([
            ["x:NetworkListener/set", {"accountId": acc, "create": {"h": {"name": "http-local", "protocol": "http", "bind": {format!("127.0.0.1:{port}"): true}}}}, "0"],
            ["x:Domain/set", {"accountId": acc, "create": {"d": {"name": "test.local"}}}, "1"],
            ["x:Account/set", {"accountId": acc, "create": {"u": {"@type": "User", "name": "alice", "domainId": "#d", "credentials": {"0": {"@type": "Password", "secret": "alice-secret-1"}}}}}, "2"],
            ["x:DnsResolver/set", {"accountId": acc, "update": {"singleton": {"@type": "Custom", "servers": {"0": {"address": "127.0.0.1", "port": 53, "protocol": "udp"}}}}}, "3"],
            ["x:SpamSettings/set", {"accountId": acc, "update": {"singleton": {"enable": false, "spamFilterRulesUrl": null}}}, "4"],
        ]))
        .await;
        for (i, kind) in ["created", "created", "created", "updated", "updated"]
            .iter()
            .enumerate()
        {
            assert!(
                r[i][1]
                    .get(*kind)
                    .is_some_and(|c| !c.as_object().unwrap().is_empty()),
                "provisioning step {i} failed: {}",
                r[i]
            );
        }
        let _ = setup.kill().await;
        let _ = setup.wait().await;
        let child = tokio::process::Command::new(&bin)
            .arg("--config")
            .arg(&config)
            .env("STALWART_HOSTNAME", "mail.test")
            .env("STALWART_PUBLIC_URL", format!("http://127.0.0.1:{port}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start stalwart");
        wait_port(port).await;
        Self {
            port,
            user: "alice@test.local".into(),
            password: "alice-secret-1".into(),
            child,
            _dir: dir,
        }
    }
}

impl Drop for Stalwart {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}
