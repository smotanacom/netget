//! The 9P client against NetGet's own server (every operation, refusals reported rather than
//! fatal, a path longer than one walk), with the server's script copied from
//! `tests/server/ninep/wire_test.rs` since the two test binaries share no modules.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const TREE_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']; p=e['path']
FILES={'/readme.txt':'hello from netget\n','/docs/guide.md':'# Guide\n'}
DIRS={'/':['readme.txt','docs','scratch','many','bin.dat','readonly.txt'],'/docs':['guide.md'],'/scratch':[],'/many':['f%03d'%n for n in range(300)]}
def entry(p):
  if p in DIRS: return {'type':'ninep_entry','kind':'dir','mtime':1700000000}
  if p in FILES: return {'type':'ninep_entry','kind':'file','size':len(FILES[p]),'mtime':1700000000,'owner':'glenda'}
  if p=='/bin.dat': return {'type':'ninep_entry','kind':'file','size':3}
  if p.startswith('/scratch/') or p.startswith('/many/'): return {'type':'ninep_entry','kind':'file','size':0}
  return {'type':'ninep_not_found'}
def child(d,n):
  a=entry(('' if d=='/' else d)+'/'+n); a.pop('type'); a['name']=n
  if a.get('kind') is None: a['kind']='file'
  return a
if t=='ninep_stat': a=entry(p)
elif t=='ninep_list': a={'type':'ninep_listing','entries':[child(p,n) for n in DIRS[p]]}
elif t=='ninep_read': a={'type':'ninep_content','data':'00ff10','encoding':'hex'} if p=='/bin.dat' else {'type':'ninep_content','data':FILES.get(p,'')}
elif p.startswith('/scratch/'): a={'type':'ninep_ok'}
else: a={'type':'ninep_error','message':'permission denied'}
print(json.dumps({'actions':[a]}))"#;

async fn netget_server() -> (AppState, netget::state::ServerId, std::net::SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "9p".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve files".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":TREE_SCRIPT}})]),
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
    (
        state,
        id,
        std::net::SocketAddr::from(([127, 0, 0, 1], addr.port())),
    )
}

pub async fn client(remote: String, ready: Value) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "9p".into(),
        remote_addr: Some(remote),
        instruction: Some("Use the files".into()),
        startup_params: Some(json!({"uname": "glenda"})),
        event_handlers: Some(vec![
            json!({"event_pattern":"ninep_connected","handler":{"type":"static","actions":ready}}),
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

pub async fn wait_log(state: &AppState, id: ClientId, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
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

/// Run one action through the client and return the ninep_result it produced.
pub async fn op(state: &AppState, id: ClientId, action: Value) -> Value {
    let outcome = state
        .send_to_client(id, action.clone(), Duration::from_secs(30))
        .await
        .unwrap();
    match outcome {
        netget::state::client_handles::ClientSendOutcome::Executed { detail } => {
            serde_json::from_str(&detail).unwrap()
        }
        other => panic!("{action}: {other:?}"),
    }
}

#[tokio::test]
async fn every_operation_against_netget() {
    let (server_state, server_id, addr) = netget_server().await;
    let (state, id) = client(addr.to_string(), json!([{"type":"ninep_ls","path":"/"}])).await;
    let event = wait_log(&state, id, r#""op":"ls""#).await;
    for name in ["readme.txt", "docs", "scratch", "many", "bin.dat"] {
        assert!(
            event.contains(&format!(r#""name":"{name}""#)),
            "{name} in {event}"
        );
    }
    let r = op(&state, id, json!({"type":"ninep_cat","path":"/readme.txt"})).await;
    assert_eq!(
        (r["ok"].clone(), r["data"].clone(), r["encoding"].clone()),
        (json!(true), json!("hello from netget\n"), json!("utf8"))
    );
    let r = op(&state, id, json!({"type":"ninep_cat","path":"/bin.dat"})).await;
    assert_eq!(
        (r["data"].clone(), r["encoding"].clone()),
        (json!("00ff10"), json!("hex"))
    );
    let r = op(
        &state,
        id,
        json!({"type":"ninep_stat","path":"/docs/guide.md"}),
    )
    .await;
    assert_eq!(
        r["stat"],
        json!({"name":"guide.md","kind":"file","size":8,"mode":420,"owner":"glenda","mtime":1_700_000_000})
    );
    let r = op(&state, id, json!({"type":"ninep_ls","path":"/many"})).await;
    assert_eq!(r["entries"].as_array().unwrap().len(), 300);
    // 19 elements: walked in two steps.
    let long = format!("/{}readme.txt", "docs/../".repeat(9));
    let r = op(&state, id, json!({"type":"ninep_cat","path":long})).await;
    assert_eq!(r["data"], "hello from netget\n", "{r}");
    // Refusals are results, not failures.
    let r = op(
        &state,
        id,
        json!({"type":"ninep_cat","path":"/missing.txt"}),
    )
    .await;
    assert_eq!(
        (r["ok"].clone(), r["error"].clone()),
        (json!(false), json!("file does not exist"))
    );
    let r = op(
        &state,
        id,
        json!({"type":"ninep_write","path":"/readme.txt","data":"x"}),
    )
    .await;
    assert_eq!(r["error"], "permission denied");
    let r = op(
        &state,
        id,
        json!({"type":"ninep_remove","path":"/readme.txt"}),
    )
    .await;
    assert_eq!(r["error"], "permission denied");
    // Changes under /scratch are accepted and reach the server's handler.
    let r = op(&state, id, json!({"type":"ninep_write","path":"/scratch/new.txt","data":"from the netget client","create":true})).await;
    assert_eq!(
        (r["ok"].clone(), r["bytes_written"].clone()),
        (json!(true), json!(22)),
        "{r}"
    );
    for action in [
        json!({"type":"ninep_write","path":"/scratch/new.txt","data":"ff00","encoding":"hex","append":true}),
        json!({"type":"ninep_mkdir","path":"/scratch/dir"}),
        json!({"type":"ninep_rename","path":"/scratch/new.txt","name":"old.txt"}),
        json!({"type":"ninep_remove","path":"/scratch/old.txt"}),
    ] {
        let r = op(&state, id, action.clone()).await;
        assert_eq!(r["ok"], true, "{action}: {r}");
    }
    let seen: Vec<String> = server_state
        .list_access_logs_for(Some(AccessLogOwner::Server(server_id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    for needle in [
        "from the netget client",
        r#""encoding":"hex""#,
        r#""path":"/scratch/dir""#,
        r#""name":"old.txt""#,
    ] {
        assert!(
            seen.iter().any(|e| e.contains(needle)),
            "the server's handler saw {needle}"
        );
    }
    // A bad action is rejected before anything is sent.
    let rejected = state
        .send_to_client(
            id,
            json!({"type":"ninep_rename","path":"/a","name":"b/c"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(format!("{rejected:?}").contains("Rejected"), "{rejected:?}");
    state.remove_client(id).await;
    server_state.remove_server(server_id).await;
}
