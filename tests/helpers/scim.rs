//! SCIM fixtures: a store policy, server and client through the shared forms, the pinned
//! python-scim peers and access-log waits.
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

const STORE_SCRIPT: &str = r#"import json,sys,uuid
e=json.load(sys.stdin)['event']
STATE='__STATE__'
try: db=json.load(open(STATE))
except Exception: db={'User':{},'Group':{}}
CORE={'User':'urn:ietf:params:scim:schemas:core:2.0:User','Group':'urn:ietf:params:scim:schemas:core:2.0:Group'}
rt=e['resource_type']; op=e['operation']; store=db[rt]
def save(): json.dump(db,open(STATE,'w'))
def out(a): print(json.dumps({'actions':[a]})); sys.exit()
def res(r): out({'type':'scim_resource','resource':r})
def key(obj,name):
    for k in obj:
        if k.lower()==name.lower(): return k
    return name
if op=='list': out({'type':'scim_resources','resources':list(store.values())})
if op=='create':
    r=e['resource']
    if rt=='User' and any(str(u.get('userName','')).lower()==str(r.get('userName','')).lower() for u in store.values()):
        out({'type':'scim_error','status':409,'scim_type':'uniqueness','detail':'userName is taken'})
    r['id']=uuid.uuid4().hex; store[r['id']]=r; save(); res(r)
rid=e['id']; cur=store.get(rid)
if cur is None: out({'type':'scim_error','status':404,'detail':'%s %s not found'%(rt,rid)})
if op=='get': res(cur)
if op=='delete': del store[rid]; save(); out({'type':'scim_no_content'})
if op=='replace':
    r=e['resource']; r['id']=rid; store[rid]=r; save(); res(r)
def merge(target,value,adding):
    for k,v in value.items():
        tk=key(target,k)
        if adding and isinstance(target.get(tk),list) and isinstance(v,list): target[tk]+= [x for x in v if x not in target[tk]]
        elif isinstance(target.get(tk),dict) and isinstance(v,dict): merge(target[tk],v,adding)
        else: target[tk]=v
for o in e['operations']:
    kind=o['op']; v=o.get('value'); pp=o.get('parsed_path')
    if pp is None: merge(cur,v,kind=='add'); continue
    p=pp['path']; urn=p['schema']
    if p['attribute'] is None:
        k=key(cur,urn)
        if kind=='remove': cur.pop(k,None)
        elif kind=='add' and isinstance(cur.get(k),dict): merge(cur[k],v,True)
        else: cur[k]=v
        continue
    box=cur if (urn is None or urn.lower()==CORE[rt].lower()) else cur.setdefault(key(cur,urn),{})
    attr=key(box,p['attribute']); sub=p['sub_attribute'] or pp.get('sub_attribute_after_filter')
    flt=pp.get('filter')
    if flt is not None:
        items=box.get(attr) or []
        want=lambda x: flt.get('op')=='eq' and str(x.get(key(x,flt['path']['attribute']))).lower()==str(flt['value']).lower()
        hits=[x for x in items if isinstance(x,dict) and want(x)]
        if not hits: out({'type':'scim_error','status':400,'scim_type':'noTarget','detail':'no value matches the filter'})
        if kind=='remove' and not sub: box[attr]=[x for x in items if x not in hits]
        for x in hits:
            if sub:
                if kind=='remove': x.pop(key(x,sub),None)
                else: x[key(x,sub)]=v
            elif kind!='remove' and isinstance(v,dict): merge(x,v,False)
        continue
    if sub:
        inner=box.setdefault(attr,{})
        if not isinstance(inner,dict): out({'type':'scim_error','status':400,'scim_type':'invalidPath','detail':'not a complex attribute'})
        sk=key(inner,sub)
        if kind=='remove': inner.pop(sk,None)
        else: inner[sk]=v
        continue
    if kind=='remove': box.pop(attr,None)
    elif kind=='add' and isinstance(box.get(attr),list):
        add=v if isinstance(v,list) else [v]
        box[attr]+= [x for x in add if x not in box[attr]]
    elif kind=='add' and isinstance(box.get(attr),dict) and isinstance(v,dict): merge(box[attr],v,True)
    else: box[attr]=v
save(); res(cur)
"#;

/// A complete SCIM store for Users and Groups in `state` (a JSON file): create assigns ids and
/// refuses duplicate userNames, replace and patch apply to the stored resource (add, replace,
/// remove on attributes, sub-attributes, extension attributes and `eq` value filters), list
/// returns everything and leaves filtering, sorting and paging to NetGet.
pub(crate) fn store_policy(state: &std::path::Path) -> Vec<Value> {
    let code = STORE_SCRIPT.replace("__STATE__", &state.display().to_string());
    vec![
        json!({"event_pattern":"scim_request","handler":{"type":"script","language":"python","code":code}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if std::env::var("GQL_DEBUG").is_ok() {
                eprintln!("STATUS {m}");
            }
        }
    });
    let id = ServerForm {
        protocol: "scim".into(),
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
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern":"scim_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"scim_response","handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "scim".into(),
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
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("SCIM client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
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
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

/// The python-scim venv's bin directory (scim2, scim2-server).
pub(crate) fn peer_bin(tool: &str) -> String {
    let dir = std::env::var("NETGET_SCIM_BIN").expect("NETGET_SCIM_BIN must name the venv bin directory from tests/server/scim/install_peers.py (scim2-tester 0.5.1, scim2-cli 0.4.0, scim2-server 0.4.0); this evidence never skips");
    format!("{dir}/{tool}")
}

/// scim2-server, unchanged, on a probed loopback port.
pub(crate) async fn start_scim2_server() -> super::E2EResult<super::real_server::RealServer> {
    super::real_server::RealServer::builder(
        &peer_bin("scim2-server"),
        super::real_server::InstallHint {
            brew: "python (then tests/server/scim/install_peers.py)",
            apt: "python3 (then tests/server/scim/install_peers.py)",
        },
    )
    .args(["--hostname", "127.0.0.1", "--port", "{port}"])
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
}
