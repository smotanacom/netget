//! The Anthropic Messages API over raw HTTP: validation and the error envelope, a plain reply,
//! the event stream with text and tool-use deltas, count_tokens, models, the API key check, the
//! body bound and the fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::anthropic::wire;
use netget::state::{app_state::AppState, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Echoes the last user text (and the system prompt); `deny-me` is rate limited, `silent` says
/// nothing; a question about weather with tools offered calls the first tool, and a tool_result
/// is answered with what the tool said.
pub const ASSISTANT: &str = r#"import json,sys
e=json.load(sys.stdin)['event']
last=e['messages'][-1]['content']
results=[b for b in last if b['type']=='tool_result']
text=' '.join(b.get('text','') for b in last if b['type']=='text')
a=None
if e['model']=='deny-me': a={'type':'anthropic_error','error_type':'rate_limit_error','message':'slow down'}
elif e['model']=='silent': a=None
elif results: a={'type':'anthropic_reply','text':'Tool said: '+results[0]['content']}
elif e['tools'] and 'weather' in text: a={'type':'anthropic_reply','content':[{'type':'text','text':'Let me check.'},{'type':'tool_use','name':e['tools'][0]['name'],'input':{'city':'Paris','days':[1,2]}}]}
else: a={'type':'anthropic_reply','text':'Echo: '+text+(' | system: '+e['system'] if e['system'] else '')}
print(json.dumps({'actions':[a] if a else []}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"anthropic_message","handler":{"type":"script","language":"python","code":ASSISTANT}}),
    ]
}

pub async fn start(
    handlers: Vec<Value>,
    params: Option<Value>,
) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "anthropic".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Assistant".into()),
        event_handlers: Some(handlers),
        startup_params: params,
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

pub struct Answer {
    pub status: u16,
    pub headers: String,
    pub body: String,
}

impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }
}

