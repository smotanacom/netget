//! The Anthropic client against NetGet's own server (a small echo script; the server suite's lives
//! in another test target): a handler chain where each request is built from the last answer,
//! streamed reassembly, tool use, refusals from the server and locally, and the follow-up bound.
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, client_handles::ClientSendOutcome, AccessLogOwner, ClientId},
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

/// The chain: a plain message; its text is asked again, streamed; the streamed text is counted;
/// then the models are listed.
pub const CHAIN: &str = r#"import json,sys
e=json.load(sys.stdin)['event']; a=[]
op=e['operation']
if e['status']==200 and op=='anthropic_create_message' and not e.get('streamed'):
  a=[{'type':'anthropic_create_message','prompt':'again: '+e['message']['text'],'max_tokens':16,'stream':True}]
elif e['status']==200 and op=='anthropic_create_message':
  a=[{'type':'anthropic_count_tokens','prompt':e['message']['text']}]
elif e['status']==200 and op=='anthropic_count_tokens':
  a=[{'type':'anthropic_list_models'}]
print(json.dumps({'actions':a}))"#;

pub fn chain_handlers(first_prompt: &str) -> Vec<Value> {
    vec![
        json!({"event_pattern":"anthropic_connected","handler":{"type":"static","actions":[
            {"type":"anthropic_create_message","prompt":first_prompt,"max_tokens":16,"temperature":0}]}}),
        json!({"event_pattern":"anthropic_response","handler":{"type":"script","language":"python","code":CHAIN}}),
    ]
}

pub async fn client_with_status(
    remote: String,
    handlers: Vec<Value>,
    params: Option<Value>,
) -> (AppState, ClientId, mpsc::UnboundedReceiver<String>) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, rx) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "anthropic".into(),
        remote_addr: Some(remote),
        instruction: Some("Talk to the model".into()),
        event_handlers: Some(handlers),
        startup_params: Some(params.unwrap_or_else(|| json!({"model":"claude-netget-1"}))),
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
    (state, id, rx)
}

/// Every anthropic_response, oldest first.
pub async fn responses(state: &AppState, id: ClientId) -> Vec<Value> {
    let mut out: Vec<Value> = state
        .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == "anthropic_response")
        .map(|e| e["request"].clone())
        .collect();
    out.reverse();
    out
}

