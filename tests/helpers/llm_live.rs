//! Live-LLM protocol integration framework.
//!
//! Unlike the mocked E2E suites, everything here drives the **real** netget
//! binary against a **real** Ollama model and grades wire behavior end to end:
//!
//! 1. **Setup**: a natural-language prompt ("Start a TCP server on port N that
//!    echoes...") goes to the TUI LLM, which must pick the right `base_stack`,
//!    port and per-server instruction via `open_server`.
//! 2. **Request handling**: a real protocol client sends a request; the server
//!    LLM receives the network event and must answer with the right protocol
//!    action; the test asserts on the bytes that come back on the wire.
//!
//! These are model evaluations, not regression tests: without a live Ollama
//! (and the model pulled) they can only fail, so every test must gate on
//! [`live_llm_enabled`] and skip otherwise. Run with:
//!
//! ```bash
//! NETGET_USE_OLLAMA=1 ./cargo-isolated.sh test --no-default-features \
//!     --features tcp,udp,http,dns,redis --test llm_live -- --test-threads=100
//! ```
//!
//! Model selection: `NETGET_LLM_TEST_MODEL` > `OLLAMA_MODEL` > the default
//! [`DEFAULT_LIVE_MODEL`].

// helpers/ is compiled into every root test target; only llm_live.rs uses
// this module, so everything here is "dead" from the other targets' view.
#![allow(dead_code)]

use super::common::E2EResult;
use super::netget::{start_netget, NetGetConfig, NetGetInstance};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// Default model for live-LLM integration tests.
pub const DEFAULT_LIVE_MODEL: &str = "qwen3.8:27b-mlx";

/// Wall-clock bound netget puts on a single backend LLM call
/// (`--llm-request-timeout`). The binary's default of 120s is calibrated for
/// interactive use; a 27B model answering a 30k-char setup prompt has been
/// observed to legitimately need more.
pub const LLM_REQUEST_TIMEOUT_SECS: u64 = 300;

/// How long a wire client waits for the first response byte.
///
/// Must exceed [`LLM_REQUEST_TIMEOUT_SECS`], the bound netget itself puts on a
/// backend call: a client that gives up first reports "the model never
/// answered" for an exchange the server was still legitimately working on.
/// That is exactly what happened to the IRC and NNTP suites at 180s against a
/// 300s server bound, where a slow first event pushed the second past the
/// client's patience.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(LLM_REQUEST_TIMEOUT_SECS + 30);

/// After the first byte, how long a read stays idle before we consider the
/// response complete (protocols here don't frame their responses for us).
pub const IDLE_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How long setup may take. The setup conversation can run several model
/// iterations (read_documentation → open_server), each a slow live inference,
/// so this is far above the mocked-suite default of 120s.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(600);

/// Serializes live tests within this process. Without it, every test spawns
/// its netget at once and all their setup model calls queue behind the shared
/// `--ollama-lock`, so most instances blow the 120s startup timeout in
/// `wait_for_netget_startup` before their first model call even runs. Each
/// [`LiveServer`] holds the guard for its whole lifetime, so tests proceed
/// one at a time no matter what `--test-threads` says.
static LIVE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire the suite-wide serialization lock directly. For live tests that
/// talk to Ollama without spawning netget (e.g. the registry-driven prompting
/// evaluation), so they still run one at a time alongside the wire tests.
pub async fn live_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    LIVE_TEST_LOCK.lock().await
}

/// Canonicalize a stack name via the server registry, so "jsonrpc" matches
/// the registry's reported "JSON-RPC". Falls back to the input unchanged.
fn canonical_stack(name: &str) -> String {
    netget::protocol::server_registry::registry()
        .parse_from_str(name)
        .unwrap_or_else(|| name.to_string())
}

fn stacks_match(reported: &str, expected: &str) -> bool {
    reported.eq_ignore_ascii_case(expected)
        || canonical_stack(reported).eq_ignore_ascii_case(&canonical_stack(expected))
}

/// Resolve the model to use for live tests.
pub fn live_model() -> String {
    std::env::var("NETGET_LLM_TEST_MODEL")
        .or_else(|_| std::env::var("OLLAMA_MODEL"))
        .unwrap_or_else(|_| DEFAULT_LIVE_MODEL.to_string())
}

