//! JetStream on NetGet's NATS server, against two independent JetStream clients that fail
//! rather than skip when absent: nats.go's `jetstream` package (`jetstream_peer/`) and nats-py
//! (`jetstream_peer.py`). Each creates a stream, publishes, has one publish refused, creates a
//! durable pull consumer, fetches with acks (async and sync), fetches the rest, reads infos and
//! deletes. The handler keeps the streams in a JSON file — NetGet stores no messages — so what
//! each client reads back is what the handler was told, through NetGet's envelopes, ack subjects
//! and status headers. `install_jetstream_peers.py` prints NETGET_JS_GO_PEER and
//! NETGET_JS_PYTHON.
use netget::cli::management::ServerForm;
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{path::Path, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Streams, their messages and consumers, kept by the handler in a file.
fn store_script(store: &Path) -> String {
    format!(
        r#"import json,sys,os
P={store:?}
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
st=json.load(open(P)) if os.path.exists(P) else {{'streams':{{}}}}
S=st['streams']
def save(): json.dump(st,open(P,'w'))
def reply(r): return {{'type':'nats_js_reply','response':r}}
def err(c,ec,d): return {{'type':'nats_js_error','code':c,'err_code':ec,'description':d}}
a=None
if t=='nats_js_api':
  op=e['operation']; s=e.get('stream'); c=e.get('consumer'); r=e['request']
  if op=='INFO': a=reply({{'streams':len(S),'consumers':sum(len(x['consumers']) for x in S.values())}})
  elif op=='STREAM.NAMES': a=reply({{'streams':sorted(S)}})
  elif op=='STREAM.CREATE':
    if s in S: a=err(400,10058,'stream name already in use')
    else:
      S[s]={{'config':r,'messages':[],'consumers':{{}}}}; save(); a=reply({{'config':r}})
  elif s not in S: a=err(404,10059,'stream not found')
  elif op=='STREAM.INFO':
    m=S[s]['messages']
    a=reply({{'config':S[s]['config'],'state':{{'messages':len(m),'bytes':sum(len(x['payload']) for x in m),'first_seq':1 if m else 0,'last_seq':len(m),'consumer_count':len(S[s]['consumers'])}}}})
  elif op=='STREAM.DELETE':
    del S[s]; save(); a=reply({{}})
  elif op=='CONSUMER.CREATE':
    cfg=r.get('config',{{}}); name=c or cfg.get('durable_name') or cfg.get('name')
    S[s]['consumers'].setdefault(name,{{'config':cfg,'next':0,'acked':[]}}); save()
    a=reply({{'config':cfg,'num_pending':len(S[s]['messages'])}})
  elif c not in S[s]['consumers']: a=err(404,10014,'consumer not found')
  elif op=='CONSUMER.INFO':
    k=S[s]['consumers'][c]; f=max(k['acked'] or [0])
    a=reply({{'config':k['config'],'ack_floor':{{'consumer_seq':f,'stream_seq':f}},'num_pending':len(S[s]['messages'])-k['next']}})
  elif op=='CONSUMER.DELETE':
    del S[s]['consumers'][c]; save(); a=reply({{}})
  else: a=err(400,10025,'unsupported')
elif t=='nats_js_publish':
  if e['subject'].endswith('.rejected'): a=err(400,10060,'rejected by policy')
  else:
    m=S[e['stream']]['messages']; m.append({{'subject':e['subject'],'payload':e['payload'],'encoding':e['payload_encoding'],'headers':e['headers']}}); save()
    a={{'type':'nats_js_ack','seq':len(m)}}
elif t=='nats_js_pull':
  m=S[e['stream']]['messages']; k=S[e['stream']]['consumers'][e['consumer']]; n=k['next']; b=m[n:n+e['batch']]; k['next']=n+len(b); save()
  a={{'type':'nats_js_deliver','messages':[dict(subject=x['subject'],payload=x['payload'],encoding=x['encoding'],headers=x['headers'],stream_seq=n+j+1,consumer_seq=n+j+1) for j,x in enumerate(b)],'pending':len(m)-k['next']}}
elif t=='nats_js_acked':
  for x in S.values():
    if e['consumer'] in x['consumers']: x['consumers'][e['consumer']]['acked'].append(e['stream_seq'])
  save(); a={{'type':'nats_js_noted'}}
print(json.dumps({{'actions':[a] if a else []}}))"#
    )
}

async fn start(store: &Path, handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let handlers = handlers.unwrap_or_else(|| {
        vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":store_script(store)}})]
    });
    let id = ServerForm {
        protocol: "nats".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve JetStream".into()),
        startup_params: Some(json!({"jetstream": true})),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| {
        panic!("{var} is required: run tests/server/nats/install_jetstream_peers.py <root> and export what it prints")
    })
}

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(&program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

async fn saw(state: &AppState, id: ServerId, needle: &str) -> bool {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .any(|e| serde_json::to_string(e).unwrap().contains(needle))
}

#[tokio::test]
async fn nats_go_jetstream_client() {
    let dir = tempfile::tempdir().unwrap();
    let (state, id, port) = start(&dir.path().join("js.json"), None).await;
    let out = run(
        env_path("NETGET_JS_GO_PEER"),
        &[&format!("nats://127.0.0.1:{port}")],
    )
    .await;
    assert_eq!(out["created"], "ORDERS", "{out}");
    assert_eq!(out["created_subjects"], json!(["orders.*"]));
    assert_eq!(out["published_seqs"], json!([1, 2, 3]));
    assert!(
        out["refused"]
            .as_str()
            .unwrap_or_default()
            .contains("rejected by policy"),
        "{out}"
    );
    assert_eq!(out["consumer"], "proc");
    let fetched = out["fetched"].as_array().unwrap();
    assert_eq!(fetched.len(), 2, "{out}");
    for (i, m) in fetched.iter().enumerate() {
        let n = i as u64 + 1;
        assert_eq!(m["subject"], "orders.new");
        assert_eq!(m["data"], format!("{{\"id\":{n}}}"));
        assert_eq!(m["header"], n.to_string());
        assert_eq!(
            (m["stream_seq"].as_u64(), m["consumer_seq"].as_u64()),
            (Some(n), Some(n))
        );
        assert_eq!(
            (m["stream"].clone(), m["consumer"].clone()),
            (json!("ORDERS"), json!("proc"))
        );
    }
    assert!(out.get("batch_error").is_none(), "{out}");
    assert_eq!(out["rest"], json!([r#"{"id":3}"#]));
    assert_eq!(out["empty_count"], 0);
    assert_eq!(out["stream_messages"], 3);
    assert_eq!(out["stream_last_seq"], 3);
    assert_eq!(out["ack_floor"], 3, "the handler saw all three acks: {out}");
    assert_eq!(out["names"], json!(["ORDERS"]));
    assert_eq!(out["account_streams"], 1);
    assert!(out["missing"].as_str().unwrap().contains("10059"), "{out}");
    // The sync ack was answered and the handler saw every kind of event.
    for needle in [
        "nats_js_api",
        "nats_js_publish",
        "nats_js_pull",
        "nats_js_acked",
    ] {
        assert!(saw(&state, id, needle).await, "{needle}");
    }
}

#[tokio::test]
async fn nats_py_jetstream_client() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, _id, port) = start(&dir.path().join("js.json"), None).await;
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/server/nats/jetstream_peer.py"
    );
    let out = run(
        env_path("NETGET_JS_PYTHON"),
        &["-I", script, &format!("nats://127.0.0.1:{port}")],
    )
    .await;
    assert_eq!(out["created"], "PYORDERS", "{out}");
    assert_eq!(out["published_seqs"], json!([1, 2, 3]));
    assert_eq!(out["refused"], "rejected by policy");
    let fetched = out["fetched"].as_array().unwrap();
    assert_eq!(fetched.len(), 2, "{out}");
    assert_eq!(fetched[1]["data"], r#"{"id": 2}"#);
    assert_eq!(fetched[1]["header"], "2");
    assert_eq!(fetched[1]["stream_seq"], 2);
    assert_eq!(out["rest"], json!([r#"{"id": 3}"#]));
    assert_eq!(out["empty"], "timeout");
    assert_eq!(out["stream_messages"], 3);
    assert_eq!(out["missing"], "stream not found");
}

/// A raw client: request on a subject with a subscribed inbox, read the reply's body.
async fn request(s: &mut TcpStream, subject: &str, body: &str) -> String {
    s.write_all(format!("PUB {subject} _INBOX.raw.1 {}\r\n{body}\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(20), s.read(&mut chunk))
            .await
            .expect("a reply in time")
            .unwrap();
        assert!(
            n > 0,
            "connection closed: {}",
            String::from_utf8_lossy(&buf)
        );
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(at) = text.find("MSG _INBOX.raw.1 ") {
            let rest = &text[at..];
            if let Some(end) = rest.find("\r\n") {
                let len: usize = rest[..end].rsplit(' ').next().unwrap().parse().unwrap();
                if rest.len() >= end + 2 + len {
                    return rest[end + 2..end + 2 + len].to_string();
                }
            }
        }
    }
}

async fn raw(port: u16) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut info = [0u8; 2048];
    let n = s.read(&mut info).await.unwrap();
    let text = String::from_utf8_lossy(&info[..n]).to_string();
    assert!(text.contains("\"jetstream\":true"), "{text}");
    s.write_all(b"CONNECT {\"verbose\":false,\"headers\":true}\r\nSUB _INBOX.raw.* 1\r\n")
        .await
        .unwrap();
    s
}

#[tokio::test]
async fn envelopes_errors_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, _id, port) = start(&dir.path().join("js.json"), None).await;
    let mut s = raw(port).await;
    // The envelope fills what a client decodes; the type names the operation.
    let info: Value = serde_json::from_str(&request(&mut s, "$JS.API.INFO", "").await).unwrap();
    assert_eq!(
        info["type"],
        "io.nats.jetstream.api.v1.account_info_response"
    );
    assert_eq!(info["limits"]["max_streams"], -1);
    let created: Value = serde_json::from_str(
        &request(&mut s, "$JS.API.STREAM.CREATE.RAW", r#"{"name":"RAW"}"#).await,
    )
    .unwrap();
    assert_eq!(
        created["type"],
        "io.nats.jetstream.api.v1.stream_create_response"
    );
    assert_eq!(
        created["config"]["subjects"],
        json!(["RAW"]),
        "a stream without subjects captures its own name"
    );
    assert_eq!(created["state"]["messages"], 0);
    // ...and that subject now routes to the stream.
    let ack: Value = serde_json::from_str(&request(&mut s, "RAW", "hello").await).unwrap();
    assert_eq!(ack, json!({"stream": "RAW", "seq": 1}));
    // Errors carry the handler's codes; a bad body and an unsupported API are refused in Rust.
    let missing: Value =
        serde_json::from_str(&request(&mut s, "$JS.API.STREAM.INFO.NOPE", "").await).unwrap();
    assert_eq!(
        missing["error"],
        json!({"code": 404, "err_code": 10059, "description": "stream not found"})
    );
    let bad: Value =
        serde_json::from_str(&request(&mut s, "$JS.API.STREAM.CREATE.X", "{not json").await)
            .unwrap();
    assert_eq!(bad["error"]["code"], 400);
    let unsupported: Value =
        serde_json::from_str(&request(&mut s, "$JS.API.STREAM.MSG.GET.RAW", "{}").await).unwrap();
    assert_eq!(unsupported["error"]["err_code"], 10025);
    // A no_wait pull for a consumer with nothing pending ends with the 404 status.
    request(
        &mut s,
        "$JS.API.CONSUMER.CREATE.RAW.c",
        r#"{"stream_name":"RAW","config":{"durable_name":"c"}}"#,
    )
    .await;
    let pull = r#"{"batch":5,"no_wait":true}"#;
    s.write_all(
        format!(
            "PUB $JS.API.CONSUMER.MSG.NEXT.RAW.c _INBOX.raw.2 {}\r\n{pull}\r\n",
            pull.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !String::from_utf8_lossy(&buf).contains("404 No Messages")
        && !String::from_utf8_lossy(&buf).contains("408 Request Timeout")
    {
        let n = tokio::time::timeout(Duration::from_secs(20), s.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    let delivery = text
        .find("MSG RAW 1 $JS.ACK.RAW.c.1.1.1.")
        .expect("the stored message, with its ack subject");
    let status = text
        .find("HMSG _INBOX.raw.2 1 ")
        .expect("the end-of-batch status");
    assert!(delivery < status, "{text}");
    assert!(
        text.contains("408 Request Timeout"),
        "one message of five: the pull expires: {text}"
    );

    // No handler for JetStream (core frames answered with nothing): the model is unreachable,
    // so every API answer is a 503 error, never a fabricated success.
    let dir2 = tempfile::tempdir().unwrap();
    let core_only = ["nats_connect", "nats_subscribe"]
        .map(|e| json!({"event_pattern": e, "handler": {"type": "static", "actions": []}}))
        .to_vec();
    let (_state, _id, port) = start(&dir2.path().join("js.json"), Some(core_only)).await;
    let mut s = raw(port).await;
    let refused: Value =
        serde_json::from_str(&request(&mut s, "$JS.API.STREAM.CREATE.X", r#"{"name":"X"}"#).await)
            .unwrap();
    assert_eq!(refused["error"]["code"], 503, "{refused}");
    assert!(refused["error"]["description"]
        .as_str()
        .unwrap()
        .starts_with("netget:"));
}
