//! The ClickHouse client against NetGet's own server (its SQL script copied from
//! `tests/server/clickhouse/wire_test.rs`, since the test binaries share no modules): a typed
//! result, DDL, an INSERT the server sees, an exception, a refused login, and rows that do not
//! fit the table refused with the insert ended cleanly.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const SQL_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='clickhouse_insert_data':
  a={'type':'clickhouse_ok'} if len(e['rows'])<=10 else {'type':'clickhouse_exception','code':241,'message':'too many rows'}
else:
  q=' '.join(e['query'].split()).rstrip(';'); ql=q.lower()
  if ql.startswith('select') and 'from events' in ql:
    a={'type':'clickhouse_result','columns':[{'name':'id','type':'UInt32'},{'name':'name','type':'String'},{'name':'score','type':'Nullable(Float64)'},{'name':'day','type':'Date'},{'name':'at','type':'DateTime'},{'name':'ok','type':'Bool'},{'name':'delta','type':'Int64'}],'rows':[[1,'alpha',1.5,'2024-01-02','2024-01-02 03:04:05',True,-7],[2,'beta',None,'2024-12-31','2024-12-31 23:59:59',False,9007199254740993]]}
  elif ql.startswith('insert into events'):
    a={'type':'clickhouse_insert','columns':[{'name':'id','type':'UInt32'},{'name':'name','type':'String'}]}
  elif ql.startswith('create') or ql.startswith('drop'):
    a={'type':'clickhouse_ok'}
  else:
    a={'type':'clickhouse_exception','code':60,'message':'Table default.missing does not exist'}
print(json.dumps({'actions':[a]}))"#;

async fn netget_server() -> (AppState, netget::state::ServerId, String) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "clickhouse".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Answer SQL".into()),
        startup_params: Some(json!({"user": "analyst", "password": "s3cret"})),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":SQL_SCRIPT}})]),
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

pub async fn client(remote: String, params: Value) -> anyhow::Result<(AppState, ClientId)> {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "clickhouse".into(),
        remote_addr: Some(remote),
        instruction: Some("Run SQL".into()),
        startup_params: Some(params),
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
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !state.has_client_handle(id).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok((state, id))
}

/// Run one action and return the clickhouse_result it produced.
pub async fn run(state: &AppState, id: ClientId, action: Value) -> Value {
    match state
        .send_to_client(id, action.clone(), Duration::from_secs(60))
        .await
        .unwrap()
    {
        ClientSendOutcome::Executed { detail } => serde_json::from_str(&detail).unwrap(),
        other => panic!("{action}: {other:?}"),
    }
}

pub fn connected(state: &AppState, id: ClientId) -> impl std::future::Future<Output = String> + '_ {
    async move {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(e) = state
                    .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                    .await
                    .iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .find(|e| e.contains("clickhouse_connected") || e.contains("server_name"))
                {
                    break e;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a clickhouse_connected event")
    }
}

#[tokio::test]
async fn queries_and_inserts_against_netget() {
    let (server_state, server_id, addr) = netget_server().await;
    let refused = client(addr.clone(), json!({"user":"analyst","password":"nope"})).await;
    let error = format!(
        "{:#}",
        refused.err().expect("a refused login fails creation")
    );
    assert!(
        error.contains("516") && error.contains("Authentication failed"),
        "{error}"
    );
    let (state, id) = client(addr, json!({"user":"analyst","password":"s3cret"}))
        .await
        .unwrap();
    let hello = connected(&state, id).await;
    assert!(hello.contains(r#""revision":54429"#), "{hello}");
    let r = run(
        &state,
        id,
        json!({"type":"clickhouse_query","query":"SELECT * FROM events"}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(
        r["rows"][1],
        json!([
            2,
            "beta",
            null,
            "2024-12-31",
            "2024-12-31 23:59:59",
            false,
            9007199254740993i64
        ])
    );
    assert_eq!(
        r["columns"][2],
        json!({"name":"score","type":"Nullable(Float64)"})
    );
    let r = run(
        &state,
        id,
        json!({"type":"clickhouse_query","query":"CREATE TABLE t (x UInt8) ENGINE = Memory"}),
    )
    .await;
    assert_eq!(
        (r["ok"].clone(), r.get("rows").is_none()),
        (json!(true), true),
        "{r}"
    );
    let r = run(&state, id, json!({"type":"clickhouse_insert","query":"INSERT INTO events (id, name) VALUES","rows":[[5,"five"],[6,"six"]]})).await;
    assert_eq!(
        (r["ok"].clone(), r["rows_written"].clone()),
        (json!(true), json!(2)),
        "{r}"
    );
    let seen = server_state
        .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
        .await
        .iter()
        .any(|e| {
            serde_json::to_string(e)
                .unwrap()
                .contains(r#""rows":[[5,"five"],[6,"six"]]"#)
        });
    assert!(seen, "the server's handler saw the inserted rows");
    let r = run(
        &state,
        id,
        json!({"type":"clickhouse_query","query":"SELECT * FROM missing"}),
    )
    .await;
    assert_eq!(r["exception"]["code"], 60, "{r}");
    // Rows that do not fit the table's columns are refused; the session stays usable.
    let bad = state
        .send_to_client(id, json!({"type":"clickhouse_insert","query":"INSERT INTO events (id, name) VALUES","rows":[["not a number","x"]]}), Duration::from_secs(30))
        .await;
    assert!(bad.is_err(), "{bad:?}");
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
