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

/// Pulled in by path rather than through `mod helpers;` because this file needs exactly one
/// module out of the shared harness: the OS-level death tie that reaps the spawned binary
/// when this test process is killed or aborted, which `Drop` alone cannot do (see
/// `tests/spawned_netget_is_tied_test.rs`).
#[path = "helpers/child_guard.rs"]
mod child_guard;

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"guard-test","version":"1"}}}"#;

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Released only after the child has been signalled; a tie released first leaves
        // nothing watching.
        child_guard::untie_child(pid);
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
    child_guard::tie_child(child.id());
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

/// A syntactically valid `initialize` padded past the body cap through `clientInfo.name`,
/// so a server that *did* read it all would answer 200 with `serverInfo`: the refusal below
/// is then evidence of the cap, not of an unparseable body.
fn oversized_initialize() -> Vec<u8> {
    let limit = netget::mcp_stdio::http_guard::MAX_REQUEST_BODY_BYTES;
    let padding = "x".repeat(limit + 1);
    format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"{padding}","version":"1"}}}}}}"#
    )
    .into_bytes()
}

/// Write a request by hand on a raw socket and return the status the server answered, if
/// it answered at all. reqwest cannot be used here: the server refuses as soon as it knows
/// the body is too large and closes the connection, so a client still writing megabytes of
/// body sees a broken pipe before it ever reads the response. Writing stops at the first
/// error, and the status line is read from whatever the server sent before closing.
async fn raw_post(port: u16, extra_head: &str, body: &[u8]) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n{extra_head}\r\n"
    );
    stream.write_all(head.as_bytes()).await.expect("write head");
    for chunk in body.chunks(64 * 1024) {
        if stream.write_all(chunk).await.is_err() {
            break;
        }
    }
    // The status line is all that is needed; stop at the end of the header block rather
    // than waiting for a close the server may not send.
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut byte = [0u8; 4096];
    loop {
        match tokio::time::timeout_at(deadline, stream.read(&mut byte)).await {
            Ok(Ok(n)) if n > 0 => buf.extend_from_slice(&byte[..n]),
            _ => break,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    text.strip_prefix("HTTP/1.1 ")?.get(..3)?.parse().ok()
}

#[tokio::test]
async fn an_oversized_body_is_refused_and_the_server_survives() {
    let server = spawn(&[], &[]);
    let body = oversized_initialize();

    // Declared up front: refused before a byte of body is read.
    let head = format!("Content-Length: {}\r\n", body.len());
    assert_eq!(raw_post(server.port, &head, &body).await, Some(413));

    // Chunked, so nothing declares the size: the body cap makes rmcp's collect fail
    // instead of buffering it all. The connection is cut as the cap is crossed, so the
    // status may or may not make it back; what must not come back is the 200 a server
    // that read the whole body would send.
    let mut chunked = Vec::new();
    for chunk in body.chunks(1 << 20) {
        chunked.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        chunked.extend_from_slice(chunk);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let status = raw_post(server.port, "Transfer-Encoding: chunked\r\n", &chunked).await;
    assert!(
        !matches!(status, Some(200..=299)),
        "a chunked body past the cap must not be accepted: {status:?}"
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
