//! A running server with a model-call counter, for the per-protocol `max_inbound_bytes` tests.
//!
//! `tests/max_inbound_bytes_bound_plus_one_test.rs` probes every declared bound generically, with
//! `bound + 1` raw bytes (or an HTTP body) — which for a length-prefixed protocol is refused by
//! whatever reads the first bytes, not by the declared bound. The per-protocol
//! `inbound_limit_test.rs` files construct a message that declares exactly `bound` and
//! `bound + 1` **in the protocol's own vocabulary**, and this is the shared part of that: a real
//! server started through `ServerForm` exactly as the dashboard and MCP start one, whose model is
//! an in-process mock that answers everything with no actions and counts what reaches it.
//!
//! The instruction is non-empty and there are no `event_handlers`, so the model is the only thing
//! that could answer: a zero count is the bound's doing and nothing else's. (`ServerForm::create`
//! substitutes a default instruction for `None`, and an empty one would make the server
//! model-free, so neither is used.)

#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::mock_builder::MockLlmBuilder;
use super::mock_ollama::MockOllamaServer;

pub struct InboundLimitServer {
    pub state: AppState,
    pub mock: MockOllamaServer,
    pub server_id: ServerId,
    pub port: u16,
}

impl InboundLimitServer {
    /// Start `protocol` on an ephemeral loopback port with `startup_params`.
    pub async fn start(protocol: &str, startup_params: Option<serde_json::Value>) -> Self {
        Self::try_start(protocol, startup_params)
            .await
            .unwrap_or_else(|e| panic!("start {protocol}: {e}"))
    }

    /// As [`Self::start`], returning the startup error instead of panicking — for asserting
    /// that a parameter past a hard ceiling is refused.
    pub async fn try_start(
        protocol: &str,
        startup_params: Option<serde_json::Value>,
    ) -> Result<Self, String> {
        Self::try_start_with_mock(
            protocol,
            startup_params,
            MockLlmBuilder::new()
                .on_any()
                .respond_with_actions(serde_json::json!([]))
                .expect_at_least(0)
                .build(),
        )
        .await
    }