/// Gate for live-model tests. Returns `false` (after printing a skip notice)
/// unless `NETGET_USE_OLLAMA` is set. Call first in every test:
///
/// ```ignore
/// if !live_llm_enabled() { return Ok(()); }
/// ```
pub fn live_llm_enabled() -> bool {
    if std::env::var("NETGET_USE_OLLAMA").is_ok() {
        true
    } else {
        eprintln!(
            "skipped: live-model integration test, needs a real Ollama. \
             Run with NETGET_USE_OLLAMA=1 (or ./test-e2e.sh --use-ollama)."
        );
        false
    }
}

/// Fail fast with a clear message if the model is not pulled locally.
pub async fn ensure_model_available(model: &str) -> E2EResult<()> {
    let base_url =
        std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".to_string());
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/tags", base_url))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("Ollama not reachable at {}: {}", base_url, e))?;
    let tags: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Bad /api/tags response from {}: {}", base_url, e))?;
    let names: Vec<String> = tags["models"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if names.iter().any(|n| n == model) {
        Ok(())
    } else {
        Err(format!(
            "Model '{}' is not pulled in Ollama at {}. Available: {:?}. \
             Pull it or override with NETGET_LLM_TEST_MODEL / OLLAMA_MODEL.",
            model, base_url, names
        )
        .into())
    }
}

/// Builder for one live protocol test: a setup prompt for the TUI LLM plus
/// assertions about the server it must start.
pub struct LiveProtocolTest {
    protocol: String,
    setup_prompt: String,
    expected_stack: String,
    no_scripts: bool,
    log_level: String,
}

impl LiveProtocolTest {
    /// `protocol` is the registry stack name the setup must produce (e.g.
    /// "tcp", "http", "dns"). The setup prompt defaults to a bare
    /// "Start a {protocol} server on port {AVAILABLE_PORT}".
    pub fn new(protocol: impl Into<String>) -> Self {
        let protocol = protocol.into();
        Self {
            setup_prompt: format!(
                "Start a {} server on port {{AVAILABLE_PORT}}",
                protocol.to_uppercase()
            ),
            expected_stack: protocol.clone(),
            protocol,
            // Default: scripts disabled, so request handling exercises a real
            // model round-trip per network event instead of a generated
            // script. Use `allow_scripts()` to test script-mode setup.
            no_scripts: true,
            log_level: "debug".to_string(),
        }
    }

    /// Natural-language setup prompt. `{AVAILABLE_PORT}` placeholders are
    /// replaced with a free port before the prompt is sent.
    pub fn setup_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.setup_prompt = prompt.into();
        self
    }

    /// Override the stack name the started server must report (defaults to
    /// the protocol name passed to `new`).
    #[allow(dead_code)]
    pub fn expect_stack(mut self, stack: impl Into<String>) -> Self {
        self.expected_stack = stack.into();
        self
    }

    /// Permit the model to install script handlers (skips `--no-scripts`).
    #[allow(dead_code)]
    pub fn allow_scripts(mut self) -> Self {
        self.no_scripts = false;
        self
    }

    /// Raise the netget log level (e.g. "trace") for debugging a failure.
    #[allow(dead_code)]
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.log_level = level.into();
        self
    }

    /// Send the setup prompt to the real model, wait for netget to start the
    /// server, and verify the model chose the expected stack.
    pub async fn start(self) -> E2EResult<LiveServer> {
        let model = live_model();
        ensure_model_available(&model).await?;

        // One live test at a time (see LIVE_TEST_LOCK).
        let serialization_guard = LIVE_TEST_LOCK.lock().await;

        println!(
            "🤖 live-llm setup test: protocol={} model={} prompt={:?}",
            self.protocol, model, self.setup_prompt
        );

        let mut config = NetGetConfig::new(&self.setup_prompt)
            .with_model(&model)
            .with_log_level(&self.log_level)
            .with_startup_timeout(SETUP_TIMEOUT)
            .with_extra_args([
                "--llm-request-timeout".to_string(),
                LLM_REQUEST_TIMEOUT_SECS.to_string(),
            ])
            .with_ollama();
        if self.no_scripts {
            config = config.with_no_scripts(true);
        }

        let instance = start_netget(config).await?;

        // The model must have started exactly the protocol we asked for.
        let server = instance
            .servers
            .iter()
            .find(|s| stacks_match(&s.stack, &self.expected_stack))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Model did not start a '{}' server. Servers started: {:?}",
                    self.expected_stack,
                    instance
                        .servers
                        .iter()
                        .map(|s| (s.stack.clone(), s.port))
                        .collect::<Vec<_>>()
                )
            })?;
        if server.port == 0 {
            return Err(format!(
                "'{}' server started but its listening port was never confirmed",
                self.expected_stack
            )
            .into());
        }
        println!(
            "✅ setup: model started {} server #{} on port {}",
            server.stack, server.id, server.port
        );

        Ok(LiveServer {
            protocol: self.protocol,
            port: server.port,
            instance,
            _serialization_guard: serialization_guard,
        })
    }
}

