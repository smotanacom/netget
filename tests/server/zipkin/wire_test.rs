//! Zipkin collector over raw HTTP: reports accepted, refused and canonicalised, gzip, every
//! query shape answered from what was reported, and the bounds and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::zipkin::wire;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// A collector whose "storage" is a JSON file the handler keeps — NetGet stores nothing. A
/// span named refuse-me makes the report a 429; a trace nobody reported is a 404.
pub fn store_script(store: &Path) -> String {
    format!(
        r#"import json,sys,os
P={store:?}
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
spans=json.load(open(P)) if os.path.exists(P) else []
svc=lambda s: s.get('localEndpoint',{{}}).get('serviceName')
if t=='zipkin_spans':
  if any(s.get('name')=='refuse-me' for s in e['spans']):
    a=[{{'type':'zipkin_reject','status':429,'message':'sampling budget exhausted'}}]
  else:
    json.dump(spans+e['spans'],open(P,'w')); a=[{{'type':'zipkin_accept'}}]
else:
  ep=e['endpoint']; q=e['query']; a=None
  if ep=='services': r=sorted({{svc(s) for s in spans if svc(s)}})
  elif ep=='spans': r=sorted({{s['name'] for s in spans if svc(s)==q.get('serviceName') and 'name' in s}})
  elif ep=='trace':
    r=[s for s in spans if s['traceId']==e['trace_id']]
    if not r: a=[{{'type':'zipkin_reject','status':404,'message':e['trace_id']+' not found'}}]
  elif ep=='traces':
    ids=sorted({{s['traceId'] for s in spans if svc(s)==q.get('serviceName')}})
    r=[[s for s in spans if s['traceId']==t] for t in ids]
  elif ep=='dependencies': r=[{{'parent':'frontend','child':'backend','callCount':3}}]
  elif ep=='autocompleteKeys': r='not-an-array'
  else: r=[]
  a=a or [{{'type':'zipkin_query_result','result':r}}]
print(json.dumps({{'actions':a}}))"#
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
        protocol: "zipkin".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Collect spans".into()),
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

pub async fn handler_saw(state: &AppState, id: ServerId, needle: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state
                .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
                .await
                .iter()
                .any(|e| serde_json::to_string(e).unwrap().contains(needle))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// One HTTP/1.1 exchange: (status, headers lower-cased, body).
async fn http(addr: SocketAddr, head: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{head}\r\nHost: zipkin\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    s.write_all(&req).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut out))
        .await
        .expect("response deadline")
        .unwrap();
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a header block");
    let head = String::from_utf8_lossy(&out[..split]).to_ascii_lowercase();
    let status = head[9..12].parse().unwrap();
    (status, head, out[split + 4..].to_vec())
}

async fn post(addr: SocketAddr, body: &Value) -> (u16, String) {
    let (status, _, body) = http(
        addr,
        "POST /api/v2/spans HTTP/1.1\r\nContent-Type: application/json",
        &serde_json::to_vec(body).unwrap(),
    )
    .await;
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let (status, _, body) = http(addr, &format!("GET {path} HTTP/1.1"), b"").await;
    (
        status,
        serde_json::from_slice(&body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&body))),
    )
}

#[tokio::test]
async fn reports_queries_and_canonical_spans() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id, addr) = start(handlers(&dir.path().join("spans.json"))).await;
    // Short ids are padded, unknown keys and an unparseable address dropped, scalar tags kept
    // as text: what zipkin-server does with the same span.
    let (status, body) = post(
        addr,
        &json!([
            {"traceId":"abc","id":"1","name":"get /cart","kind":"SERVER","timestamp":1700000000000000u64,"duration":2500,
             "localEndpoint":{"serviceName":"frontend","ipv4":"999.1.1.1","port":8080},"tags":{"http.status":200},"bogus":true},
            {"traceId":"abc","parentId":"1","id":"2","name":"select","kind":"CLIENT","localEndpoint":{"serviceName":"frontend"},
             "remoteEndpoint":{"serviceName":"db"},"annotations":[{"timestamp":1700000000000100u64,"value":"sent"}]},
            {"traceId":"def","id":"3","name":"consume","localEndpoint":{"serviceName":"worker"}}
        ]),
    )
    .await;
    assert_eq!((status, body.as_str()), (202, ""));
    assert!(handler_saw(&state, id, "\"span_count\":3").await);
    assert_eq!(
        get(addr, "/api/v2/services").await,
        (200, json!(["frontend", "worker"]))
    );
    assert_eq!(
        get(addr, "/api/v2/spans?serviceName=frontend").await,
        (200, json!(["get /cart", "select"]))
    );
    let (status, trace) = get(addr, "/api/v2/trace/abc").await;
    assert_eq!(status, 200);
    assert_eq!(
        trace[0],
        json!({"traceId":"0000000000000abc","id":"0000000000000001","kind":"SERVER","name":"get /cart",
               "timestamp":1700000000000000u64,"duration":2500,"localEndpoint":{"serviceName":"frontend","port":8080},
               "tags":{"http.status":"200"}})
    );
    assert_eq!(trace[1]["parentId"], "0000000000000001");
    assert_eq!(trace[1]["annotations"][0]["value"], "sent");
    let (status, traces) = get(addr, "/api/v2/traces?serviceName=worker&limit=10").await;
    assert_eq!((status, traces.as_array().unwrap().len()), (200, 1));
    assert_eq!(traces[0][0]["name"], "consume");
    assert_eq!(
        get(addr, "/api/v2/dependencies?endTs=1700000000000").await,
        (
            200,
            json!([{"parent":"frontend","child":"backend","callCount":3}])
        )
    );
    // The handler refuses: its status and message reach the wire.
    assert_eq!(
        get(addr, "/api/v2/trace/00000000000000ff").await,
        (404, json!("00000000000000ff not found"))
    );
    assert_eq!(
        post(addr, &json!([{"traceId":"a","id":"1","name":"refuse-me"}])).await,
        (429, "sampling budget exhausted".into())
    );
    // gzip is decoded before validation.
    let gz = wire::gzip(
        br#"[{"traceId":"77","id":"7","name":"zipped","localEndpoint":{"serviceName":"gz"}}]"#,
    )
    .unwrap();
    let (status, _, _) = http(
        addr,
        "POST /api/v2/spans HTTP/1.1\r\nContent-Type: application/json\r\nContent-Encoding: gzip",
        &gz,
    )
    .await;
    assert_eq!(status, 202);
    assert_eq!(
        get(addr, "/api/v2/spans?serviceName=gz").await,
        (200, json!(["zipped"]))
    );
}

