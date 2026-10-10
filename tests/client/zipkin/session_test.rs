//! The Zipkin reporter against NetGet's own collector: a handler-driven report, injected
//! queries in every shape, a refusal, a local refusal of a bad span, and the follow-up bound.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::sync::mpsc;

fn collector_script(store: &Path) -> String {
    format!(
        r#"import json,sys,os
P={store:?}
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
spans=json.load(open(P)) if os.path.exists(P) else []
if t=='zipkin_spans':
  json.dump(spans+e['spans'],open(P,'w')); a={{'type':'zipkin_accept'}}
elif e['endpoint']=='services':
  a={{'type':'zipkin_query_result','result':sorted({{s['localEndpoint']['serviceName'] for s in spans}})}}
elif e['endpoint']=='trace':
  r=[s for s in spans if s['traceId']==e['trace_id']]
  a={{'type':'zipkin_query_result','result':r}} if r else {{'type':'zipkin_reject','status':404,'message':'no such trace'}}
else:
  a={{'type':'zipkin_query_result','result':[]}}
print(json.dumps({{'actions':[a]}}))"#
    )
}

async fn netget_collector(store: &Path) -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "zipkin".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Collect".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":collector_script(store)}})]),
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

/// The two spans a handler reports when the reporter connects.
pub fn connect_report() -> Value {
    json!({"type":"zipkin_report","gzip":true,"spans":[
        {"traceId":"463ac35c9f6413ad48485a3953bb6124","id":"a2fb4a1d1a96d312","name":"checkout","kind":"SERVER",
         "timestamp":1700000000000000u64,"duration":5000,"localEndpoint":{"serviceName":"netget-shop"},"tags":{"cart.items":"3"}},
        {"traceId":"463ac35c9f6413ad48485a3953bb6124","parentId":"a2fb4a1d1a96d312","id":"0020000000000001","name":"charge",
         "kind":"CLIENT","timestamp":1700000000001000u64,"duration":2000,"localEndpoint":{"serviceName":"netget-shop"},
         "remoteEndpoint":{"serviceName":"payments"},"annotations":[{"timestamp":1700000000001500u64,"value":"card-sent"}]}
    ]})
}

/// A reporter whose handler reports `connect_report()` on connect and answers every other
/// event with `then`.
pub async fn client(remote: String, then: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "zipkin".into(),
        remote_addr: Some(remote),
        instruction: Some("Report spans".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"zipkin_connected","handler":{"type":"static","actions":[connect_report()]}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":then}}),
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

/// Inject an action and return the event it raised.
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

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(30), async {
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
async fn reports_and_queries_against_netget() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("spans.json");
    let (server_state, server_id, addr) = netget_collector(&store).await;
    let (state, id) = client(addr, json!([])).await;
    // The connect handler's report reached the collector, gzipped, both spans intact.
    let report = wait_log(&state, id, "zipkin_report_result").await;
    assert!(report.contains(r#""accepted":true"#), "{report}");
    let services = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"services"}),
    )
    .await;
    assert_eq!(services["result"], json!(["netget-shop"]), "{services}");
    let trace = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"trace","trace_id":"463ac35c9f6413ad48485a3953bb6124"}),
    )
    .await;
    assert_eq!(trace["status"], 200, "{trace}");
    assert_eq!(
        trace["result"][1]["remoteEndpoint"]["serviceName"],
        "payments"
    );
    let missing = send(
        &state,
        id,
        json!({"type":"zipkin_query","endpoint":"trace","trace_id":"ff"}),
    )
    .await;
    assert_eq!(
        (
            missing["status"].clone(),
            missing["message"].clone(),
            missing["result"].clone()
        ),
        (json!(404), json!("no such trace"), Value::Null)
    );
    // A bad span, an unknown endpoint and a parameter the endpoint does not take are refused
    // before anything is sent.
    for bad in [
        json!({"type":"zipkin_report","spans":[{"traceId":"XYZ","id":"1"}]}),
        json!({"type":"zipkin_report","spans":[]}),
        json!({"type":"zipkin_query","endpoint":"nope"}),
        json!({"type":"zipkin_query","endpoint":"services","query":{"evil":"1"}}),
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

#[tokio::test]
async fn handler_chain_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let (server_state, server_id, addr) = netget_collector(&dir.path().join("spans.json")).await;
    // Every result asks for another query: without the bound this never ends.
    let (state, id) = client(addr, json!([{"type":"zipkin_query","endpoint":"services"}])).await;
    let count = || async {
        server_state
            .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
            .await
            .iter()
            .filter(|e| {
                serde_json::to_string(e)
                    .unwrap()
                    .contains("zipkin_query_answered")
            })
            .count()
    };
    // The report is depth 1, so queries run at depths 2 through MAX_FOLLOWUP_DEPTH.
    let expected = netget::client::zipkin::MAX_FOLLOWUP_DEPTH - 1;
    tokio::time::timeout(Duration::from_secs(60), async {
        while count().await < expected {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the chain ran");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(count().await, expected);
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
