//! Helpers shared by the Bolt suites: an in-process `AppState` whose model endpoint is a dead
//! port, a server started through the same `ServerForm` path the dashboard uses, a raw Bolt peer
//! built on NetGet's own PackStream codec, and a deterministic graph as a script handler.
//!
//! The raw peer is NetGet's codec talking to NetGet's codec, so what it proves is mechanics —
//! states, ordering, batching, bounds. That the bytes are acceptable to something NetGet did not
//! write is `real_client_test.rs`'s job.

#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::server::bolt::packstream::{self, Dechunker, Value};
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Not listening: any call that reaches the model fails with a transport error.
pub const DEAD_LLM: &str = "http://127.0.0.1:1";

/// What cypher-shell 2026.09 proposes: the 5.7+ manifest, 5.8 down to 5.0, 4.4 down to 4.2, 3.0.
pub const CYPHER_SHELL_PROPOSALS: [u8; 16] = [
    0x00, 0x00, 0x01, 0xFF, 0x00, 0x08, 0x08, 0x05, 0x00, 0x02, 0x04, 0x04, 0x00, 0x00, 0x00, 0x03,
];

pub async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, DEAD_LLM.to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(DEAD_LLM.to_string()))
        .await;
    state
}

pub async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("Bolt server #{} never bound a port", id.as_u32());
}

/// Start a Bolt server with the given handlers and startup parameters. The instruction is
/// empty, so nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "bolt".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params,
        event_handlers: if handlers.is_empty() {
            None
        } else {
            Some(handlers)
        },
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create bolt server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

pub fn accept_logins() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "bolt_authenticate",
        "handler": {"type": "static", "actions": [{"type": "accept_bolt_login"}]}
    })
}

/// A deterministic graph database, as a Python script handler (no model involved).
///
/// * `... RETURN n.name AS name` over Person → two rows, Alice and Bob.
/// * `RETURN n` → one Person node.
/// * `RETURN p` → a two-node path, Alice -[:ACTED_IN]-> The Matrix.
/// * `RETURN back` → the same path walked backwards (the relationship points against it).
/// * `RETURN r` → a bare relationship.
/// * `UNWIND range(1, 5)` → five rows, for PULL batching.
/// * `CREATE` → no rows, write statistics, query type `w`.
/// * `RETURN $x` → the parameter echoed back.
/// * `RETURN kinds` → one row of every plain JSON value kind.
/// * anything else → Neo.ClientError.Statement.SyntaxError.
pub const GRAPH_SCRIPT: &str = r#"import json, sys
i = json.load(sys.stdin)
e = i['event']
q = e.get('query', '')
alice = {'id': 1, 'labels': ['Person'], 'properties': {'name': 'Alice', 'born': 1990}}
matrix = {'id': 2, 'labels': ['Movie'], 'properties': {'title': 'The Matrix'}}
acted = {'id': 9, 'type': 'ACTED_IN', 'start': 1, 'end': 2, 'properties': {'role': 'Neo'}}
def rec(fields, rows, **kw):
    a = {'type': 'send_bolt_records', 'fields': fields, 'records': rows}
    a.update(kw)
    return [a]
if 'RETURN n.name AS name' in q:
    a = rec(['name'], [['Alice'], ['Bob']])
elif q.endswith('RETURN n'):
    a = rec(['n'], [[{'$node': alice}]])
elif q.endswith('RETURN p'):
    a = rec(['p'], [[{'$path': {'nodes': [alice, matrix], 'relationships': [acted]}}]])
elif q.endswith('RETURN back'):
    a = rec(['back'], [[{'$path': {'nodes': [matrix, alice], 'relationships': [acted]}}]])
elif q.endswith('RETURN r'):
    a = rec(['r'], [[{'$relationship': acted}]])
elif 'UNWIND range(1, 5)' in q:
    a = rec(['x'], [[1], [2], [3], [4], [5]])
elif q.startswith('CREATE'):
    a = rec([], [], stats={'nodes_created': 1, 'properties_set': 2, 'labels_added': 1}, query_type='w')
elif 'RETURN $x' in q:
    a = rec(['x'], [[e.get('parameters', {}).get('x')]])
elif q.endswith('RETURN kinds'):
    a = rec(['s', 'i', 'f', 'b', 'nil', 'l', 'm'], [['text', -17, 1.5, True, None, [1, 'two'], {'k': 'v'}]])
else:
    a = [{'type': 'send_bolt_failure', 'code': 'Neo.ClientError.Statement.SyntaxError', 'message': 'This graph only knows Person nodes'}]
print(json.dumps({'actions': a}))
"#;

pub fn graph_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "bolt_query",
        "handler": {"type": "script", "language": "python", "code": GRAPH_SCRIPT}
    })
}

/// A raw Bolt peer.
pub struct Peer {
    pub stream: TcpStream,
    dechunker: Dechunker,
}