/// Builder for request-handling tests. Unlike [`LiveProtocolTest`], the
/// server is started **deterministically** via `netget --server <protocol>
/// --port 0 <instruction>` — no model call happens at setup, so the test
/// exercises exactly one unpredictable behavior: the live model answering the
/// request event. Setup correctness has its own `*_setup_via_llm` tests;
/// never chain the two.
pub struct LiveRequestTest {
    protocol: String,
    instruction: String,
    log_level: String,
    server_params: Option<Value>,
}

impl LiveRequestTest {
    /// `protocol` is the registry stack name (`--server` accepts it directly).
    /// `instruction` is the server's per-request instruction, handed to the
    /// model verbatim on every network event.
    pub fn new(protocol: impl Into<String>, instruction: impl Into<String>) -> Self {
        Self {
            protocol: protocol.into(),
            instruction: instruction.into(),
            log_level: "debug".to_string(),
            server_params: None,
        }
    }

    /// Startup parameters for the server (`--server-params`), e.g. SOCKS5's
    /// `{"filter_mode": "ask_llm"}` — without which that protocol never
    /// consults the model at all.
    pub fn server_params(mut self, params: Value) -> Self {
        self.server_params = Some(params);
        self
    }

    /// Raise the netget log level (e.g. "trace") for debugging a failure.
    #[allow(dead_code)]
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.log_level = level.into();
        self
    }

    /// Start the server directly (no model involved) and return it ready for
    /// wire requests. Startup is deterministic, so the mocked-suite default
    /// startup timeout applies unchanged.
    pub async fn start(self) -> E2EResult<LiveServer> {
        let model = live_model();
        ensure_model_available(&model).await?;

        // One live test at a time (see LIVE_TEST_LOCK): request events still
        // queue behind the shared --ollama-lock across tests.
        let serialization_guard = LIVE_TEST_LOCK.lock().await;

        println!(
            "🤖 live-llm request test: protocol={} model={} instruction={:?}",
            self.protocol, model, self.instruction
        );

        // Allocate the port ourselves and treat it as authoritative: the
        // "listening on ADDR:PORT" status line races with the "Server #N
        // started" line (the status forwarder is a separate task), so the
        // startup parser cannot be relied on to learn the port. If the bind
        // fails, run_server_direct errors and no server line appears at all.
        let port = super::common::get_available_port().await?;

        let mut extra_args = vec![
            "--server".to_string(),
            self.protocol.clone(),
            "--port".to_string(),
            port.to_string(),
            "--llm-request-timeout".to_string(),
            LLM_REQUEST_TIMEOUT_SECS.to_string(),
        ];
        if let Some(params) = &self.server_params {
            extra_args.push("--server-params".to_string());
            extra_args.push(params.to_string());
        }

        let config = NetGetConfig::new(&self.instruction)
            .with_model(&model)
            .with_log_level(&self.log_level)
            .with_no_scripts(true)
            .with_extra_args(extra_args)
            .with_ollama();

        let instance = start_netget(config).await?;

        let server = instance
            .servers
            .iter()
            .find(|s| stacks_match(&s.stack, &self.protocol))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Direct start of '{}' did not report a running server. Servers: {:?}",
                    self.protocol,
                    instance
                        .servers
                        .iter()
                        .map(|s| (s.stack.clone(), s.port))
                        .collect::<Vec<_>>()
                )
            })?;
        println!(
            "✅ direct start: {} server #{} on port {} (no model call)",
            server.stack, server.id, port
        );

        Ok(LiveServer {
            protocol: self.protocol,
            port,
            instance,
            _serialization_guard: serialization_guard,
        })
    }
}

