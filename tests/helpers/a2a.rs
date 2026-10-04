//! A2A fixtures: an echo-agent policy, server and client through the shared forms, the pinned
//! a2a-sdk peer and access-log waits.
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

/// "slow" → a working task; "task" → a completed task with an echo artifact; anything else a
/// direct message. GetTask/CancelTask answer about the asked id; "missing" ids are not found.
pub(crate) fn echo_agent_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"a2a_message","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "t=e['text']\n",
            "if 'slow' in t:\n    a={'task':{'state':'working','text':'on it'}}\n",
            "elif 'task' in t:\n    a={'task':{'state':'completed','artifacts':[{'name':'echo','text':'echo: '+t}]}}\n",
            "else:\n    a={'message':{'text':'echo: '+t}}\n",
            "a['type']='a2a_reply'\n",
            "print(json.dumps({'actions':[a]}))\n"
        )}}),
        json!({"event_pattern":"a2a_task_request","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "m=e['method']; tid=e.get('task_id')\n",
            "if m=='ListTasks':\n    a={'tasks':[{'id':'t-1','state':'completed'}]}\n",
            "elif tid and 'missing' in tid:\n    a={'error':{'code':'task_not_found','message':'no such task'}}\n",
            "elif m=='CancelTask':\n    a={'task':{'id':tid,'state':'canceled'}}\n",
            "else:\n    a={'task':{'id':tid,'state':'completed','artifacts':[{'name':'echo','text':'remembered'}]}}\n",
            "a['type']='a2a_reply'\n",
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
        protocol: "a2a".into(),
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
    let handlers = vec![
        json!({"event_pattern":"a2a_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"a2a_response","handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "a2a".into(),
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
    .map_err(|_| anyhow::anyhow!("A2A client did not connect"))??;
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

pub(crate) fn python() -> String {
    std::env::var("NETGET_A2A_PYTHON").expect("NETGET_A2A_PYTHON must name the Python from tests/server/a2a/install_peers.py (a2a-sdk 1.2.1); this evidence never skips")
}
pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/a2a/peer.py")
}