impl Peer {
    pub async fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        Self {
            stream,
            dechunker: Dechunker::new(64 * 1024 * 1024),
        }
    }

    /// Send the magic and four proposals; return the server's 4-byte answer.
    pub async fn handshake(&mut self, proposals: [u8; 16]) -> [u8; 4] {
        let mut hs = vec![0x60, 0x60, 0xB0, 0x17];
        hs.extend_from_slice(&proposals);
        self.stream.write_all(&hs).await.expect("write handshake");
        let mut answer = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(10), self.stream.read_exact(&mut answer))
            .await
            .expect("no handshake answer within 10s")
            .expect("read handshake answer");
        answer
    }

    /// Handshake as cypher-shell does, then HELLO and LOGON; asserts both succeed.
    pub async fn connect_and_login(port: u16) -> Self {
        let mut peer = Self::connect(port).await;
        assert_eq!(peer.handshake(CYPHER_SHELL_PROPOSALS).await, [0, 0, 8, 5]);
        peer.send(&hello()).await;
        assert_success(&peer.recv().await);
        peer.send(&logon("neo4j", "pw")).await;
        assert_success(&peer.recv().await);
        peer
    }

    pub async fn send(&mut self, message: &Value) {
        self.send_all(std::slice::from_ref(message)).await;
    }

    /// Pipelined: every message in one write, as drivers send RUN + PULL.
    pub async fn send_all(&mut self, messages: &[Value]) {
        let mut bytes = Vec::new();
        for m in messages {
            bytes.extend(packstream::message_bytes(m));
        }
        self.stream.write_all(&bytes).await.expect("write");
    }

    pub async fn send_raw(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("write raw");
    }

    /// The next message, or `None` at EOF.
    pub async fn try_recv(&mut self, secs: u64) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        let mut buf = [0u8; 8192];
        loop {
            if let Some(m) = self
                .dechunker
                .next_message()
                .expect("server message too large")
            {
                return Some(packstream::decode(&m).expect("server sent invalid PackStream"));
            }
            let n = tokio::time::timeout_at(deadline, self.stream.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("no message from the Bolt server within {secs}s"))
                .unwrap_or(0);
            if n == 0 {
                return None;
            }
            self.dechunker.push(&buf[..n]);
        }
    }

    pub async fn recv(&mut self) -> Value {
        self.try_recv(30)
            .await
            .expect("the server closed the connection instead of answering")
    }

    /// Assert the server closes (EOF) with nothing further.
    pub async fn expect_eof(&mut self, secs: u64) {
        if let Some(m) = self.try_recv(secs).await {
            panic!("expected the connection to close, got {m:?}");
        }
    }
}

pub fn msg(tag: u8, fields: Vec<Value>) -> Value {
    Value::Struct { tag, fields }
}

pub fn hello() -> Value {
    msg(
        0x01,
        vec![Value::map([
            ("user_agent", Value::string("netget-test/1.0")),
            (
                "bolt_agent",
                Value::map([("product", Value::string("netget-test/1.0"))]),
            ),
        ])],
    )
}

pub fn logon(user: &str, password: &str) -> Value {
    msg(
        0x6A,
        vec![Value::map([
            ("scheme", Value::string("basic")),
            ("principal", Value::string(user)),
            ("credentials", Value::string(password)),
        ])],
    )
}

pub fn run(query: &str) -> Value {
    run_with(query, Value::Map(Vec::new()), Value::Map(Vec::new()))
}

pub fn run_with(query: &str, params: Value, extra: Value) -> Value {
    msg(0x10, vec![Value::string(query), params, extra])
}

pub fn pull(n: i64) -> Value {
    msg(0x3F, vec![Value::map([("n", Value::Int(n))])])
}

pub fn pull_qid(n: i64, qid: i64) -> Value {
    msg(
        0x3F,
        vec![Value::map([("n", Value::Int(n)), ("qid", Value::Int(qid))])],
    )
}

pub fn discard(n: i64) -> Value {
    msg(0x2F, vec![Value::map([("n", Value::Int(n))])])
}

pub fn begin() -> Value {
    msg(0x11, vec![Value::Map(Vec::new())])
}

pub fn commit() -> Value {
    msg(0x12, vec![])
}

pub fn rollback() -> Value {
    msg(0x13, vec![])
}

pub fn reset() -> Value {
    msg(0x0F, vec![])
}

pub fn goodbye() -> Value {
    msg(0x02, vec![])
}

pub fn tag(v: &Value) -> u8 {
    match v {
        Value::Struct { tag, .. } => *tag,
        other => panic!("not a structure: {other:?}"),
    }
}

/// The metadata map of a SUCCESS or FAILURE.
pub fn meta(v: &Value) -> &Value {
    match v {
        Value::Struct { fields, .. } => fields.first().expect("no metadata field"),
        other => panic!("not a structure: {other:?}"),
    }
}

pub fn assert_success(v: &Value) {
    assert_eq!(tag(v), 0x70, "expected SUCCESS, got {v:?}");
}

pub fn assert_ignored(v: &Value) {
    assert_eq!(tag(v), 0x7E, "expected IGNORED, got {v:?}");
}

/// Assert a FAILURE with the given Neo4j code (read from `neo4j_code` on 5.7+, `code` before).
pub fn assert_failure(v: &Value, code: &str) {
    assert_eq!(tag(v), 0x7F, "expected FAILURE, got {v:?}");
    let m = meta(v);
    let got = m
        .get("neo4j_code")
        .or_else(|| m.get("code"))
        .and_then(Value::as_str);
    assert_eq!(got, Some(code), "FAILURE code: {v:?}");
}

/// The values of a RECORD.
pub fn record_values(v: &Value) -> Vec<Value> {
    assert_eq!(tag(v), 0x71, "expected RECORD, got {v:?}");
    match meta(v) {
        Value::List(items) => items.clone(),
        other => panic!("RECORD field is not a list: {other:?}"),
    }
}

/// Drain everything the server logged so far.
pub fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = rx.try_recv() {
        out.push(line);
    }
    out
}

/// Wait until a log line containing `needle` arrives; returns everything seen.
pub async fn wait_for_log(
    rx: &mut mpsc::UnboundedReceiver<String>,
    needle: &str,
    secs: u64,
) -> Vec<String> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(line)) => {
                let hit = line.contains(needle);
                seen.push(line);
                if hit {
                    return seen;
                }
            }
            _ => panic!("no log line containing {needle:?} within {secs}s; saw {seen:#?}"),
        }
    }
}