/// A live netget server started by the real model, with wire-level clients
/// for driving request scenarios.
pub struct LiveServer {
    #[allow(dead_code)]
    pub protocol: String,
    pub port: u16,
    pub instance: NetGetInstance,
    /// Held for the server's whole lifetime; released on `finish()`/drop.
    _serialization_guard: tokio::sync::MutexGuard<'static, ()>,
}

impl LiveServer {
    pub fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// TCP request/response: connect, send `payload`, read until the response
    /// goes idle or the peer closes. Errors if no byte arrives within
    /// [`FIRST_BYTE_TIMEOUT`].
    pub async fn tcp_roundtrip(&self, payload: &[u8]) -> E2EResult<Vec<u8>> {
        let mut stream = TcpStream::connect(self.addr()).await?;
        stream.write_all(payload).await?;
        stream.flush().await?;
        let response = read_until_idle(&mut stream, "response").await?;
        println!(
            "📥 tcp response ({} bytes): {:?}",
            response.len(),
            String::from_utf8_lossy(&response)
        );
        Ok(response)
    }

    /// TCP dialogue for greeting-first protocols (SMTP, POP3, FTP…): connect,
    /// read the server's greeting (itself a live model call on the
    /// connection-open event), then send `payload` and read the reply.
    /// Returns `(greeting, response)`.
    pub async fn tcp_greeting_roundtrip(&self, payload: &[u8]) -> E2EResult<(Vec<u8>, Vec<u8>)> {
        let mut stream = TcpStream::connect(self.addr()).await?;

        let greeting = read_until_idle(&mut stream, "greeting").await?;
        println!(
            "📥 greeting ({} bytes): {:?}",
            greeting.len(),
            String::from_utf8_lossy(&greeting)
        );

        stream.write_all(payload).await?;
        stream.flush().await?;
        let response = read_until_idle(&mut stream, "response").await?;
        println!(
            "📥 response ({} bytes): {:?}",
            response.len(),
            String::from_utf8_lossy(&response)
        );
        Ok((greeting, response))
    }

    /// Open a persistent TCP session for protocols whose dialogue spans
    /// several exchanges on one connection (IMAP tag correlation, IRC's
    /// NICK+USER registration burst, NNTP's greeting-then-command flow).
    pub async fn tcp_session(&self) -> E2EResult<TcpSession> {
        Ok(TcpSession {
            stream: TcpStream::connect(self.addr()).await?,
        })
    }

    /// UDP request/response: send one datagram, wait for one reply.
    pub async fn udp_roundtrip(&self, payload: &[u8]) -> E2EResult<Vec<u8>> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(self.addr()).await?;
        socket.send(payload).await?;

