//! The SMPP client against NetGet's own SMSC: bind with credentials (and a refused bind
//! failing creation), a submit accepted with a receipt and a reply, one rejected with the
//! SMSC's status, a long UCS-2 text, enquire_link, and bad actions refused locally.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const SUBMIT_SCRIPT: &str = "import json,sys\ne=json.load(sys.stdin)['event']\nif e['destination_addr'].startswith('1555'):\n  a={'type':'smpp_accept','receipt':'DELIVRD','reply_text':'Got: '+(e.get('text') or '?')}\nelse:\n  a={'type':'smpp_reject','status':'ESME_RINVDSTADR'}\nprint(json.dumps({'actions':[a]}))";

async fn netget_smsc() -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "smpp".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be an SMSC".into()),
        startup_params: Some(json!({"esme_system_id": "esme1", "password": "secret"})),
        event_handlers: Some(vec![json!({"event_pattern":"smpp_submit","handler":{"type":"script","language":"python","code":SUBMIT_SCRIPT}})]),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, format!("127.0.0.1:{}", addr.port()))
}

pub async fn client(
    remote: String,
    params: Value,
    ready: Value,
) -> anyhow::Result<(AppState, ClientId)> {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "smpp".into(),
        remote_addr: Some(remote),
        instruction: Some("Send messages".into()),
        startup_params: Some(params),
        event_handlers: Some(vec![
            json!({"event_pattern":"smpp_bound","handler":{"type":"static","actions":ready}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok((state, id))
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str, secs: u64) -> String {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            if let Some(entry) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| e.contains(needle))
            {
                break entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing {needle:?}"))
}

pub async fn send(state: &AppState, id: ClientId, action: Value) -> ClientSendOutcome {
    state
        .send_to_client(id, action, Duration::from_secs(20))
        .await
        .unwrap()
}

#[tokio::test]
async fn submit_receipt_reply_and_refusals_against_netget() {
    let (server_state, server_id, addr) = netget_smsc().await;
    let refused = client(
        addr.clone(),
        json!({"system_id":"esme1","password":"wrong"}),
        json!([]),
    )
    .await;
    let error = format!(
        "{:#}",
        refused.err().expect("a refused bind fails creation")
    );
    assert!(error.contains("ESME_RINVPASWD"), "{error}");
    let (state, id) = client(
        addr.clone(),
        json!({"system_id":"esme1","password":"secret"}),
        json!([{"type":"smpp_submit","source_addr":"NetGet","destination_addr":"15551230001","text":"hello","registered_delivery":true}]),
    )
    .await
    .unwrap();
    wait_log(&state, id, r#""smsc_system_id":"NETGET""#, 10).await;
    let result = wait_log(&state, id, r#""message_id":"NG00000001""#, 20).await;
    assert!(result.contains(r#""status":"ESME_ROK""#), "{result}");
    let receipt = wait_log(&state, id, r#""is_receipt":true"#, 20).await;
    assert!(
        receipt.contains(r#""id":"NG00000001""#) && receipt.contains(r#""stat":"DELIVRD""#),
        "{receipt}"
    );
    wait_log(&state, id, "Got: hello", 20).await;
    let rejected = send(&state, id, json!({"type":"smpp_submit","source_addr":"NetGet","destination_addr":"44990000000","text":"no"})).await;
    assert!(
        format!("{rejected:?}").contains("ESME_RINVDSTADR"),
        "{rejected:?}"
    );
    let long = "ü".repeat(200);
    let accepted = send(&state, id, json!({"type":"smpp_submit","source_addr":"NetGet","destination_addr":"15551230002","text":long})).await;
    assert!(
        format!("{accepted:?}").contains("NG00000002"),
        "{accepted:?}"
    );
    wait_log(&state, id, &format!("Got: {long}"), 20).await;
    let link = send(&state, id, json!({"type":"smpp_enquire_link"})).await;
    assert!(format!("{link:?}").contains("ESME_ROK"), "{link:?}");
    let bad = send(&state, id, json!({"type":"smpp_submit","source_addr":"x".repeat(21),"destination_addr":"1","text":"t"})).await;
    assert!(matches!(bad, ClientSendOutcome::Rejected { .. }), "{bad:?}");
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
