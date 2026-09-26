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
        let mock = MockOllamaServer::start(
            MockLlmBuilder::new()
                .on_any()
                .respond_with_actions(serde_json::json!([]))
                .expect_at_least(0)
                .build(),
        )
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
