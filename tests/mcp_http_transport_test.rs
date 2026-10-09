//! `--mcp-http` end to end: the real binary, a real socket, plain HTTP requests.
//!
//! Proves the admission policy in `src/mcp_stdio/http_guard.rs` is in front of rmcp's
//! transport — a DNS-rebound or cross-origin page is answered 403, a missing token
//! 401, an oversized body 413 — and that a legitimate `initialize` still completes
//! through the middleware. No model: `initialize` and `tools/list` never call one.
//!
//! Run with:
//!   cargo test --no-default-features --features mcp-http,tcp --test mcp_http_transport_test
#![cfg(feature = "mcp-http")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"guard-test","version":"1"}}}"#;

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn `netget --mcp-http 0 <extra>` with `env` set, and read the bound port out of
/// its own "listening on" log line.
fn spawn(extra: &[&str], env: &[(&str, &str)]) -> Server {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_netget"));
    cmd.args(["--mcp-http", "0", "--log-level", "info"])
        .args(extra)
        .env_remove("NETGET_MCP_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn netget --mcp-http");
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(rest) = line.split("listening on http://").nth(1) {
                if let Some(port) = rest
                    .split('/')
                    .next()
                    .and_then(|a| a.rsplit(':').next())
                    .and_then(|p| p.parse::<u16>().ok())
                {
                    let _ = tx.send(port);
                }
            }
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("netget --mcp-http never logged its listening address");
    Server { child, port }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

async fn post(
    server: &Server,
    headers: &[(&str, &str)],
    body: impl Into<reqwest::Body>,
) -> reqwest::Response {
    let mut req = client()
        .post(format!("http://127.0.0.1:{}/mcp", server.port))
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(body).send().await.expect("request")
}

async fn assert_initialize_succeeds(server: &Server, headers: &[(&str, &str)]) {
    let response = post(server, headers, INITIALIZE).await;
    assert_eq!(response.status(), 200, "initialize through the middleware");
    let text = response.text().await.unwrap();
    assert!(
        text.contains("\"serverInfo\"") && text.contains("netget"),
        "initialize result should name the server: {text}"
    );
}

#[tokio::test]
async fn loopback_mode_admits_local_requests_and_refuses_rebound_or_foreign_ones() {
    let server = spawn(&[], &[]);

    // A real local client (Host is what reqwest sets: 127.0.0.1:port).
    assert_initialize_succeeds(&server, &[]).await;
    assert_initialize_succeeds(&server, &[("Origin", "http://localhost:5173")]).await;

    // DNS rebinding: the page's own hostname arrives in Host.
    let response = post(&server, &[("Host", "attacker.example")], INITIALIZE).await;
    assert_eq!(response.status(), 403);

    // A cross-origin page that somehow got past the preflight.
    let response = post(
        &server,
        &[("Origin", "http://attacker.example")],
        INITIALIZE,
    )
    .await;
    assert_eq!(response.status(), 403);

    // The refusals did not take the server down.
    assert_initialize_succeeds(&server, &[]).await;
}

#[tokio::test]
async fn token_mode_requires_the_bearer_token() {
    let server = spawn(&[], &[("NETGET_MCP_TOKEN", "hunter2")]);

    let response = post(&server, &[], INITIALIZE).await;
    assert_eq!(response.status(), 401, "no token");
    let response = post(&server, &[("Authorization", "Bearer hunter3")], INITIALIZE).await;
    assert_eq!(response.status(), 401, "wrong token");

    // With the token, a foreign Host/Origin is fine: a rebound page cannot know it.
    assert_initialize_succeeds(
        &server,
        &[
            ("Authorization", "Bearer hunter2"),
            ("Host", "attacker.example"),
            ("Origin", "http://attacker.example"),
        ],
    )
    .await;
}

#[tokio::test]
async fn an_oversized_body_is_refused_and_the_server_survives() {
    let server = spawn(&[], &[]);
    let limit = netget::mcp_stdio::http_guard::MAX_REQUEST_BODY_BYTES;

    // Declared up front: refused before a byte of body is read.
    let response = post(&server, &[], vec![b'x'; limit + 1]).await;
    assert_eq!(response.status(), 413);

    // Chunked, so nothing declares the size: the body cap makes rmcp's collect fail
    // instead of buffering it all.
    let chunks = futures::stream::iter(
        (0..8).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'{'; 1 << 20]))),
    );
    let response = post(&server, &[], reqwest::Body::wrap_stream(chunks)).await;
    assert!(
        !response.status().is_success(),
        "an 8 MiB chunked body must not be accepted: {}",
        response.status()
    );

    assert_initialize_succeeds(&server, &[]).await;
}

#[test]
fn a_non_loopback_bind_without_a_token_refuses_to_start() {
    let output = Command::new(env!("CARGO_BIN_EXE_netget"))
        .args([
            "--mcp-http",
            "0",
            "--listen-addr",
            "0.0.0.0",
            "--log-level",
            "error",
        ])
        .env_remove("NETGET_MCP_TOKEN")
        .stdin(Stdio::null())
        .output()
        .expect("run netget");
    assert!(
        !output.status.success(),
        "must not start: {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mcp-token"),
        "the refusal should name the remedy: {stderr}"
    );
}