    /// As [`Self::try_start`], with the model's answers supplied — for a protocol whose bound
    /// sits behind a step the model must approve (an SMB login). Every rule should still count
    /// rather than constrain (`expect_at_least(0)`), and the last should be a catch-all.
    pub async fn try_start_with_mock(
        protocol: &str,
        startup_params: Option<serde_json::Value>,
        mock_config: super::mock_config::MockLlmConfig,
    ) -> Result<Self, String> {
        let mock = MockOllamaServer::start(mock_config)
            .await
            .map_err(|e| format!("mock ollama: {e}"))?;

        let state = AppState::new_with_options(false, mock.base_url());
        state
            .set_llm_client(netget::llm::OllamaClient::new(mock.base_url()))
            .await;
        let (tx, _rx) = mpsc::unbounded_channel();

        let server_id = ServerForm {
            protocol: protocol.to_string(),
            port: Some(0),
            host: Some("127.0.0.1".to_string()),
            instruction: Some("Answer whatever arrives.".to_string()),
            startup_params,
            ..Default::default()
        }
        .create(&state, tx)
        .await
        .map_err(|e| e.to_string())?;

        for _ in 0..300 {
            if let Some(s) = state.get_server(server_id).await {
                if let Some(addr) = s.local_addr {
                    return Ok(Self {
                        state,
                        mock,
                        server_id,
                        port: addr.port(),
                    });
                }
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Err(format!("{protocol} never bound a port"))
    }

    /// Connect, retrying until the accept loop is running. A connect refused in the gap between
    /// bind and accept would read exactly like a refusal — a false pass.
    pub async fn connect(&self) -> TcpStream {
        for _ in 0..200 {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", self.port)).await {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never accepted a connection on port {}", self.port);
    }

    /// The model-call count once it has stopped moving.
    ///
    /// Taken before and after the message under test, so a call still in flight from an earlier
    /// step is not charged to the wrong one.
    pub async fn settled_calls(&self) -> usize {
        let ceiling = std::time::Instant::now() + Duration::from_secs(10);
        let mut last = self.mock.call_count().await;
        let mut stable_since = std::time::Instant::now();
        while std::time::Instant::now() < ceiling {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let now = self.mock.call_count().await;
            if now != last {
                last = now;
                stable_since = std::time::Instant::now();
                continue;
            }
            if stable_since.elapsed() >= Duration::from_millis(800) {
                break;
            }
        }
        last
    }

    /// Wait until the model has been called more than `baseline` times, or `secs` pass.
    pub async fn wait_for_calls_above(&self, baseline: usize, secs: u64) -> usize {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let now = self.mock.call_count().await;
            if now > baseline || std::time::Instant::now() >= deadline {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    pub async fn stop(self) {
        let _ = self.state.remove_server(self.server_id).await;
    }
}

/// The status line and headers of an HTTP response, for an assertion message that should not
/// print a megabyte of body.
pub fn head_of(response: &str) -> &str {
    match response.find("\r\n\r\n") {
        Some(end) => &response[..end],
        None => truncate_str(response, 400),
    }
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// A hand-written RFC 6455 client, for the WebSocket servers' bound tests.
///
/// Hand-written rather than tokio-tungstenite because the refusal is decided on a frame
/// **header**: to show that the server refuses on the length a header declares, the test must
/// be able to send that header and nothing after it, which no WebSocket library will do. It
/// also means the server's close frame is read off a socket with nothing unread on the server
/// side, so the close cannot be turned into a reset.
pub mod ws {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// Perform the HTTP upgrade and consume the 101 response.
    pub async fn upgrade(stream: &mut TcpStream) {
        stream
            .write_all(
                b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
                  Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 13\r\n\r\n",
            )
            .await
            .expect("write upgrade");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte))
                .await
                .expect("upgrade response timed out")
                .expect("read upgrade response");
            assert!(n == 1, "connection closed during the upgrade");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head);
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "expected 101 Switching Protocols, got {head:?}"
        );
    }

    /// A masked client frame header (FIN set) declaring `len` payload bytes, mask key zero so
    /// the payload goes on the wire as-is.
    pub fn header(opcode: u8, len: u64) -> Vec<u8> {
        let mut h = vec![0x80 | opcode];
        if len < 126 {
            h.push(0x80 | len as u8);
        } else if len <= u16::MAX as u64 {
            h.push(0x80 | 126);
            h.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            h.push(0x80 | 127);
            h.extend_from_slice(&len.to_be_bytes());
        }
        h.extend_from_slice(&[0, 0, 0, 0]);
        h
    }

    /// A complete masked text frame.
    pub fn text(payload: &[u8]) -> Vec<u8> {
        let mut f = header(0x1, payload.len() as u64);
        f.extend_from_slice(payload);
        f
    }

    /// Read one server frame: `Some((opcode, payload))`, or `None` on EOF, error or timeout.
    pub async fn read_frame(stream: &mut TcpStream, secs: u64) -> Option<(u8, Vec<u8>)> {
        let fut = async {
            let mut h = [0u8; 2];
            stream.read_exact(&mut h).await.ok()?;
            let opcode = h[0] & 0x0f;
            let mut len = (h[1] & 0x7f) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                stream.read_exact(&mut b).await.ok()?;
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                stream.read_exact(&mut b).await.ok()?;
                len = u64::from_be_bytes(b);
            }
            let mut payload = vec![0u8; len as usize];
            stream.read_exact(&mut payload).await.ok()?;
            Some((opcode, payload))
        };
        tokio::time::timeout(Duration::from_secs(secs), fut)
            .await
            .ok()
            .flatten()
    }

    /// After a close frame, whether the server also ends the TCP connection.
    pub async fn closed_within(stream: &mut TcpStream, secs: u64) -> bool {
        let mut buf = [0u8; 1024];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => continue,
                Err(_) => return false,
            }
        }
    }
}
