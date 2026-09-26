//! Helpers shared by the Gearman suites: an in-process server started through `ServerForm`, a
//! raw binary/admin peer, a deterministic worker as a Python script handler, and a recording TCP
//! relay for the pcap oracle.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::server::gearman::wire;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

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
    panic!("Gearman server #{} never bound a port", id.as_u32());
}

/// Start a Gearman server with the given handlers and startup parameters and an empty
/// instruction, so nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "gearman".to_string(),
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
    .expect("create gearman server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

/// A deterministic worker, as a Python script handler (no model involved):
///
/// * `reverse` reports 1/2, sends `partial:` as WORK_DATA, then completes with the workload
///   reversed;
/// * `describe` completes with `<priority>/<fg|bg>/<workload_bytes>`;
/// * `explode` ends in an exception, `boom`;
/// * anything else fails.
pub const WORKER_SCRIPT: &str = r#"import json, sys
e = json.load(sys.stdin)['event']
f = e['function']
w = e['workload']
if f == 'reverse':
    a = [{'type': 'send_gearman_status', 'numerator': 1, 'denominator': 2},
         {'type': 'send_gearman_data', 'data': 'partial:'},
         {'type': 'complete_gearman_job', 'result': w[::-1]}]
elif f == 'describe':
    a = [{'type': 'complete_gearman_job',
          'result': e['priority'] + '/' + ('bg' if e['background'] else 'fg') + '/' + str(e['workload_bytes'])}]
elif f == 'explode':
    a = [{'type': 'send_gearman_exception', 'text': 'boom'}]
else:
    a = [{'type': 'fail_gearman_job'}]
print(json.dumps({'actions': a}))
"#;

pub fn worker_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "gearman_job_submitted",
        "handler": {"type": "script", "language": "python", "code": WORKER_SCRIPT}
    })
}

pub fn manual_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "manual", "timeout_secs": 300}
    })
}

/// A request packet.
pub fn req(packet_type: u32, args: &[&[u8]]) -> Vec<u8> {
    wire::encode(true, packet_type, args)
}

/// A raw Gearman peer.
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

    pub async fn send(&mut self, bytes: &[u8]) {
        let stream = self.reader.get_mut();
        stream.write_all(bytes).await.expect("write");
        stream.flush().await.expect("flush");
    }

    /// Read one response packet: `(type, args)` as `wire::read_response` splits them, plus the
    /// raw bytes.
    pub async fn packet(&mut self, secs: u64) -> (u32, Vec<Vec<u8>>, Vec<u8>) {
        let mut header = vec![0u8; wire::HEADER_LEN];
        tokio::time::timeout(
            Duration::from_secs(secs),
            self.reader.read_exact(&mut header),
        )
        .await
        .unwrap_or_else(|_| panic!("no packet from the Gearman server within {secs}s"))
        .expect("read header");
        let size = wire::parse_header(&header).expect("response header").size as usize;
        let mut body = vec![0u8; size];
        self.reader.read_exact(&mut body).await.expect("read body");
        header.extend_from_slice(&body);
        let (t, args) = wire::read_response(&header)
            .unwrap_or_else(|| panic!("not a response this server writes: {header:?}"));
        (t, args, header)
    }

    /// Read one admin-protocol line, terminator included. Empty string on EOF.
    pub async fn line(&mut self, secs: u64) -> String {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(secs), self.reader.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("no line from the Gearman server within {secs}s"))
            .expect("read line");
        line
    }

    /// Everything until EOF.
    pub async fn rest(&mut self, secs: u64) -> Vec<u8> {
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(secs), self.reader.read_to_end(&mut out))
            .await
            .unwrap_or_else(|_| panic!("the Gearman server did not close within {secs}s"))
            .expect("read to end");
        out
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

/// One direction of relayed bytes, in the order they crossed.
#[derive(Debug, Clone)]
pub enum Chunk {
    ToServer(Vec<u8>),
    FromServer(Vec<u8>),
}

/// A TCP relay in front of `upstream` that records the first connection it carries, so the
/// pcap oracle can read exactly what crossed the wire between a real client and NetGet.
pub struct Recorder {
    pub port: u16,
    pub chunks: Arc<Mutex<Vec<Chunk>>>,
    pub done: Arc<tokio::sync::Notify>,
}

impl Recorder {
    pub async fn start(upstream: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let port = listener.local_addr().unwrap().port();
        let chunks: Arc<Mutex<Vec<Chunk>>> = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let (rec, fin) = (chunks.clone(), done.clone());
        tokio::spawn(async move {
            let mut first = true;
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let server = TcpStream::connect(("127.0.0.1", upstream))
                    .await
                    .expect("relay connect upstream");
                let record = first.then(|| (rec.clone(), fin.clone()));
                first = false;
                tokio::spawn(relay(client, server, record));
            }
        });
        Self { port, chunks, done }
    }

    /// Wait until the recorded connection has closed in both directions.
    pub async fn finished(&self, secs: u64) -> Vec<Chunk> {
        tokio::time::timeout(Duration::from_secs(secs), self.done.notified())
            .await
            .expect("the recorded connection never finished");
        self.chunks.lock().await.clone()
    }
}

type Recording = (Arc<Mutex<Vec<Chunk>>>, Arc<tokio::sync::Notify>);

async fn relay(client: TcpStream, server: TcpStream, record: Option<Recording>) {
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let rec_up = record.as_ref().map(|(c, _)| c.clone());
    let rec_down = record.as_ref().map(|(c, _)| c.clone());
    let up = async move {
        let mut buf = [0u8; 8192];
        loop {
            match cr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(r) = &rec_up {
                        r.lock().await.push(Chunk::ToServer(buf[..n].to_vec()));
                    }
                    if sw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = sw.shutdown().await;
    };
    let down = async move {
        let mut buf = [0u8; 8192];
        loop {
            match sr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(r) = &rec_down {
                        r.lock().await.push(Chunk::FromServer(buf[..n].to_vec()));
                    }
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = cw.shutdown().await;
    };
    tokio::join!(up, down);
    if let Some((_, done)) = record {
        done.notify_one();
    }
}
