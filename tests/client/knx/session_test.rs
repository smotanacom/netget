//! The KNX/IP client against NetGet's own gateway (its bus script copied from
//! `tests/server/knx/wire_test.rs`): the connect handler's write is fed back and its read
//! answered, both decoded by the configured DPTs; injected telegrams and local refusals.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const BUS_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[{'type':'knx_ignore'}]
if t=='knx_group_read' and e['destination']=='1/2/4': a=[{'type':'knx_group_response','value':21.5}]
elif t=='knx_group_telegram' and e['kind']=='write' and e['destination']=='1/2/3':
  a=[{'type':'knx_group_write','group_address':'1/2/10','value':e['value']}]
print(json.dumps({'actions':a}))"#;

pub fn group_types() -> Value {
    json!({"1/2/3": "1", "1/2/4": "9.001", "1/2/10": "1"})
}

async fn netget_gateway() -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "knx".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be the bus".into()),
        startup_params: Some(json!({"group_types": group_types()})),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":BUS_SCRIPT}})]),
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

/// A client whose connect handler switches 1/2/3 on and reads 1/2/4.
pub async fn client(remote: String) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "knx".into(),
        remote_addr: Some(remote),
        instruction: Some("Run the lights".into()),
        startup_params: Some(json!({"group_types": group_types()})),
        event_handlers: Some(vec![
            json!({"event_pattern":"knx_connected","handler":{"type":"static","actions":[
                {"type":"knx_group_write","group_address":"1/2/3","value":true},
                {"type":"knx_group_read","group_address":"1/2/4"}]}}),
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

/// Every knx_telegram the client raised.
pub async fn telegrams(state: &AppState, id: ClientId) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == "knx_telegram")
        .map(|e| e["request"].clone())
        .collect()
}

#[tokio::test]
async fn against_netget_gateway() {
    let (server_state, server_id, addr) = netget_gateway().await;
    let (state, id) = client(addr).await;
    let (feedback, response) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let t = telegrams(&state, id).await;
            let fb = t.iter().find(|t| t["destination"] == "1/2/10").cloned();
            let r = t.iter().find(|t| t["kind"] == "response").cloned();
            if let (Some(fb), Some(r)) = (fb, r) {
                break (fb, r);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("feedback and response");
    assert_eq!(
        (feedback["kind"].clone(), feedback["value"].clone()),
        (json!("write"), json!(true))
    );
    assert_eq!(feedback["source"], "1.1.250");
    assert_eq!(
        (response["destination"].clone(), response["value"].clone()),
        (json!("1/2/4"), json!(21.5))
    );
    // Injected: a write by an explicit DPT, then refusals before anything is sent.
    let sent = state
        .send_to_client(
            id,
            json!({"type":"knx_group_write","group_address":"3/0/1","value":42,"dpt":"5"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    for bad in [
        json!({"type":"knx_group_write","group_address":"3/0/2","value":1}),
        json!({"type":"knx_group_write","group_address":"40/0/1","value":true,"dpt":"1"}),
        json!({"type":"knx_group_write","group_address":"1/2/4","value":"warm"}),
    ] {
        let out = state
            .send_to_client(id, bad.clone(), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            matches!(out, ClientSendOutcome::Rejected { .. }),
            "{bad}: {out:?}"
        );
    }
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
