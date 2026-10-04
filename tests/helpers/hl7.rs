//! HL7 MLLP fixtures: an endpoint policy, server and client through the shared forms, the
//! pinned python-hl7 peer and access-log waits.
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

/// ADT → AA; ORU → AE with an ERR; QRY → AA with a response segment; anything else → AR.
pub(crate) fn endpoint_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"hl7_message","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "t=e['message_type']\n",
            "if t.startswith('ADT'):\n    a={'code':'AA','text':'admitted'}\n",
            "elif t.startswith('ORU'):\n    a={'code':'AE','text':'result held','error':{'code':'207^Application internal error^HL70357','severity':'E','message':'glucose out of range'}}\n",
            "elif t.startswith('QRY'):\n    a={'code':'AA','segments':[{'id':'QRD','fields':['20260101120200','R','I','Q1']},{'id':'PID','fields':['1','','12345^^^HOSP^MR','','Doe^John']}]}\n",
            "else:\n    a={'code':'AR','text':'unsupported message type'}\n",
            "a['type']='hl7_ack'\n",
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
        protocol: "hl7".into(),
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
) -> ClientId {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ClientForm {
        protocol: "hl7".into(),
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

pub(crate) fn quiet_sender() -> Vec<Value> {
    vec![
        json!({"event_pattern":"hl7_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"hl7_ack_received","handler":{"type":"static","actions":[]}}),
    ]
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

pub(crate) fn python() -> String {
    std::env::var("NETGET_HL7_PYTHON").expect("NETGET_HL7_PYTHON must name the Python from tests/server/hl7/install_peers.py (python-hl7 0.4.5); this evidence never skips")
}
pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/hl7/peer.py")
}

pub(crate) fn adt() -> Value {
    json!({"type":"hl7_send","message_type":"ADT^A01^ADT_A01","segments":[{"id":"EVN","fields":["A01","20260101120000"]},{"id":"PID","fields":["1","","12345^^^HOSP^MR","","Doe^John"]}]})
}
