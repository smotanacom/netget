//! A request to a loopback endpoint must never leave the machine, even when the
//! environment names an HTTP proxy — and a request to a remote one must still use it.
//!
//! `reqwest`'s `system-proxy` feature is on by default, so every
//! `Client::builder().build()` asks `hyper_util`'s `Matcher::from_system()` for the
//! machine's proxy configuration. Two things follow, and NetGet cared about both:
//!
//! * **Correctness.** `from_system()` reads `HTTP_PROXY`/`http_proxy` and, on macOS,
//!   `kSCPropNetProxiesHTTPEnable`/`Proxy`/`Port` — and reads **no** exceptions list in
//!   either path (`NO_PROXY` aside). So a configured proxy captured
//!   `http://127.0.0.1:11434` as readily as anything else, which is how a local Ollama
//!   call ends up on somebody else's wire.
//! * **Cost.** That lookup opens an `SCDynamicStore` session against **configd**, a
//!   single system-wide daemon, and it serialises across processes the way
//!   `getaddrinfo`'s mDNSResponder does. Measured on this machine: 0.056 ms alone,
//!   **657 ms (p50) with 100 processes building a client at once**, against 0.090 ms
//!   for the same build with `.no_proxy()`. It is a *per-process* cost — the second
//!   client in a process costs ~0.3 ms whatever the concurrency — so it is not fixed by
//!   sharing clients, only by not asking.
//!
//! `client_for_endpoint` therefore calls `.no_proxy()` when, and only when, the endpoint
//! is loopback. These tests assert the "only when" as hard as the "when": a narrowing
//! that silently applied to everything would break reaching a remote model through a
//! corporate proxy, and nothing else in the suite would notice.
//!
//! The proxy setting has to be an environment variable, and the environment is
//! process-global — so each case runs in a **child process** (this same test binary,
//! re-executed) rather than mutating the environment of a shared 100-thread run.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Env var naming the URL the child should GET. Its presence is what makes the child
/// half of these tests do anything.
const TARGET_VAR: &str = "NETGET_PROXY_TEST_TARGET";

/// Accept connections forever, counting each one and answering `200 <body>`.
///
/// Returns the bound address and the counter. The counter is incremented **before** the
/// response is written, so a child that has received its response has already been
/// counted — which is what lets the parent assert a zero without sleeping for it.
fn counting_listener(body: &'static str) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            hits_thread.fetch_add(1, Ordering::SeqCst);
            // Read just the head; we never need the body, and reading to EOF would
            // block on a client waiting for our response.
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
            let _ = stream.flush();
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    (format!("http://{}", addr), hits)
}

/// Run the child half of a case: this test binary, with `target` to fetch and `proxy`
/// advertised through every environment variable `hyper_util` consults for HTTP.
/// Returns the child's stdout.
fn run_child(target: &str, proxy: &str) -> String {
    let exe = std::env::current_exe().expect("current_exe");
    let out = std::process::Command::new(exe)
        .args(["child_fetches_target", "--exact", "--nocapture"])
        .env(TARGET_VAR, target)
        .env("HTTP_PROXY", proxy)
        .env("http_proxy", proxy)
        .env("ALL_PROXY", proxy)
        .env("all_proxy", proxy)
        // NO_PROXY would make the loopback case pass for a reason that is not the one
        // under test, so it is cleared rather than inherited.
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .expect("spawn child");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn loopback_llm_endpoint_never_uses_a_configured_proxy() {
    let (origin, origin_hits) = counting_listener("ORIGIN");
    let (proxy, proxy_hits) = counting_listener("PROXY");

    let stdout = run_child(&origin, &proxy);

    assert!(
        stdout.contains("BODY=ORIGIN"),
        "the loopback request should have been answered by the origin itself.\n\
         child stdout:\n{stdout}"
    );
    assert_eq!(
        origin_hits.load(Ordering::SeqCst),
        1,
        "the origin should have been contacted exactly once directly.\n\
         child stdout:\n{stdout}"
    );
    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        0,
        "a request to 127.0.0.1 reached the configured HTTP proxy — loopback traffic \
         must never leave the machine.\nchild stdout:\n{stdout}"
    );
}

#[test]
fn remote_llm_endpoint_still_honours_a_configured_proxy() {
    let (proxy, proxy_hits) = counting_listener("PROXY");

    // TEST-NET-1 (RFC 5737), discard port: a literal IP, so no name resolution, and
    // routed nowhere. If the proxy is bypassed this cannot succeed, which is precisely
    // the failure this test is for.
    let stdout = run_child("http://192.0.2.10:9", &proxy);

    assert_eq!(
        proxy_hits.load(Ordering::SeqCst),
        1,
        "a request to a non-loopback endpoint did NOT go through the configured HTTP \
         proxy — the loopback bypass has been widened past loopback.\nchild stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("BODY=PROXY"),
        "the remote request should have been answered via the proxy.\n\
         child stdout:\n{stdout}"
    );
}

/// The child half of both cases. Inert unless [`TARGET_VAR`] is set, so it is a no-op in
/// an ordinary run of this binary.
#[test]
fn child_fetches_target() {
    let Ok(target) = std::env::var(TARGET_VAR) else {
        return;
    };
    let client = netget::llm::ollama_client::client_for_endpoint(&target);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let body = rt.block_on(async {
        match client.get(&target).send().await {
            Ok(r) => match r.text().await {
                Ok(t) => t,
                Err(e) => format!("<body error: {e}>"),
            },
            Err(e) => format!("<request error: {e}>"),
        }
    });
    println!("BODY={body}");
}

#[test]
fn is_loopback_endpoint_accepts_every_shape_host_of_produces() {
    use netget::llm::ollama_client::is_loopback_endpoint;

    for yes in [
        "127.0.0.1",
        "http://127.0.0.1",
        "http://127.0.0.1:11434",
        "http://127.0.0.1:11434/v1",
        "http://127.0.0.53:11434",
        "https://LocalHost:11434/v1",
        "localhost",
        "http://[::1]:11434",
        "http://[::1]",
    ] {
        assert!(is_loopback_endpoint(yes), "{yes} should be loopback");
    }

    // A LAN or public endpoint is somebody's network, and its proxy configuration is
    // theirs to decide. `localhostile.example.com` is the substring trap.
    for no in [
        "http://192.168.1.5:11434",
        "http://10.0.0.5:11434",
        "https://api.openai.com/v1",
        "http://localhostile.example.com:11434",
        "http://notlocalhost:11434",
        "http://[2001:db8::1]:11434",
    ] {
        assert!(!is_loopback_endpoint(no), "{no} should NOT be loopback");
    }
}
