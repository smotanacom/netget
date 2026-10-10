//! The RadSec client against NetGet's own RadSec server over mutual TLS: a handler chain where
//! the accounting session is named from the Access-Accept's Reply-Message, an injected
//! rejected user, and the two refusals that matter — a server whose certificate does not chain
//! to ca_file, and no ca_file at all.
use crate::helpers::radsec_pki::{make, Pki};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// On connect, authenticate alice; when accepted, start accounting for a session named after
/// the Reply-Message the server sent.
pub fn chain() -> Vec<Value> {
    vec![
        json!({"event_pattern":"radius_connected","handler":{"type":"static","actions":[
            {"type":"radius_access_request","user_name":"alice","password":"wonderland"}]}}),
        json!({"event_pattern":"radius_access_accept","handler":{"type":"script","language":"python","code":
            "import json,sys\ne=json.load(sys.stdin)['event']\nsid='s-'+e.get('reply_message','none').replace(' ','-')\nprint(json.dumps({'actions':[{'type':'radius_accounting_request','status_type':'Start','session_id':sid,'user_name':e['user_name']}]}))"}}),
        json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
    ]
}

pub async fn client(
    remote: String,
    params: Value,
    handlers: Vec<Value>,
) -> anyhow::Result<(AppState, ClientId)> {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "radsec".into(),
        remote_addr: Some(remote),
        instruction: Some("Authenticate users".into()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    Ok((state, id))
}

pub fn client_params(pki: &Pki) -> Value {
    json!({"ca_file": pki.ca, "certificate_file": pki.client_cert, "private_key_file": pki.client_key,
           "server_name": "localhost", "timeout_ms": 10000})
}

/// The first event of this type, waiting for it.
pub async fn wait_for(state: &AppState, id: ClientId, event_type: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let found = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .filter(|e| e["event_type"] == event_type)
                .last();
            if let Some(e) = found {
                return e["request"].clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {event_type} event"))
}

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='radius_accounting_request': a=[{'type':'send_accounting_response'}]
elif e.get('user_name')=='alice' and e.get('password')=='wonderland': a=[{'type':'send_access_accept','reply_message':'welcome alice'}]
else: a=[{'type':'send_access_reject','reply_message':'go away'}]
print(json.dumps({'actions':a}))"#;

async fn netget_server(pki: &Pki) -> String {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let sid = ServerForm {
        protocol: "radsec".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Authenticate".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}})]),
        startup_params: Some(json!({"certificate_file": pki.server_cert, "private_key_file": pki.server_key, "ca_file": pki.ca})),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(sid).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The server lives as long as the process; its state is leaked deliberately.
    std::mem::forget(state);
    format!("127.0.0.1:{port}")
}

#[tokio::test]
async fn against_netget_radsec_server() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    let server = netget_server(&pki).await;
    let (state, id) = client(server.clone(), client_params(&pki), chain())
        .await
        .unwrap();
    let accept = wait_for(&state, id, "radius_access_accept").await;
    assert_eq!(accept["reply_message"], "welcome alice", "{accept}");
    let acct = wait_for(&state, id, "radius_accounting_response").await;
    assert_eq!(acct["session_id"], "s-welcome-alice", "{acct}");
    // Injected: a user the server rejects, on the same connection.
    let sent = state
        .send_to_client(
            id,
            json!({"type":"radius_access_request","user_name":"mallory","password":"guess"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let reject = wait_for(&state, id, "radius_access_reject").await;
    assert_eq!(
        (&reject["user_name"], &reject["reply_message"]),
        (&json!("mallory"), &json!("go away"))
    );
    // A server certificate that does not chain to ca_file: no connection.
    let mut wrong = client_params(&pki);
    wrong["ca_file"] = json!(pki.stranger_cert);
    assert!(
        client(server.clone(), wrong, vec![]).await.is_err(),
        "connected to an unverified server"
    );
    // No ca_file at all: refused before connecting.
    let mut none = client_params(&pki);
    none.as_object_mut().unwrap().remove("ca_file");
    let e = client(server, none, vec![])
        .await
        .err()
        .expect("refused without ca_file");
    assert!(format!("{e:#}").contains("ca_file"), "{e:#}");
}