#[tokio::test]
async fn refusals_bounds_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id, addr) = start(handlers(&dir.path().join("spans.json"))).await;
    // Malformed spans never reach the handler.
    for (span, needle) in [
        (json!([{"traceId":"ABC","id":"1"}]), "lower-hex"),
        (json!([{"traceId":"0","id":"1"}]), "must not be zero"),
        (
            json!([{"traceId":"a","id":"1","kind":"WEIRD"}]),
            "kind must be",
        ),
        (json!([{"id":"1"}]), "traceId required"),
        (json!({"traceId":"a"}), "JSON array"),
    ] {
        let (status, body) = post(addr, &span).await;
        assert_eq!(status, 400, "{span}");
        assert!(body.contains(needle), "{body}");
    }
    // An empty report is accepted without asking anyone.
    assert_eq!(post(addr, &json!([])).await.0, 202);
    let (status, _, _) = http(
        addr,
        "POST /api/v2/spans HTTP/1.1\r\nContent-Type: application/x-protobuf",
        b"\x0a\x00",
    )
    .await;
    assert_eq!(status, 415);
    let too_many: Vec<Value> = (1..=wire::MAX_SPANS + 1)
        .map(|n| json!({"traceId":"a","id":format!("{n:x}")}))
        .collect();
    assert_eq!(post(addr, &json!(too_many)).await.0, 400);
    let big = vec![b' '; wire::MAX_BODY_BYTES + 1];
    assert_eq!(
        http(
            addr,
            "POST /api/v2/spans HTTP/1.1\r\nContent-Type: application/json",
            &big
        )
        .await
        .0,
        413
    );
    // A gzip bomb is refused by its decompressed size.
    let bomb = wire::gzip(&vec![b' '; wire::MAX_BODY_BYTES + 1]).unwrap();
    assert!(bomb.len() < wire::MAX_BODY_BYTES);
    assert_eq!(
        http(
            addr,
            "POST /api/v2/spans HTTP/1.1\r\nContent-Type: application/json\r\nContent-Encoding: gzip",
            &bomb
        )
        .await
        .0,
        413
    );
    assert_eq!(get(addr, "/api/v1/spans").await.0, 404);
    assert_eq!(get(addr, "/api/v2/nope").await.0, 404);
    assert_eq!(get(addr, "/api/v2/services?evil=1").await.0, 400);
    assert_eq!(get(addr, "/api/v2/trace/XYZ").await.0, 400);
    assert_eq!(
        http(addr, "DELETE /api/v2/services HTTP/1.1", b"").await.0,
        405
    );
    // An answer in the wrong shape for its endpoint is never sent.
    let (status, body) = get(addr, "/api/v2/autocompleteKeys").await;
    assert_eq!(status, 503);
    assert_eq!(body, json!("netget: request could not be processed"));
    assert!(handler_saw(&state, id, "zipkin_query").await);
}

#[tokio::test]
async fn unreachable_model_fails_closed() {
    // No handler: the model is unreachable, so a report is neither accepted nor dropped.
    let (_state, _id, addr) = start(vec![]).await;
    let (status, head, body) = http(
        addr,
        "POST /api/v2/spans HTTP/1.1\r\nContent-Type: application/json",
        br#"[{"traceId":"a","id":"1"}]"#,
    )
    .await;
    assert_eq!(status, 503);
    assert!(head.contains("retry-after: 5"), "{head}");
    assert!(String::from_utf8_lossy(&body).starts_with("netget:"));
    assert_eq!(get(addr, "/api/v2/services").await.0, 503);
}
