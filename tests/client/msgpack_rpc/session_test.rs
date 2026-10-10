//! The MessagePack-RPC client against NetGet's own server (its script copied from
//! `tests/server/msgpack_rpc/wire_test.rs`): pipelined calls matched by msgid, an error, the
//! JSON mapping of binary, a notification each way, and a bad action refused locally.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const RPC_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='msgpack_notification':
  a=[{'type':'msgpack_ignore'}]
elif e['method']=='add':
  a=[{'type':'msgpack_result','result':sum(e['params'])}]
elif e['method']=='echo':
  a=[{'type':'msgpack_result','result':e['params']}]
elif e['method']=='notify_me':
  a=[{'type':'msgpack_result','result':'ok'},{'type':'msgpack_notify','method':'progress','params':[100]}]
else:
  a=[{'type':'msgpack_error','error':'no such method: '+e['method']}]
print(json.dumps({'actions':a}))"#;

async fn netget_server() -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "msgpack-rpc".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve RPC".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":RPC_SCRIPT}})]),
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

pub async fn client(remote: String) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "msgpack-rpc".into(),
        remote_addr: Some(remote),
        instruction: Some("Call methods".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id)
}

/// Call a method through the client and return the msgpack_response it produced.
pub async fn call(state: &AppState, id: ClientId, method: &str, params: Value) -> Value {
    match state
        .send_to_client(
            id,
            json!({"type":"msgpack_call","method":method,"params":params}),
            Duration::from_secs(30),
        )
        .await
        .unwrap()
    {
        ClientSendOutcome::Executed { detail } => serde_json::from_str(&detail).unwrap(),
        other => panic!("{method}: {other:?}"),
    }
}

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(e) = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .find(|e| e.contains(needle))
            {
                break e;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no client event containing {needle:?}"))
}

#[tokio::test]
async fn calls_and_notifications_against_netget() {
    let (server_state, server_id, addr) = netget_server().await;
    let (state, id) = client(addr).await;
    // Two calls in flight at once, each answered under its own msgid.
    let (a, b) = tokio::join!(
        call(&state, id, "add", json!([20, 22])),
        call(&state, id, "echo", json!([{"$bin": "deadbeef"}, "x"]))
    );
    assert_eq!(
        (a["result"].clone(), a["error"].clone()),
        (json!(42), Value::Null),
        "{a}"
    );
    assert_eq!(b["result"], json!([{"$bin": "deadbeef"}, "x"]), "{b}");
    assert_ne!(a["msgid"], b["msgid"]);
    let e = call(&state, id, "nope", json!([])).await;
    assert_eq!(
        (e["error"].clone(), e["result"].clone()),
        (json!("no such method: nope"), Value::Null)
    );
    // notify_me answers and notifies back; the notification arrives as an event.
    call(&state, id, "notify_me", json!([])).await;
    wait_log(&state, id, r#""method":"progress""#).await;
    let sent = state
        .send_to_client(
            id,
            json!({"type":"msgpack_notify","method":"log","params":["from client"]}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if server_state
                .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
                .await
                .iter()
                .any(|e| serde_json::to_string(e).unwrap().contains("from client"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(seen.is_ok(), "the server's handler saw the notification");
    let bad = state
        .send_to_client(
            id,
            json!({"type":"msgpack_call","method":"add","params":"not an array"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(bad, ClientSendOutcome::Rejected { .. }), "{bad:?}");
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