/// The responses once `n` have arrived.
pub async fn wait_for(state: &AppState, id: ClientId, n: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let r = responses(state, id).await;
            if r.len() >= n {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fewer than {n} responses"))
}

pub async fn send(state: &AppState, id: ClientId, action: Value) -> ClientSendOutcome {
    state
        .send_to_client(id, action, Duration::from_secs(120))
        .await
        .unwrap()
}

pub fn executed(o: ClientSendOutcome) -> Value {
    match o {
        ClientSendOutcome::Executed { detail } => serde_json::from_str(&detail).unwrap(),
        other => panic!("{other:?}"),
    }
}

/// Echo the last user text; `deny-me` is refused; a question offering tools calls the first.
const SERVER: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
last=e['messages'][-1]['content']
text=' '.join(b.get('text','') for b in last if b['type']=='text')
res=[b for b in last if b['type']=='tool_result']
if e['model']=='deny-me': a={'type':'anthropic_error','error_type':'overloaded_error','message':'busy'}
elif res: a={'type':'anthropic_reply','text':'tool said '+res[0]['content']}
elif e['tools']: a={'type':'anthropic_reply','content':[{'type':'tool_use','id':'toolu_fixed','name':e['tools'][0]['name'],'input':{'q':text}}]}
else: a={'type':'anthropic_reply','text':'echo '+text,'stop_reason':'end_turn'}
print(json.dumps({'actions':[a]}))"#;

async fn netget_server() -> String {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let sid = ServerForm {
        protocol: "anthropic".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Echo".into()),
        event_handlers: Some(vec![json!({"event_pattern":"anthropic_message","handler":{"type":"script","language":"python","code":SERVER}})]),
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
    // The server lives as long as the process; its state is leaked deliberately.
    std::mem::forget(state);
    format!("127.0.0.1:{port}")
}

#[tokio::test]
async fn against_netget_server() {
    let (state, id, _status) =
        client_with_status(netget_server().await, chain_handlers("hello"), None).await;
    let r = wait_for(&state, id, 4).await;
    assert_eq!(r[0]["message"]["text"], "echo hello", "{r:?}");
    assert!(r[0]["message"]["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(r[0]["message"]["stop_reason"], "end_turn");
    assert!(r[0]["streamed"].is_null());
    // The second request was built from the first answer, and came back as a stream.
    assert_eq!(r[1]["message"]["text"], "echo again: echo hello", "{r:?}");
    assert_eq!(r[1]["streamed"]["message_start"], 1);
    assert_eq!(r[1]["streamed"]["message_stop"], 1);
    assert!(r[1]["streamed"]["content_block_delta"].as_u64().unwrap() > 1);
    assert!(r[2]["input_tokens"].as_u64().unwrap() > 0, "{r:?}");
    assert_eq!(r[3]["models"], json!(["claude-netget-1"]));
    // Tool use, plain and streamed, then the tool's result.
    let tools = json!([{"name":"lookup","description":"Look something up","input_schema":{"type":"object"}}]);
    for stream in [false, true] {
        let v = executed(send(&state, id, json!({"type":"anthropic_create_message","prompt":"find x","tools":tools,"stream":stream})).await);
        assert_eq!(v["message"]["stop_reason"], "tool_use", "{v}");
        assert_eq!(
            v["message"]["content"],
            json!([{"type":"tool_use","id":"toolu_fixed","name":"lookup","input":{"q":"find x"}}]),
            "{v}"
        );
    }
    let v = executed(send(&state, id, json!({"type":"anthropic_create_message","messages":[
        {"role":"user","content":"find x"},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_fixed","name":"lookup","input":{"q":"find x"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_fixed","content":"42"}]}]})).await);
    assert_eq!(v["message"]["text"], "tool said 42");
    // The server's refusals, as the envelope says them.
    let v = executed(
        send(
            &state,
            id,
            json!({"type":"anthropic_create_message","model":"deny-me","prompt":"x"}),
        )
        .await,
    );
    assert_eq!(
        (v["status"].as_u64(), &v["error"]),
        (
            Some(529),
            &json!({"type":"overloaded_error","message":"busy"})
        )
    );
    let v = executed(
        send(
            &state,
            id,
            json!({"type":"anthropic_get_model","model_id":"nope"}),
        )
        .await,
    );
    assert_eq!(
        (v["status"].as_u64(), &v["error"]["type"]),
        (Some(404), &json!("not_found_error"))
    );
    // Refused locally, before anything is sent.
    for bad in [
        json!({"type":"anthropic_create_message"}),
        json!({"type":"anthropic_create_message","prompt":"x","messages":[{"role":"user","content":"y"}]}),
        json!({"type":"anthropic_create_message","messages":[{"role":"robot","content":"y"}]}),
        json!({"type":"anthropic_create_message","prompt":"x","temperature":2}),
        json!({"type":"anthropic_create_message","prompt":"x","max_tokens":0}),
        json!({"type":"anthropic_get_model","model_id":"../../etc"}),
    ] {
        match send(&state, id, bad.clone()).await {
            ClientSendOutcome::Rejected { .. } => {}
            other => panic!("{bad}: {other:?}"),
        }
    }
    let o = send(&state, id, json!({"type":"disconnect"})).await;
    assert!(matches!(o, ClientSendOutcome::Disconnected), "{o:?}");
}

#[tokio::test]
async fn model_is_required_and_the_key_is_sent() {
    let server = netget_server().await;
    // No model anywhere: refused locally.
    let quiet = vec![json!({"event_pattern":"*","handler":{"type":"static","actions":[]}})];
    let (state, id, _s) = client_with_status(server.clone(), quiet.clone(), Some(json!({}))).await;
    assert!(matches!(
        send(
            &state,
            id,
            json!({"type":"anthropic_create_message","prompt":"x"})
        )
        .await,
        ClientSendOutcome::Rejected { .. }
    ));
    // The key and version reach the wire.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let seen = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut s, _) = l.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = s.read(&mut buf).await.unwrap();
        let body = br#"{"input_tokens":3}"#;
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        s.write_all(head.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
    });
    let params =
        json!({"model":"m","api_key":"sk-ant-secret-value","anthropic_version":"2023-06-01"});
    let (state, id, _s) = client_with_status(addr, quiet, Some(params)).await;
    let v = executed(
        send(
            &state,
            id,
            json!({"type":"anthropic_count_tokens","prompt":"x"}),
        )
        .await,
    );
    assert_eq!(v["input_tokens"], 3);
    let request = seen.await.unwrap();
    assert!(
        request.starts_with("post /v1/messages/count_tokens "),
        "{request}"
    );
    assert!(
        request.contains("x-api-key: sk-ant-secret-value"),
        "{request}"
    );
    assert!(
        request.contains("anthropic-version: 2023-06-01"),
        "{request}"
    );
    // The key appears in no event.
    let logs = serde_json::to_string(
        &state
            .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
            .await,
    )
    .unwrap();
    assert!(!logs.contains("sk-ant-secret-value"), "{logs}");
}

#[tokio::test]
async fn followup_chain_is_bounded() {
    let looping = vec![
        json!({"event_pattern":"*","handler":{"type":"static","actions":[
        {"type":"anthropic_list_models"}]}}),
    ];
    let (state, id, mut status) = client_with_status(netget_server().await, looping, None).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = status.recv().await {
            if line.contains("handler chain stopped after 8 follow-ups") {
                return;
            }
        }
        panic!("status channel closed");
    })
    .await
    .expect("the chain to be stopped");
    assert_eq!(responses(&state, id).await.len(), 8);
}
