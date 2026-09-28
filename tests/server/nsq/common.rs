//! Helpers shared by the NSQ suites: an in-process server started through `ServerForm`, a raw
//! V2 peer that writes commands and reads frames, and the handlers the suites share.

#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::server::nsq::wire::{self, Command};
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    panic!("NSQ server #{} never bound a port", id.as_u32());
}

/// Start an NSQ server with the given handlers and startup parameters and an empty
/// instruction, so nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "nsq".to_string(),
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
    .expect("create nsq server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

pub fn static_handler(pattern: &str, actions: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "event_pattern": pattern,
        "handler": {"type": "static", "actions": actions}
    })
}

/// A deterministic broker, as a Python script handler (no model involved):
///
/// * every publish is accepted, except to topic `refused`, which gets E_PUB_FAILED;
/// * every subscription is accepted;
/// * on RDY, topic `greetings` receives `hello` and `world`, and any other topic `<topic>-1`;
/// * FIN and REQ deliver nothing.
pub const BROKER_SCRIPT: &str = r#"import json, sys
i = json.load(sys.stdin)
t = i['event_type_id']
e = i['event']
if t == 'nsq_publish':
    if e['topic'] == 'refused':
        a = [{'type': 'send_nsq_error', 'code': 'E_PUB_FAILED', 'message': 'topic is closed'}]
    else:
        a = [{'type': 'send_nsq_ok'}]
elif t == 'nsq_subscribe':
    a = [{'type': 'send_nsq_ok'}]
elif t == 'nsq_ready':
    if e['topic'] == 'greetings':
        a = [{'type': 'deliver_nsq_messages', 'messages': [{'body': 'hello'}, {'body': 'world'}]}]
    else:
        a = [{'type': 'deliver_nsq_messages', 'messages': [{'body': e['topic'] + '-1'}]}]
else:
    a = []
print(json.dumps({'actions': a}))
"#;

pub fn broker_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "script", "language": "python", "code": BROKER_SCRIPT}
    })
}

pub fn manual_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "manual", "timeout_secs": 300}
    })
}

/// A raw V2 peer.
pub struct Peer {
    pub stream: TcpStream,
    buf: Vec<u8>,
}

impl Peer {
    /// Connect and send the magic.
    pub async fn connect(port: u16) -> Self {
        let mut peer = Self::connect_raw(port).await;
        peer.send(wire::MAGIC_V2).await;
        peer
    }

    /// Connect without sending anything.
    pub async fn connect_raw(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    pub async fn send(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).await.expect("write");
        self.stream.flush().await.expect("flush");
    }

    pub async fn command(&mut self, command: &Command) {
        self.send(&wire::encode_command(command)).await;
    }

    /// IDENTIFY with the given JSON body.
    pub async fn identify(&mut self, body: serde_json::Value) {
        self.command(&Command::Identify(body.to_string().into_bytes()))
            .await;
    }

    /// Read one frame, skipping heartbeats when `skip_heartbeats`. Panics on EOF or timeout.
    pub async fn frame_raw(&mut self, secs: u64, skip_heartbeats: bool) -> wire::Frame {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match wire::parse_frame(&self.buf).expect("a frame this server writes") {
                Some((frame, used)) => {
                    self.buf.drain(..used);
                    if skip_heartbeats
                        && frame.frame_type == wire::FRAME_RESPONSE
                        && frame.data == wire::HEARTBEAT
                    {
                        continue;
                    }
                    return frame;
                }
                None => {
                    let mut chunk = [0u8; 16 * 1024];
                    let n = tokio::time::timeout_at(deadline, self.stream.read(&mut chunk))
                        .await
                        .unwrap_or_else(|_| panic!("no frame from the NSQ server within {secs}s"))
                        .expect("read");
                    assert!(n > 0, "the NSQ server closed before a frame arrived");
                    self.buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }

    /// The next frame that is not a heartbeat.
    pub async fn frame(&mut self, secs: u64) -> wire::Frame {
        self.frame_raw(secs, true).await
    }

    /// Expect a response frame with this data.
    pub async fn expect_response(&mut self, data: &[u8], secs: u64) {
        let frame = self.frame(secs).await;
        assert_eq!(
            (frame.frame_type, String::from_utf8_lossy(&frame.data)),
            (wire::FRAME_RESPONSE, String::from_utf8_lossy(data)),
        );
    }

    /// Expect an error frame, returning its text.
    pub async fn expect_error(&mut self, secs: u64) -> String {
        let frame = self.frame(secs).await;
        assert_eq!(
            frame.frame_type,
            wire::FRAME_ERROR,
            "expected an error frame, got {:?}",
            String::from_utf8_lossy(&frame.data)
        );
        String::from_utf8_lossy(&frame.data).into_owned()
    }

    /// Expect a message frame.
    pub async fn expect_message(&mut self, secs: u64) -> wire::Message {
        let frame = self.frame(secs).await;
        assert_eq!(
            frame.frame_type,
            wire::FRAME_MESSAGE,
            "expected a message frame, got type {} {:?}",
            frame.frame_type,
            String::from_utf8_lossy(&frame.data)
        );
        wire::parse_message(&frame.data).expect("a message frame")
    }

    /// Everything until EOF, heartbeats included, as raw bytes after anything already buffered.
    pub async fn rest(&mut self, secs: u64) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.buf);
        tokio::time::timeout(Duration::from_secs(secs), self.stream.read_to_end(&mut out))
            .await
            .unwrap_or_else(|_| panic!("the NSQ server did not close within {secs}s"))
            .expect("read to end");
        out
    }

    /// Nothing but heartbeats (or nothing at all) arrives for `secs`, and the socket stays open.
    pub async fn quiet_for(&mut self, secs: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some((frame, used)) = wire::parse_frame(&self.buf).expect("frame") {
                self.buf.drain(..used);
                assert!(
                    frame.frame_type == wire::FRAME_RESPONSE && frame.data == wire::HEARTBEAT,
                    "expected nothing, got type {} {:?}",
                    frame.frame_type,
                    String::from_utf8_lossy(&frame.data)
                );
                continue;
            }
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Err(_) => return,
                Ok(Ok(0)) => panic!("the NSQ server closed the connection"),
                Ok(Ok(n)) => self.buf.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => panic!("read error: {e}"),
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
