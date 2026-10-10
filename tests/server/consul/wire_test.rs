//! Consul agent API over raw HTTP: KV get (single, recurse, keys, raw), put with flags and
//! check-and-set, delete, catalog and health built from the handler's instances, service
//! registration, the index header, and the refusals and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// A KV store and service registry kept in a JSON file — NetGet stores nothing. Keys under
/// locked/ are refused with 403.
pub fn store_script(store: &Path) -> String {
    format!(
        r#"import json,sys,os
P={store:?}
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
st=json.load(open(P)) if os.path.exists(P) else {{'kv':{{}},'svc':{{}},'n':1}}
kv=st['kv']; svc=st['svc']
def save(): json.dump(st,open(P,'w'))
def ent(k): x=kv[k]; return dict(key=k,value=x['value'],encoding=x['enc'],flags=x['flags'],modify_index=x['idx'])
def inst(l): return {{'type':'consul_instances','instances':l}}
ok={{'type':'consul_ok'}}
if t=='consul_kv_read':
  k=e['key']
  hits=sorted(x for x in kv if x.startswith(k)) if (e['recurse'] or e['keys_only']) else ([k] if k in kv else [])
  a={{'type':'consul_kv_entries','entries':[ent(x) for x in hits]}} if hits else {{'type':'consul_not_found'}}
elif t=='consul_kv_write':
  cur=kv.get(e['key']); c=e['cas']
  if e['key'].startswith('locked/'): a={{'type':'consul_error','status':403,'message':'Permission denied'}}
  elif c is not None and ((c==0 and cur) or (c!=0 and (not cur or cur['idx']!=c))): a={{'type':'consul_refuse'}}
  else:
    st['n']+=1; kv[e['key']]={{'value':e['value'],'enc':e['value_encoding'],'flags':e['flags'],'idx':st['n']}}; save(); a=ok
elif t=='consul_kv_delete':
  for x in [x for x in kv if (x.startswith(e['key']) if e['recurse'] else x==e['key'])]: del kv[x]
  save(); a=ok
elif t=='consul_catalog':
  ep=e['endpoint']
  if ep=='services':
    a={{'type':'consul_services','services':{{s['name']:sorted(set(sum([x.get('tags') or [] for x in svc.values() if x['name']==s['name']],[]))) for s in svc.values()}}}}
  elif ep=='agent_services': a=inst(list(svc.values()))
  else: a=inst([s for s in svc.values() if s['name']==e['name']])
else:
  if e['operation']=='register': svc[e['service']['id']]=e['service']
  else: svc.pop(e['id'],None)
  save(); a=ok
print(json.dumps({{'actions':[a]}}))"#
    )
}

pub fn handlers(store: &Path) -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":store_script(store)}}),
    ]
}

pub async fn start(handlers: Vec<Value>) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "consul".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be a Consul agent".into()),
        event_handlers: Some(handlers),
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
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

/// One HTTP exchange: (status, lower-cased head, body).
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: consul\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    s.write_all(&req).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut out))
        .await
        .expect("deadline")
        .unwrap();
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a head");
    let head = String::from_utf8_lossy(&out[..split]).to_ascii_lowercase();
    (
        head[9..12].parse().unwrap(),
        head,
        out[split + 4..].to_vec(),
    )
}

fn json_of(b: &[u8]) -> Value {
    serde_json::from_slice(b).unwrap_or_else(|_| json!(String::from_utf8_lossy(b)))
}

fn index(head: &str) -> u64 {
    head.lines()
        .find_map(|l| l.strip_prefix("x-consul-index: "))
        .and_then(|v| v.trim().parse().ok())
        .expect("an index header")
}

