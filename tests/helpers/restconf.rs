//! RESTCONF fixtures: a car datastore policy (FreeCONF's car module), server and client through
//! the shared forms, the pinned FreeCONF binary (`tests/server/restconf/install_peers.py`) and
//! access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use super::real_server::{InstallHint, RealServer};
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']; STATE='__STATE__'
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
def missing(m): out({'type':'restconf_error','error_tag':'invalid-value','status':404,'message':m})
try: db=json.load(open(STATE))
except Exception: db={'speed':1000,'oilLevel':10,'tire':[{'pos':p,'size':'H15','worn':False,'wear':100,'flat':False} for p in range(4)]}
def save(): json.dump(db,open(STATE,'w'))
def car(): return {'speed':db['speed'],'running':False,'miles':0,'oilLevel':db['oilLevel'],'lastRotation':0,'tire':db['tire']}
if k=='restconf_operation':
    if e['operation']=='car:addOil':
        amount=e.get('input',{}).get('amount',0)
        if amount<=0 or amount>20: out({'type':'restconf_error','error_tag':'operation-failed','message':'invalid oil change level'})
        db['oilLevel']=(0 if e['input'].get('drainFirst') else db['oilLevel'])+amount; save()
        out({'type':'restconf_output','output':{'oilLevel':db['oilLevel']}})
    out({'type':'restconf_output'})
m=e['method']; t=e['target']
if m in ('GET','HEAD'):
    if e['path'] in ('car:',''): out({'type':'restconf_data','data':{'car:'+k2:v for k2,v in car().items()}})
    if len(t)==1 and t[0]['name']=='tire' and t[0].get('keys'):
        hit=[x for x in db['tire'] if str(x['pos'])==t[0]['keys'][0]]
        if not hit: missing('no such tire')
        out({'type':'restconf_data','data':{'car:tire':hit}})
    if len(t)==1 and t[0]['name'] in car(): out({'type':'restconf_data','data':{'car:'+t[0]['name']:car()[t[0]['name']]}})
    missing('no such resource')
if m in ('PUT','PATCH','POST'):
    body=e['body']
    for k2,v in body.items():
        name=k2.split(':')[-1]
        if name=='car' and isinstance(v,dict):
            for a,b in v.items(): db[a.split(':')[-1]]=b
        elif name in ('speed',): db['speed']=v
        elif name=='tire':
            for x in (v if isinstance(v,list) else [v]):
                if m=='POST' and any(y['pos']==x['pos'] for y in db['tire']): out({'type':'restconf_error','error_tag':'data-exists','message':'tire exists'})
                db['tire']=[y for y in db['tire'] if y['pos']!=x['pos']]+[x]
    save(); out({'type':'restconf_ok'})
if m=='DELETE':
    if len(t)==1 and t[0]['name']=='tire' and t[0].get('keys'):
        before=len(db['tire']); db['tire']=[x for x in db['tire'] if str(x['pos'])!=t[0]['keys'][0]]
        if len(db['tire'])==before: out({'type':'restconf_error','error_tag':'data-missing','message':'no such tire'})
        save(); out({'type':'restconf_ok'})
    out({'type':'restconf_error','error_tag':'operation-not-supported','message':'cannot delete that'})
"#;

/// FreeCONF's car module as a datastore in a JSON file: module-level leaves, a tire list keyed
/// by pos, edits merged, and car:addOil (amount 0 < a <= 20) returning the new oil level.
pub(crate) fn policy(state_file: &std::path::Path) -> Vec<Value> {
    let code = POLICY.replace("__STATE__", &state_file.display().to_string());
    vec![
        json!({"event_pattern": "*", "handler": {"type":"script","language":"python","code": code}}),
    ]
}

pub(crate) async fn server_in(state: &AppState, handlers: Vec<Value>) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let id = ServerForm {
        protocol: "restconf".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(json!({"modules": [{"name": "car", "revision": "2023-03-27", "namespace": "freeconf.org/car"}], "operations": ["car:addOil", "car:reset"]})),
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

pub(crate) async fn client_in(state: &AppState, remote: String) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = ["restconf_connected", "restconf_response"]
        .iter()
        .map(|e| json!({"event_pattern": e,"handler":{"type":"static","actions":[]}}))
        .collect();
    let id = ClientForm {
        protocol: "restconf".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(20), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("RESTCONF client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<Value> {
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
                break rows.into_iter().map(|e| e.request).collect();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn freeconf() -> String {
    std::env::var("NETGET_RESTCONF_FREECONF").expect("NETGET_RESTCONF_FREECONF must name the binary from tests/server/restconf/install_peers.py (FreeCONF RESTCONF); this evidence never skips")
}

/// FreeCONF's RESTCONF client against `url`: its JSON lines by step.
pub(crate) async fn freeconf_client(
    url: &str,
) -> (bool, std::collections::HashMap<String, Value>, String) {
    let run = tokio::process::Command::new(freeconf())
        .args(["client", url])
        .output();
    let out = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("FreeCONF client hung")
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let steps = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| (v["step"].as_str().unwrap_or_default().to_owned(), v))
        .collect();
    (out.status.success(), steps, text)
}

/// FreeCONF's car example served over RESTCONF on `{port}`.
pub(crate) async fn start_freeconf_server() -> super::E2EResult<RealServer> {
    RealServer::builder(
        &freeconf(),
        InstallHint {
            brew: "go (then tests/server/restconf/install_peers.py)",
            apt: "golang (then tests/server/restconf/install_peers.py)",
        },
    )
    .args(["serve", "{port}"])
    .ready_when_log_matches("restconf listening")
    .start()
    .await
}
