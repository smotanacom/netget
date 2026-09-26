//! Helpers shared by the Beanstalkd suites: an in-process `AppState` whose model endpoint is a
//! dead port, a server started through the same `ServerForm` path the dashboard uses, a
//! deterministic queue as a Python script handler, and a raw line-oriented peer.

#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Not listening: any call that reaches the model fails with a transport error.
pub const DEAD_LLM: &str = "http://127.0.0.1:1";

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
    panic!("Beanstalkd server #{} never bound a port", id.as_u32());
}

/// Start a Beanstalkd server with the given handlers and startup parameters. The instruction
/// is empty, so nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "beanstalkd".to_string(),
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
    .expect("create beanstalkd server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

/// The body `reserve` hands out: non-ASCII (so the byte count differs from the character
/// count) and containing a CRLF followed by a reply line, which a server that did not count
/// the body would let the client read as a second reply.
pub const RESERVED_BODY_PREFIX: &str = "resize image 7 \u{2713} in ";
pub const RESERVED_BODY_SUFFIX: &str = "\r\nINSERTED 9";

/// A deterministic queue, as a Python script handler (no model involved).
///
/// * put → `INSERTED 1000 + body_bytes`, or `BURIED 900` for a body starting `bury-me`;
/// * reserve → job 42 whose body names the first watched tube — unless the worker watches
///   `empty`, in which case it is left waiting;
/// * job 404 does not exist; every other job command succeeds; peek finds job 42;
/// * stats / stats-tube / stats-job are fixed reports; tube `nosuch` does not exist.
pub const QUEUE_SCRIPT: &str = r#"import json, sys
i = json.load(sys.stdin)
t = i['event_type_id']
e = i['event']
def st(s, **kw):
    d = {'type': 'send_beanstalkd_status', 'status': s}
    d.update(kw)
    return [d]
if t == 'beanstalkd_put':
    if e['body'].startswith('bury-me'):
        a = [{'type': 'insert_beanstalkd_job', 'job_id': 900, 'buried': True}]
    else:
        a = [{'type': 'insert_beanstalkd_job', 'job_id': 1000 + e['body_bytes']}]
elif t == 'beanstalkd_reserve':
    if 'empty' in e['tubes']:
        a = [{'type': 'wait_for_beanstalkd_job'}]
    else:
        a = [{'type': 'reserve_beanstalkd_job', 'job_id': 42,
              'body': 'resize image 7 ✓ in ' + e['tubes'][0] + '\r\nINSERTED 9'}]
elif t == 'beanstalkd_job_command':
    c = e['command']
    if e.get('job_id') == 404:
        a = st('NOT_FOUND')
    elif c == 'delete':
        a = st('DELETED')
    elif c == 'release':
        a = st('RELEASED')
    elif c == 'bury':
        a = st('BURIED')
    elif c == 'touch':
        a = st('TOUCHED')
    elif c == 'kick-job':
        a = st('KICKED')
    elif c == 'kick':
        a = st('KICKED', count=min(e['bound'], 3))
    elif c == 'pause-tube':
        a = st('PAUSED')
    elif c == 'reserve-job':
        a = [{'type': 'reserve_beanstalkd_job', 'job_id': e['job_id'], 'body': 'reserved by id'}]
    elif c in ('peek', 'peek-ready'):
        a = [{'type': 'send_beanstalkd_found', 'job_id': 42, 'body': 'peeked in ' + e['tube']}]
    else:
        a = st('NOT_FOUND')
elif t == 'beanstalkd_stats':
    s = e['scope']
    if s == 'server':
        a = [{'type': 'send_beanstalkd_stats', 'stats': {
            'current-jobs-ready': 3, 'current-jobs-reserved': 1, 'total-jobs': 42,
            'version': '1.13', 'draining': False, 'hostname': 'netget-queue'}}]
    elif s == 'tube':
        if e['tube'] == 'nosuch':
            a = st('NOT_FOUND')
        else:
            a = [{'type': 'send_beanstalkd_stats', 'stats': {
                'name': e['tube'], 'current-jobs-ready': 2, 'pause': 0}}]
    elif s == 'job':
        a = [{'type': 'send_beanstalkd_stats', 'stats': {
            'id': e['job_id'], 'tube': 'images', 'state': 'reserved', 'pri': 10, 'ttr': 30}}]
    else:
        a = [{'type': 'send_beanstalkd_tubes', 'tubes': ['default', 'images', 'video']}]
else:
    a = []
print(json.dumps({'actions': a}))
"#;

pub fn queue_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "script", "language": "python", "code": QUEUE_SCRIPT}
    })
}

/// A raw beanstalkd peer.
pub struct Peer {
    pub reader: BufReader<TcpStream>,
}

impl Peer {
    pub async fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        Self {
            reader: BufReader::new(stream),
        }
    }

    /// Read one line, terminator included. Empty string on EOF.
    pub async fn line(&mut self, secs: u64) -> String {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(secs), self.reader.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("no line from the Beanstalkd server within {secs}s"))
            .expect("read line");
        line
    }

    /// Read exactly `n` bytes.
    pub async fn bytes(&mut self, n: usize, secs: u64) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(Duration::from_secs(secs), self.reader.read_exact(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("{n} bytes did not arrive within {secs}s"))
            .expect("read bytes");
        buf
    }

    /// Read a reply line and, if it announces one (`RESERVED id n`, `FOUND id n`, `OK n`), the
    /// `n`-byte payload and its CRLF. Returns the line and the payload.
    pub async fn reply(&mut self, secs: u64) -> (String, Option<Vec<u8>>) {
        let line = self.line(secs).await;
        let words: Vec<&str> = line.trim_end().split(' ').collect();
        let size = match words.as_slice() {
            ["RESERVED" | "FOUND", _, n] | ["OK", n] => n.parse::<usize>().ok(),
            _ => None,
        };
        let payload = match size {
            Some(n) => {
                let mut data = self.bytes(n + 2, secs).await;
                assert_eq!(
                    &data[n..],
                    b"\r\n",
                    "payload not followed by CRLF: {line:?}"
                );
                data.truncate(n);
                Some(data)
            }
            None => None,
        };
        (line, payload)
    }

    pub async fn send_raw(&mut self, data: &[u8]) {
        let stream = self.reader.get_mut();
        stream.write_all(data).await.expect("write");
        stream.flush().await.expect("flush");
    }

    pub async fn send(&mut self, command: &str) {
        self.send_raw(format!("{command}\r\n").as_bytes()).await;
    }

    /// `put <pri> 0 60 <len>\r\n<body>\r\n`
    pub async fn put(&mut self, body: &[u8]) {
        let mut data = format!("put 5 0 60 {}\r\n", body.len()).into_bytes();
        data.extend_from_slice(body);
        data.extend_from_slice(b"\r\n");
        self.send_raw(&data).await;
    }

    /// Assert nothing arrives, and the connection stays open, for `secs`.
    pub async fn assert_silent_and_open(&mut self, secs: u64, what: &str) {
        let mut buf = [0u8; 256];
        match tokio::time::timeout(Duration::from_secs(secs), self.reader.read(&mut buf)).await {
            Err(_) => {}
            Ok(Ok(0)) => panic!("closed while {what}"),
            Ok(Ok(n)) => panic!(
                "answered while {what}: {:?}",
                String::from_utf8_lossy(&buf[..n])
            ),
            Ok(Err(e)) => panic!("reset while {what}: {e}"),
        }
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