/// One request on its own connection, read to the end.
pub async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Answer {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), s.read_to_end(&mut raw))
        .await
        .expect("response deadline")
        .unwrap();
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = headers
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Answer {
        status,
        headers: headers.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

pub async fn post(addr: SocketAddr, path: &str, body: Value) -> Answer {
    http(
        addr,
        "POST",
        path,
        &[("content-type", "application/json")],
        body.to_string().as_bytes(),
    )
    .await
}

fn events(stream: &str) -> Vec<(String, Value)> {
    stream
        .split("\n\n")
        .filter(|f| !f.trim().is_empty())
        .map(|f| {
            let event = f
                .lines()
                .find_map(|l| l.strip_prefix("event: "))
                .unwrap()
                .to_string();
            let data = f.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
            (event, serde_json::from_str(data).unwrap())
        })
        .collect()
}

fn assert_error(a: &Answer, status: u16, error_type: &str) -> Value {
    assert_eq!(a.status, status, "{}", a.body);
    let v = a.json();
    assert_eq!(v["type"], "error", "{v}");
    assert_eq!(v["error"]["type"], error_type, "{v}");
    assert!(v["request_id"].as_str().unwrap().starts_with("req_"), "{v}");
    assert!(a.headers.contains("request-id: req_"), "{}", a.headers);
    v
}

#[tokio::test]
async fn messages_plain_streamed_and_tools() {
    let (_state, _id, addr) = start(handlers(), None).await;
    // Plain.
    let a = post(
        addr,
        "/v1/messages",
        json!({"model":"claude-netget-1","max_tokens":32,"system":"Be brief.",
        "messages":[{"role":"user","content":"hello"}]}),
    )
    .await;
    assert_eq!(a.status, 200, "{}", a.body);
    let m = a.json();
    assert!(m["id"].as_str().unwrap().starts_with("msg_"));
    assert_eq!(
        (&m["type"], &m["role"], &m["model"]),
        (
            &json!("message"),
            &json!("assistant"),
            &json!("claude-netget-1")
        )
    );
    assert_eq!(
        m["content"],
        json!([{"type":"text","text":"Echo: hello | system: Be brief."}])
    );
    assert_eq!(m["stop_reason"], "end_turn");
    assert!(
        m["usage"]["input_tokens"].as_u64().unwrap() > 0
            && m["usage"]["output_tokens"].as_u64().unwrap() > 0
    );
    // Streamed text: the exact event sequence, and the deltas rebuild the text.
    let a = post(addr, "/v1/messages", json!({"model":"claude-netget-1","max_tokens":32,"stream":true,
        "messages":[{"role":"user","content":[{"type":"text","text":"a longer line of text, ünïcode ✓"}]}]}))
    .await;
    assert!(
        a.headers.contains("content-type: text/event-stream"),
        "{}",
        a.headers
    );
    let ev = events(&a.body);
    let names: Vec<&str> = ev.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names[..4],
        [
            "message_start",
            "ping",
            "content_block_start",
            "content_block_delta"
        ]
    );
    assert_eq!(
        names[names.len() - 3..],
        ["content_block_stop", "message_delta", "message_stop"]
    );
    assert!(
        names
            .iter()
            .filter(|n| **n == "content_block_delta")
            .count()
            > 1
    );
    for (name, data) in &ev {
        assert_eq!(data["type"], name.as_str());
    }
    let text: String = ev
        .iter()
        .filter_map(|(_, d)| d["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "Echo: a longer line of text, ünïcode ✓");
    assert_eq!(ev[0].1["message"]["content"], json!([]));
    assert_eq!(ev[ev.len() - 2].1["delta"]["stop_reason"], "end_turn");
    // Tool use, streamed: input_json_delta pieces rebuild the input.
    let tools =
        json!([{"name":"get_weather","description":"Weather","input_schema":{"type":"object"}}]);
    let a = post(
        addr,
        "/v1/messages",
        json!({"model":"m","max_tokens":64,"stream":true,"tools":tools,
        "messages":[{"role":"user","content":"weather?"}]}),
    )
    .await;
    let ev = events(&a.body);
    let start = ev
        .iter()
        .find(|(n, d)| n == "content_block_start" && d["index"] == 1)
        .unwrap();
    assert_eq!(start.1["content_block"]["type"], "tool_use");
    assert_eq!(start.1["content_block"]["input"], json!({}));
    assert!(start.1["content_block"]["id"]
        .as_str()
        .unwrap()
        .starts_with("toolu_"));
    let input: String = ev
        .iter()
        .filter(|(_, d)| d["index"] == 1)
        .filter_map(|(_, d)| d["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&input).unwrap(),
        json!({"city":"Paris","days":[1,2]})
    );
    assert_eq!(ev[ev.len() - 2].1["delta"]["stop_reason"], "tool_use");
    // The tool's result comes back as a tool_result block.
    let a = post(addr, "/v1/messages", json!({"model":"m","max_tokens":64,"tools":tools,"messages":[
        {"role":"user","content":"weather?"},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Paris"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"sunny"}]}]}]}))
    .await;
    assert_eq!(a.json()["content"][0]["text"], "Tool said: sunny");
}

#[tokio::test]
async fn validation_models_count_and_key() {
    let (_state, _id, addr) = start(handlers(), None).await;
    let bad = [
        (
            json!({"model":"m","messages":[{"role":"user","content":"x"}]}),
            "max_tokens: Field required",
        ),
        (
            json!({"model":"m","max_tokens":5}),
            "messages: Field required",
        ),
        (
            json!({"model":"m","max_tokens":0,"messages":[{"role":"user","content":"x"}]}),
            "max_tokens",
        ),
        (
            json!({"model":"m","max_tokens":5,"messages":[{"role":"robot","content":"x"}]}),
            "messages.0.role",
        ),
        (
            json!({"max_tokens":5,"messages":[{"role":"user","content":"x"}]}),
            "model: Field required",
        ),
        (
            json!({"model":"m","max_tokens":5,"messages":[{"role":"user","content":7}]}),
            "messages.0.content",
        ),
    ];
    for (body, needle) in bad {
        let v = assert_error(
            &post(addr, "/v1/messages", body.clone()).await,
            400,
            "invalid_request_error",
        );
        assert!(
            v["error"]["message"].as_str().unwrap().contains(needle),
            "{body}: {v}"
        );
    }
    let a = http(addr, "POST", "/v1/messages", &[], b"{not json").await;
    assert_error(&a, 400, "invalid_request_error");
    let a = post(
        addr,
        "/v1/messages/count_tokens",
        json!({"model":"m","messages":[{"role":"user","content":"count these words"}]}),
    )
    .await;
    assert!(a.json()["input_tokens"].as_u64().unwrap() > 0, "{}", a.body);
    let a = http(addr, "GET", "/v1/models", &[], b"").await;
    assert_eq!(a.json()["data"][0]["id"], wire::DEFAULT_MODEL);
    assert_eq!(a.json()["has_more"], false);
    assert_eq!(
        http(addr, "GET", "/v1/models/claude-netget-1", &[], b"")
            .await
            .json()["type"],
        "model"
    );
    assert_error(
        &http(addr, "GET", "/v1/models/nope", &[], b"").await,
        404,
        "not_found_error",
    );
    assert_error(
        &http(addr, "GET", "/v1/nothing", &[], b"").await,
        404,
        "not_found_error",
    );
    // An API key: refused before the handler is asked, accepted from x-api-key or a bearer token.
    let params = json!({"api_key":"sk-right","models":["a","b"]});
    let (_s, _i, locked) = start(handlers(), Some(params)).await;
    let body =
        json!({"model":"m","max_tokens":5,"messages":[{"role":"user","content":"x"}]}).to_string();
    for key in [None, Some("sk-wrong"), Some("sk-righ"), Some("sk-right-")] {
        let h: Vec<(&str, &str)> = key.map(|k| vec![("x-api-key", k)]).unwrap_or_default();
        assert_error(
            &http(locked, "POST", "/v1/messages", &h, body.as_bytes()).await,
            401,
            "authentication_error",
        );
    }
    assert_eq!(
        http(
            locked,
            "POST",
            "/v1/messages",
            &[("x-api-key", "sk-right")],
            body.as_bytes()
        )
        .await
        .status,
        200
    );
    assert_eq!(
        http(
            locked,
            "POST",
            "/v1/messages",
            &[("authorization", "Bearer sk-right")],
            body.as_bytes()
        )
        .await
        .status,
        200
    );
    let a = http(
        locked,
        "GET",
        "/v1/models",
        &[("x-api-key", "sk-right")],
        b"",
    )
    .await;
    assert_eq!(a.json()["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn bounds_and_fail_closed() {
    let (_state, _id, addr) = start(handlers(), None).await;
    // One byte over the body bound.
    let a = http(
        addr,
        "POST",
        "/v1/messages",
        &[("content-type", "application/json")],
        &vec![b' '; wire::MAX_BODY + 1],
    )
    .await;
    assert_error(&a, 413, "request_too_large");
    // At the bound itself the body is read (and is not JSON).
    let a = http(
        addr,
        "POST",
        "/v1/messages",
        &[],
        &vec![b' '; wire::MAX_BODY],
    )
    .await;
    assert_error(&a, 400, "invalid_request_error");
    let ask = |model: &str| json!({"model":model,"max_tokens":5,"messages":[{"role":"user","content":"x"}]});
    // The handler's own refusal, with its type's status.
    let v = assert_error(
        &post(addr, "/v1/messages", ask("deny-me")).await,
        429,
        "rate_limit_error",
    );
    assert_eq!(v["error"]["message"], "slow down");
    // Silence is never a fabricated message.
    let v = assert_error(
        &post(addr, "/v1/messages", ask("silent")).await,
        500,
        "api_error",
    );
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("netget:"),
        "{v}"
    );
    // No handler and no reachable model: an API error naming nothing internal.
    let (_s, _i, bare) = start(vec![], None).await;
    let a = post(bare, "/v1/messages", ask("m")).await;
    assert!(a.status == 500 || a.status == 529, "{}", a.body);
    let v = a.json();
    assert!(["api_error", "overloaded_error"].contains(&v["error"]["type"].as_str().unwrap()));
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("netget:"),
        "{v}"
    );
    assert!(!a.body.contains("127.0.0.1:1"), "{}", a.body);
    // A reply the executor refuses (an unknown stop reason) fails closed too.
    let bad = vec![
        json!({"event_pattern":"anthropic_message","handler":{"type":"static","actions":[
        {"type":"anthropic_reply","text":"x","stop_reason":"bored"}]}}),
    ];
    let (_s, _i, strict) = start(bad, None).await;
    assert_error(
        &post(strict, "/v1/messages", ask("m")).await,
        500,
        "api_error",
    );
}