        let mut buf = vec![0u8; 65535];
        let n = tokio::time::timeout(FIRST_BYTE_TIMEOUT, socket.recv(&mut buf))
            .await
            .map_err(|_| {
                format!(
                    "No UDP response within {:?} (model never answered the event)",
                    FIRST_BYTE_TIMEOUT
                )
            })??;
        buf.truncate(n);
        println!(
            "📥 udp response ({} bytes): {:?}",
            buf.len(),
            String::from_utf8_lossy(&buf)
        );
        Ok(buf)
    }

    /// HTTP request via reqwest. Returns (status, body).
    pub async fn http_request(
        &self,
        method: &str,
        path: &str,
        body: Option<(&str, String)>,
    ) -> E2EResult<(u16, String)> {
        let client = reqwest::Client::builder()
            .resolve("127.0.0.1", std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .timeout(FIRST_BYTE_TIMEOUT)
            .build()?;
        let url = format!("http://{}{}", self.addr(), path);
        let mut req = client.request(method.parse::<reqwest::Method>()?, &url);
        if let Some((content_type, payload)) = body {
            req = req.header("Content-Type", content_type).body(payload);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;
        println!("📥 http {} → status={} body={:?}", url, status, body);
        Ok((status, body))
    }

    /// Count captured output lines containing `pattern`.
    pub async fn count_in_output(&self, pattern: &str) -> usize {
        self.instance
            .output_lines
            .lock()
            .await
            .iter()
            .filter(|line| line.contains(pattern))
            .count()
    }

    /// Number of per-event live model calls the server has made so far
    /// (the `call_llm` fallback path logs "LLM call for event" at debug).
    pub async fn llm_event_calls(&self) -> usize {
        self.count_in_output("LLM call for event").await
    }

    /// Assert at least one network event was answered by a live model call,
    /// not a pre-configured handler. Requires log level "debug" (the default).
    pub async fn expect_llm_answered(&self) -> E2EResult<()> {
        let calls = self.llm_event_calls().await;
        if calls > 0 {
            println!("✅ {} live model call(s) answered network events", calls);
            return Ok(());
        }
        let statics = self.count_in_output("Static handler executing").await;
        let scripts = self.count_in_output("Script executing").await;
        Err(format!(
            "No live model call answered any network event (static handler runs: {}, script \
             runs: {}). A LiveRequestTest server has no handlers, so this means the request \
             never reached the LLM path at all — check the server logs above.",
            statics, scripts
        )
        .into())
    }

    /// Stop netget, ignoring shutdown errors.
    pub async fn finish(self) -> E2EResult<()> {
        self.instance.stop().await
    }
}

/// A persistent TCP connection for multi-exchange protocol dialogues.
/// Each `read`/`exchange` waits up to [`FIRST_BYTE_TIMEOUT`] for the first
/// byte (a live model call) and then drains until idle.
pub struct TcpSession {
    stream: TcpStream,
}

impl TcpSession {
    /// Read one server message (e.g. a greeting the model produced on the
    /// connection-open event).
    pub async fn read(&mut self, what: &str) -> E2EResult<String> {
        let bytes = read_until_idle(&mut self.stream, what).await?;
        let text = String::from_utf8_lossy(&bytes).to_string();
        println!("📥 {} ({} bytes): {:?}", what, bytes.len(), text);
        Ok(text)
    }

    /// Write without waiting for a reply (e.g. IRC's NICK before USER).
    pub async fn send(&mut self, payload: &[u8]) -> E2EResult<()> {
        self.stream.write_all(payload).await?;
        self.stream.flush().await?;
        println!("📤 sent: {:?}", String::from_utf8_lossy(payload));
        Ok(())
    }

    /// Send and read the reply.
    pub async fn exchange(&mut self, payload: &[u8], what: &str) -> E2EResult<String> {
        self.send(payload).await?;
        self.read(what).await
    }
}

/// Shared read loop: first byte within [`FIRST_BYTE_TIMEOUT`], then keep
/// reading until [`IDLE_READ_TIMEOUT`] passes with no data or the peer closes.
async fn read_until_idle(stream: &mut TcpStream, what: &str) -> E2EResult<Vec<u8>> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let mut deadline = FIRST_BYTE_TIMEOUT;
    loop {
        match tokio::time::timeout(deadline, stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                data.extend_from_slice(&buf[..n]);
                deadline = IDLE_READ_TIMEOUT;
            }
            Ok(Err(e)) => {
                if data.is_empty() {
                    return Err(format!("TCP read error before any {}: {}", what, e).into());
                }
                break;
            }
            Err(_) if data.is_empty() => {
                return Err(format!(
                    "No {} within {:?} (model never answered the event)",
                    what, FIRST_BYTE_TIMEOUT
                )
                .into());
            }
            Err(_) => break,
        }
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// Response validators. All errors carry the full response so a failed model
// evaluation is diagnosable from the test output alone.
// ---------------------------------------------------------------------------

/// Decode a wire response as UTF-8 (lossy is deliberate: model output may mix
/// text with stray bytes, and the assertion should still see the text).
pub fn as_text(response: &[u8]) -> String {
    String::from_utf8_lossy(response).to_string()
}

/// Assert the response text contains `needle` (case-insensitive).
pub fn expect_contains(response: &str, needle: &str) -> E2EResult<()> {
    if response.to_lowercase().contains(&needle.to_lowercase()) {
        Ok(())
    } else {
        Err(format!(
            "Expected response to contain {:?} (case-insensitive).\nFull response:\n{}",
            needle, response
        )
        .into())
    }
}

/// Assert the response text matches `pattern`.
#[allow(dead_code)]
pub fn expect_matches(response: &str, pattern: &str) -> E2EResult<()> {
    let re = regex::Regex::new(pattern)?;
    if re.is_match(response) {
        Ok(())
    } else {
        Err(format!(
            "Expected response to match /{}/.\nFull response:\n{}",
            pattern, response
        )
        .into())
    }
}

/// Assert a non-empty response.
#[allow(dead_code)]
pub fn expect_non_empty(response: &[u8]) -> E2EResult<()> {
    if response.is_empty() {
        Err("Expected a non-empty response, got 0 bytes".into())
    } else {
        Ok(())
    }
}