#[tokio::test]
async fn kv_catalog_and_registration() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, _id, addr) = start(handlers(&dir.path().join("consul.json"))).await;
    let (status, head, body) = http(addr, "PUT", "/v1/kv/app/config?flags=7", b"hello world").await;
    assert_eq!((status, json_of(&body)), (200, json!(true)));
    let written = index(&head);
    let (status, head, body) = http(addr, "GET", "/v1/kv/app/config", b"").await;
    assert_eq!(status, 200);
    assert!(index(&head) >= written);
    let e = &json_of(&body)[0];
    assert_eq!(e["Key"], "app/config");
    assert_eq!(
        e["Value"], "aGVsbG8gd29ybGQ=",
        "base64 of the bytes written"
    );
    assert_eq!(e["Flags"], 7);
    let modify = e["ModifyIndex"].as_u64().unwrap();
    // Raw, keys, recurse.
    assert_eq!(
        http(addr, "GET", "/v1/kv/app/config?raw", b"").await.2,
        b"hello world"
    );
    http(addr, "PUT", "/v1/kv/app/bin", &[0, 255, 1]).await;
    assert_eq!(
        json_of(&http(addr, "GET", "/v1/kv/app/?keys", b"").await.2),
        json!(["app/bin", "app/config"])
    );
    let all = json_of(&http(addr, "GET", "/v1/kv/app/?recurse", b"").await.2);
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert_eq!(
        all[0]["Value"], "AP8B",
        "binary values survive as hex to the handler and back"
    );
    // Check-and-set: a stale index is refused with false, the current one accepted.
    assert_eq!(
        json_of(
            &http(addr, "PUT", "/v1/kv/app/config?cas=1", b"nope")
                .await
                .2
        ),
        json!(false)
    );
    let cas = format!("/v1/kv/app/config?cas={modify}");
    assert_eq!(
        json_of(&http(addr, "PUT", &cas, b"next").await.2),
        json!(true)
    );
    // The handler's refusal, a delete and a 404.
    let (status, _, body) = http(addr, "PUT", "/v1/kv/locked/x", b"y").await;
    assert_eq!(
        (status, String::from_utf8_lossy(&body).to_string()),
        (403, "Permission denied".into())
    );
    assert_eq!(
        json_of(&http(addr, "DELETE", "/v1/kv/app/bin", b"").await.2),
        json!(true)
    );
    assert_eq!(http(addr, "GET", "/v1/kv/app/bin", b"").await.0, 404);
    // Services: registered, listed with tags, looked up in the catalog and health.
    let reg =
        json!({"ID": "web1", "Name": "web", "Port": 8080, "Tags": ["v1"], "Address": "10.0.0.5"});
    assert_eq!(
        http(
            addr,
            "PUT",
            "/v1/agent/service/register",
            reg.to_string().as_bytes()
        )
        .await
        .0,
        200
    );
    assert_eq!(
        json_of(&http(addr, "GET", "/v1/catalog/services", b"").await.2),
        json!({"web": ["v1"]})
    );
    let cat = json_of(&http(addr, "GET", "/v1/catalog/service/web", b"").await.2);
    assert_eq!(
        (
            cat[0]["ServiceID"].clone(),
            cat[0]["ServicePort"].clone(),
            cat[0]["Node"].clone()
        ),
        (json!("web1"), json!(8080), json!("netget"))
    );
    let health = json_of(
        &http(addr, "GET", "/v1/health/service/web?passing=1", b"")
            .await
            .2,
    );
    assert_eq!(health[0]["Service"]["Address"], "10.0.0.5");
    assert_eq!(health[0]["Checks"][0]["Status"], "passing");
    let agent = json_of(&http(addr, "GET", "/v1/agent/services", b"").await.2);
    assert_eq!(agent["web1"]["Service"], "web");
    assert_eq!(
        http(addr, "PUT", "/v1/agent/service/deregister/web1", b"")
            .await
            .0,
        200
    );
    assert_eq!(
        json_of(&http(addr, "GET", "/v1/catalog/services", b"").await.2),
        json!({})
    );
    // What Rust answers without the handler.
    assert_eq!(
        json_of(&http(addr, "GET", "/v1/status/leader", b"").await.2),
        json!("127.0.0.1:8300")
    );
    assert_eq!(
        json_of(&http(addr, "GET", "/v1/agent/self", b"").await.2)["Config"]["Datacenter"],
        "dc1"
    );
}

#[tokio::test]
async fn refusals_bounds_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, _id, addr) = start(handlers(&dir.path().join("consul.json"))).await;
    let big = vec![b'x'; 512 * 1024 + 1];
    assert_eq!(http(addr, "PUT", "/v1/kv/big", &big).await.0, 413);
    assert_eq!(
        http(addr, "PUT", "/v1/agent/service/register", b"{\"Port\":1}")
            .await
            .0,
        400
    );
    assert_eq!(http(addr, "GET", "/v1/acl/tokens", b"").await.0, 404);
    assert_eq!(http(addr, "GET", "/ui/", b"").await.0, 404);
    assert_eq!(http(addr, "PATCH", "/v1/kv/x", b"").await.0, 405);
    // No handler: the model is unreachable, so a write is never confirmed and a read invents
    // nothing.
    let (_state, _id, addr) = start(vec![]).await;
    let (status, _, body) = http(addr, "PUT", "/v1/kv/k", b"v").await;
    assert_eq!(status, 500);
    assert!(String::from_utf8_lossy(&body).starts_with("netget:"));
    assert_eq!(http(addr, "GET", "/v1/kv/k", b"").await.0, 500);
}
