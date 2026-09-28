//! Helpers shared by the Nostr suites: an in-process relay started through `ServerForm` (the
//! dashboard's path), a raw WebSocket peer, events signed in the test, `nak` located or failed
//! for, and a recording TCP relay for the pcap oracle.
//!
//! The raw peer is tokio-tungstenite — the crate the server frames with — so it is used only
//! for what NetGet decides mechanically (refusals, bounds, framing of answers). The evidence
//! that the relay speaks Nostr comes from `nak` and rust-nostr in `real_client_test.rs`.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use netget::cli::management::ServerForm;
use netget::server::nostr::wire::{Event, RelayKey};
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

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
    panic!("Nostr relay #{} never bound a port", id.as_u32());
}

/// Start a relay with the given handlers and startup parameters. The instruction is empty, so
/// nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "nostr".to_string(),
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
    .expect("create nostr relay");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

/// A secret key the tests sign with, and the relay key the tests pin.
pub const AUTHOR_SECRET: &str = "7f7ff03d123792d6ac594bfa67bf6d0c0ab55b6b1fdb6249303fe861f1ccba9a";
pub const AUTHOR_PUBKEY: &str = "17162c921dc4d2518f9a101db33695df1afb56ab82f5ff3e5da6eec3ca5cd917";
pub const RELAY_SECRET: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f00112233445566778899aabbccddeeff0";

pub fn author() -> RelayKey {
    RelayKey::from_hex(AUTHOR_SECRET).expect("test key")
}

/// A signed kind-1 note.
pub fn note(content: &str, tags: Vec<Vec<String>>, created_at: u64) -> Event {
    author().sign(created_at, 1, tags, content.to_string())
}

/// `["EVENT", <event>]`
pub fn event_frame(event: &Event) -> String {
    serde_json::to_string(&serde_json::json!(["EVENT", event.to_json()])).unwrap()
}

/// Accept every event, and answer every REQ with the given events.
pub fn relay_handlers(events: serde_json::Value) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "event_pattern": "nostr_event",
            "handler": {"type": "static", "actions": [{"type": "accept_nostr_event"}]}
        }),
        serde_json::json!({
            "event_pattern": "nostr_req",
            "handler": {"type": "static", "actions": [
                {"type": "send_nostr_events", "events": events}
            ]}
        }),
    ]
}

/// A raw WebSocket peer.
pub struct Peer {
    pub ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
}

/// What the peer read next.
#[derive(Debug)]
pub enum Read {
    Json(serde_json::Value),
    Closed(Option<(u16, String)>),
}

impl Peer {
    pub async fn connect(port: u16) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/"))
            .await
            .expect("websocket upgrade");
        Self { ws }
    }

    pub async fn send(&mut self, text: &str) {
        self.ws
            .send(Message::Text(text.to_string()))
            .await
            .expect("send frame");
    }

    pub async fn send_json(&mut self, value: serde_json::Value) {
        self.send(&value.to_string()).await;
    }

    /// The next relay message or the close, skipping pings and pongs.
    pub async fn read(&mut self, secs: u64) -> Read {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let next = tokio::time::timeout_at(deadline, self.ws.next())
                .await
                .unwrap_or_else(|_| panic!("nothing from the relay within {secs}s"));
            match next {
                None => return Read::Closed(None),
                Some(Err(_)) => return Read::Closed(None),
                Some(Ok(Message::Text(t))) => {
                    return Read::Json(serde_json::from_str(&t).expect("relay sent JSON"))
                }
                Some(Ok(Message::Close(frame))) => {
                    return Read::Closed(frame.map(|f| (u16::from(f.code), f.reason.to_string())))
                }
                Some(Ok(_)) => continue,
            }
        }
    }

    /// The next relay message, which must be one.
    pub async fn json(&mut self, secs: u64) -> serde_json::Value {
        match self.read(secs).await {
            Read::Json(v) => v,
            Read::Closed(c) => panic!("the relay closed ({c:?}) where a message was expected"),
        }
    }

    /// Assert no relay message and no close arrives for `secs`. Reads the whole time, so the
    /// relay's keepalive Pings are answered as any client answers them.
    pub async fn assert_silent(&mut self, secs: u64, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => return,
                Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
                Ok(other) => panic!("the relay answered while {what}: {other:?}"),
            }
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

/// Locate a binary, or fail saying why a skip would be worse. Named `require_tool("…")` so
/// `scripts/beta_evidence_table.py` can see which third-party client a file drives.
pub fn require_tool(name: &str) -> String {
    for prefix in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let candidate = std::path::Path::new(prefix).join(name);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        if let Some(found) = path
            .split(':')
            .map(|dir| std::path::Path::new(dir).join(name))
            .find(|candidate| candidate.exists())
        {
            return found.to_string_lossy().into_owned();
        }
    }
    panic!(
        "`{name}` not found (searched /opt/homebrew/bin, /usr/local/bin, /usr/bin and $PATH). \
         These tests drive fiatjaf's nak, a Go Nostr client built on go-nostr, against NetGet's \
         relay, and it is the independent check that our OK/EOSE/EVENT frames, event ids and \
         signatures are acceptable to something we did not write. Skipping would leave the \
         Nostr evidence resting on nothing, so this is a failure and not a skip. Install with \
         `brew install nak` (macOS) or, on Linux, the release binary from \
         https://github.com/fiatjaf/nak/releases placed on PATH as `nak`."
    );
}

/// Run `nak <args>` with `stdin`; returns (exit code, stdout, stderr).
pub async fn run_nak(args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let nak = require_tool("nak");
    let mut command = tokio::process::Command::new(&nak);
    command
        .args(args)
        .kill_on_drop(true)
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn nak");
    if let Some(input) = stdin {
        let mut pipe = child.stdin.take().expect("stdin");
        pipe.write_all(input.as_bytes())
            .await
            .expect("write nak stdin");
        drop(pipe);
    }
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("nak did not finish within 60s")
        .expect("run nak");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    println!(
        "--- nak {} (exit {:?}) ---\nstdout:\n{stdout}stderr:\n{stderr}",
        args.join(" "),
        output.status.code()
    );
    (output.status.code().unwrap_or(-1), stdout, stderr)
}

/// One direction's bytes as they crossed a recorded connection.
#[derive(Debug, Clone)]
pub enum Chunk {
    ToServer(Vec<u8>),
    FromServer(Vec<u8>),
}

/// A TCP relay in front of the server that records the first connection through it.
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
