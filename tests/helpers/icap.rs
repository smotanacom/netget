//! ICAP fixtures: a filtering policy, server and client through the shared forms, the pinned
//! c-icap peer (server config writer included) and access-log waits.
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

/// Block bodies containing EICAR, redact "secret" in uploads, pass everything else.
pub(crate) fn filter_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"icap_request","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "b=e.get('body_text') or ''\n",
            "if 'EICAR' in b:\n    a={'verdict':'block','body_text':'Blocked by NetGet'}\n",
            "elif e['method']=='REQMOD' and 'secret' in b:\n",
            "    r=e['http_request']\n",
            "    a={'verdict':'modify','http_request':{'method':r['method'],'uri':r['uri'],'headers':[['X-Redacted','1']]},'body_text':'[redacted]'}\n",
            "else:\n    a={'verdict':'no_modification'}\n",
            "a['type']='icap_response'\n",
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
        protocol: "icap".into(),
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

pub(crate) async fn client_in(state: &AppState, remote: String) -> ClientId {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern":"icap_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"icap_response","handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "icap".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            assert!(!matches!(
                state.get_client(id).await.map(|c| c.status),
                Some(ClientStatus::Error(_))
            ));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    id
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

/// The c-icap install prefix from tests/server/icap/install_peers.sh.
pub(crate) fn c_icap_prefix() -> PathBuf {
    PathBuf::from(std::env::var("NETGET_C_ICAP").expect("NETGET_C_ICAP must name the c-icap 0.6.5 prefix from tests/server/icap/install_peers.sh; this evidence never skips"))
}

/// Start c-icap with its echo service on a free loopback port; returns (child, port, dir).
pub(crate) async fn c_icap_server() -> (tokio::process::Child, u16, tempfile::TempDir) {
    let prefix = c_icap_prefix();
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let d = dir.path().display();
    let lib = prefix.join("lib/c_icap");
    let conf = format!(
        "PidFile {d}/c-icap.pid\nCommandsSocket {d}/c-icap.ctl\nTimeout 30\nPort 127.0.0.1:{port}\nServerLog {d}/server.log\nAccessLog {d}/access.log\nModulesDir {lib}\nServicesDir {lib}\nTmpDir {d}\nStartServers 1\nMaxServers 2\nMinSpareThreads 2\nMaxSpareThreads 4\nThreadsPerChild 4\nService echo srv_echo.so\n",
        lib = lib.display()
    );
    std::fs::write(dir.path().join("c-icap.conf"), conf).unwrap();
    let child = tokio::process::Command::new(prefix.join("bin/c-icap"))
        .arg("-N")
        .arg("-f")
        .arg(dir.path().join("c-icap.conf"))
        .kill_on_drop(true)
        .spawn()
        .expect("start c-icap");
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    (child, port, dir)
}
