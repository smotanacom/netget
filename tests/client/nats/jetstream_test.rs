//! NetGet's NATS client driving JetStream on the official `nats-server -js` (v2.10), failing
//! rather than skipping when the binary is absent. The client's handlers do all of it — create
//! a stream on connect, publish three messages and create a durable pull consumer when the
//! stream exists, fetch when the consumer exists, ack each message — and the assertions are the
//! server's own accounting read back over a raw connection: three messages stored, all three
//! acknowledged, nothing pending.
use crate::helpers::real_server::{InstallHint, RealServer};
use netget::{
    cli::management::ClientForm,
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

const ON_RESPONSE: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; a=[]
if e['operation']=='STREAM.CREATE' and not e['error']:
  a=[{'type':'nats_js_publish','subject':'orders.new','payload':json.dumps({'id':n}),'headers':{'Order-Index':str(n)}} for n in (1,2,3)]
  a.append({'type':'nats_js_api','operation':'CONSUMER.CREATE','stream':'ORDERS','consumer':'proc','request':{'stream_name':'ORDERS','config':{'durable_name':'proc','ack_policy':'explicit'}}})
elif e['operation']=='CONSUMER.CREATE' and not e['error']:
  a=[{'type':'nats_js_fetch','stream':'ORDERS','consumer':'proc','batch':5,'expires_ms':1500}]
print(json.dumps({'actions':a}))"#;

const ON_MESSAGE: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
print(json.dumps({'actions':[{'type':'nats_js_ack','ack_subject':e['ack_subject']}]}))"#;

async fn nats_server() -> RealServer {
    RealServer::builder(
        "nats-server",
        InstallHint {
            brew: "nats-server",
            apt: "nats-server (or the v2.10.24 release tarball)",
        },
    )
    .args(["-a", "127.0.0.1", "-p", "{port}", "-js", "-sd", "{dir}/js"])
    .ready_when_log_matches("Server is ready")
    .startup_timeout(Duration::from_secs(30))
    .start()
    .await
    .expect("start nats-server -js")
}

async fn client(addr: String) -> (AppState, ClientId) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "nats".into(),
        remote_addr: Some(addr),
        instruction: Some("Run the order stream".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"nats_connected","handler":{"type":"static","actions":[
                {"type":"nats_js_api","operation":"STREAM.CREATE","stream":"ORDERS","request":{"name":"ORDERS","subjects":["orders.*"]}}]}}),
            json!({"event_pattern":"nats_js_response","handler":{"type":"script","language":"python","code":ON_RESPONSE}}),
            json!({"event_pattern":"nats_js_message","handler":{"type":"script","language":"python","code":ON_MESSAGE}}),
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
    (state, id)
}

async fn events(state: &AppState, id: ClientId, kind: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == kind)
        .map(|e| e["request"].clone())
        .collect()
}

/// Ask the real server a JetStream question over a raw connection of the test's own.
async fn ask(addr: &str, subject: &str) -> Value {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let body = format!("CONNECT {{\"verbose\":false}}\r\nSUB _INBOX.check 1\r\nPUB {subject} _INBOX.check 0\r\n\r\n");
    s.write_all(body.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut chunk))
            .await
            .expect("an answer")
            .unwrap();
        assert!(n > 0);
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        if let Some(at) = text.find("MSG _INBOX.check 1 ") {
            let rest = &text[at..];
            if let Some(end) = rest.find("\r\n") {
                let len: usize = rest[..end].rsplit(' ').next().unwrap().parse().unwrap();
                if rest.len() >= end + 2 + len {
                    return serde_json::from_str(&rest[end + 2..end + 2 + len]).unwrap();
                }
            }
        }
    }
}

#[tokio::test]
async fn netget_drives_jetstream_on_nats_server() {
    let server = nats_server().await;
    let addr = server.addr();
    let (state, id) = client(addr.clone()).await;
    // The handlers' fetch delivers three messages and ends at its expiry with 408.
    tokio::time::timeout(Duration::from_secs(30), async {
        while events(&state, id, "nats_js_fetch_done").await.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the fetch ended");
    let acks = events(&state, id, "nats_js_publish_ack").await;
    let mut seqs: Vec<u64> = acks.iter().filter_map(|a| a["seq"].as_u64()).collect();
    seqs.sort_unstable();
    assert_eq!(seqs, vec![1, 2, 3], "{acks:?}");
    assert!(acks
        .iter()
        .all(|a| a["stream"] == "ORDERS" && a["error"].is_null()));
    let mut msgs = events(&state, id, "nats_js_message").await;
    msgs.sort_by_key(|m| m["stream_seq"].as_u64());
    assert_eq!(msgs.len(), 3, "{msgs:?}");
    for (i, m) in msgs.iter().enumerate() {
        let n = i as u64 + 1;
        assert_eq!(m["subject"], "orders.new");
        assert_eq!(m["payload"], format!("{{\"id\": {n}}}"));
        assert_eq!(m["headers"]["Order-Index"], n.to_string());
        assert_eq!(
            (m["stream"].clone(), m["consumer"].clone()),
            (json!("ORDERS"), json!("proc"))
        );
        assert_eq!(
            (m["stream_seq"].as_u64(), m["consumer_seq"].as_u64()),
            (Some(n), Some(n))
        );
    }
    let done = &events(&state, id, "nats_js_fetch_done").await[0];
    assert_eq!(done["status"], 408, "{done}");
    // The server's own accounting: three stored, all acknowledged.
    let info = ask(&addr, "$JS.API.STREAM.INFO.ORDERS").await;
    assert_eq!(info["state"]["messages"], 3, "{info}");
    let consumer = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let c = ask(&addr, "$JS.API.CONSUMER.INFO.ORDERS.proc").await;
            if c["ack_floor"]["stream_seq"] == 3 {
                break c;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("every ack reached the server");
    assert_eq!(consumer["num_ack_pending"], 0, "{consumer}");
    assert_eq!(consumer["num_pending"], 0);
    // An injected API call, and a refusal the server's error carries back.
    let sent = state
        .send_to_client(
            id,
            json!({"type":"nats_js_api","operation":"STREAM.INFO","stream":"MISSING"}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(sent, ClientSendOutcome::Sent { .. }), "{sent:?}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let r = events(&state, id, "nats_js_response").await;
            if let Some(e) = r.iter().find(|e| e["stream"] == "MISSING") {
                assert_eq!(e["error"]["err_code"], 10059, "{e}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the refusal arrived");
    // A bad action is refused before anything is sent.
    let bad = state
        .send_to_client(
            id,
            json!({"type":"nats_js_fetch","stream":"ORDERS","consumer":"proc","batch":0}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(matches!(bad, ClientSendOutcome::Rejected { .. }), "{bad:?}");
    state.remove_client(id).await;
}
