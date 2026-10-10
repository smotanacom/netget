//! The Consul client against NetGet's own agent (a small store script; the server suite's lives
//! in another test target): a handler-driven put → get → register chain, injected catalog
//! queries and local refusals.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// After connecting: write app/config, then read it back and register a service once the
/// write is confirmed.
const CHAIN: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; a=[]
if e['operation']=='consul_kv_put' and e['result']==True:
  a=[{'type':'consul_kv_get','key':'app/config'},{'type':'consul_register_service','name':'netget-web','id':'nw1','port':8080,'address':'127.0.0.7','tags':['from-netget']}]
print(json.dumps({'actions':a}))"#;

pub async fn client(remote: String) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "consul".into(),
        remote_addr: Some(remote),
        instruction: Some("Configure the service".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"consul_connected","handler":{"type":"static","actions":[
                {"type":"consul_kv_put","key":"app/config","value":"from netget","flags":5}]}}),
            json!({"event_pattern":"consul_response","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
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

/// Every consul_response, newest first.
pub async fn responses(state: &AppState, id: ClientId) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == "consul_response")
        .map(|e| e["request"].clone())
        .collect()
}

pub async fn wait_for(state: &AppState, id: ClientId, operation: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(r) = responses(state, id)
                .await
                .into_iter()
                .find(|r| r["operation"] == operation)
            {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {operation} response"))
}

pub async fn send(state: &AppState, id: ClientId, action: Value) -> Value {
    match state
        .send_to_client(id, action.clone(), Duration::from_secs(30))
        .await
        .unwrap()
    {
        ClientSendOutcome::Executed { detail } => serde_json::from_str(&detail).unwrap(),
        other => panic!("{action}: {other:?}"),
    }
}

const STORE: &str = r#"import json,sys,os
P=STOREPATH
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
st=json.load(open(P)) if os.path.exists(P) else {'kv':{},'svc':{}}
kv=st['kv']; svc=st['svc']
def save(): json.dump(st,open(P,'w'))
ok={'type':'consul_ok'}
if t=='consul_kv_read':
  k=e['key']; a={'type':'consul_kv_entries','entries':[dict(key=k,value=kv[k][0],flags=kv[k][1])]} if k in kv else {'type':'consul_not_found'}
elif t=='consul_kv_write': kv[e['key']]=[e['value'],e['flags']]; save(); a=ok
elif t=='consul_register': svc[e['service']['id']]=e['service']; save(); a=ok
elif t=='consul_catalog':
  a={'type':'consul_services','services':{s['name']:s['tags'] or [] for s in svc.values()}} if e['endpoint']=='services' else {'type':'consul_instances','instances':[s for s in svc.values() if s['name']==e['name']]}
else: a=ok
print(json.dumps({'actions':[a]}))"#;

#[tokio::test]
async fn against_netget_agent() {
    let dir = tempfile::tempdir().unwrap();
    let script = STORE.replace("STOREPATH", &format!("{:?}", dir.path().join("c.json")));
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let sid = ServerForm {
        protocol: "consul".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Agent".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":script}})]),
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
    let (cstate, cid) = client(format!("127.0.0.1:{port}")).await;
    let got = wait_for(&cstate, cid, "consul_kv_get").await;
    assert_eq!(got["result"][0]["value"], "from netget", "{got}");
    assert_eq!(got["result"][0]["flags"], 5);
    wait_for(&cstate, cid, "consul_register_service").await;
    let svc = send(
        &cstate,
        cid,
        json!({"type":"consul_catalog","endpoint":"service","name":"netget-web"}),
    )
    .await;
    assert_eq!(svc["result"][0]["port"], 8080, "{svc}");
    let missing = send(&cstate, cid, json!({"type":"consul_kv_get","key":"nope"})).await;
    assert_eq!(missing["status"], 404);
    for bad in [
        json!({"type":"consul_kv_get","key":""}),
        json!({"type":"consul_catalog","endpoint":"service"}),
        json!({"type":"consul_kv_put","key":"k","value":"zz","encoding":"hex"}),
    ] {
        let out = cstate
            .send_to_client(cid, bad.clone(), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(
            matches!(out, ClientSendOutcome::Rejected { .. }),
            "{bad}: {out:?}"
        );
    }
    cstate.remove_client(cid).await;
    state.remove_server(sid).await;
}
