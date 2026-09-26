//! Helpers shared by the Zabbix suites: an in-process server started through `ServerForm`, a
//! raw `ZBXD` peer, a deterministic trapper as a Python script handler, and a recording TCP
//! relay for the pcap oracle.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::server::zabbix::wire;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    panic!("Zabbix server #{} never bound a port", id.as_u32());
}

/// Start a Zabbix trapper with the given handlers and startup parameters and an empty
/// instruction, so nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "zabbix".to_string(),
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
    .expect("create zabbix server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

/// A deterministic trapper: values whose key starts with `bad` fail, every other is processed.
pub const TRAPPER_SCRIPT: &str = r#"import json, sys
items = json.load(sys.stdin)['event']['items']
bad = sum(1 for i in items if i['key'].startswith('bad'))
print(json.dumps({'actions': [{'type': 'send_zabbix_result', 'processed': len(items) - bad, 'failed': bad}]}))
"#;

pub fn trapper_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "zabbix_sender_data",
        "handler": {"type": "script", "language": "python", "code": TRAPPER_SCRIPT}
    })
}

/// A `sender data` request body with `(host, key, value)` items.
pub fn sender_data(items: &[(&str, &str, &str)]) -> Vec<u8> {
    let data: Vec<serde_json::Value> = items
        .iter()
        .map(|(h, k, v)| serde_json::json!({"host": h, "key": k, "value": v}))
        .collect();
    serde_json::json!({"request": "sender data", "data": data, "clock": 1790000000, "ns": 5})
        .to_string()
        .into_bytes()
}

/// Send `bytes` on a fresh connection and read everything until the server closes.
pub async fn exchange(port: u16, bytes: &[u8], secs: u64) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream.write_all(bytes).await.expect("write");
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(secs), stream.read_to_end(&mut out))
        .await
        .unwrap_or_else(|_| panic!("the Zabbix server neither answered nor closed in {secs}s"))
        .expect("read");
    out
}

/// Split a response packet into its header flags, declared length and parsed JSON body.
pub fn response(packet: &[u8]) -> (u8, u64, serde_json::Value) {
    let header = wire::parse_header(packet)
        .unwrap_or_else(|e| panic!("response header refused ({e:?}): {packet:?}"));
    let body = &packet[wire::header_len(header.flags)..];
    assert_eq!(
        body.len() as u64,
        header.data_len,
        "declared length disagrees with the body"
    );
    (
        header.flags,
        header.data_len,
        serde_json::from_slice(body).expect("response body is JSON"),
    )
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
