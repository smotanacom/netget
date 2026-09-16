// Shared test utilities for NetGet E2E testing

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::sleep;

/// Result type for e2e tests
pub type E2EResult<T> = Result<T, Box<dyn std::error::Error>>;

/// How long an Ollama reachability or model-listing probe may take.
///
/// It was 2 seconds in `check_ollama_available` and 5 in `ensure_model_available`,
/// and both were below what a healthy Ollama actually costs: `/api/tags` was
/// measured at **3.85 seconds** with the service idle and healthy, and it does
/// not answer at all while the daemon is swapping models. Every one of the 45
/// `tests/llm_live/` suites gates on these calls, so the two bounds made a
/// working Ollama read as an absent one.
///
/// This is a bound on *availability*, not on a model call — nothing waits on it
/// in the common path, because the common path is the mock.
pub const OLLAMA_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// The Ollama endpoint the test harness probes.
///
/// `OLLAMA_BASE_URL` wins. The default is the **literal** loopback address
/// rather than `localhost`, so that `client_for_endpoint` recognises it as an
/// `IpAddr` and installs its resolver override: asking the system resolver about
/// a dotted quad is never meaningful, and on macOS it goes through libinfo to
/// mDNSResponder, a single system-wide daemon measured blocking for 8.25 seconds
/// under ~100 concurrent askers. `check_ollama_available` falls back to
/// `localhost` once if nothing was configured, so an Ollama bound only to `::1`
/// is still found.
pub fn ollama_base_url() -> String {
    std::env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string())
}

/// One HTTP client for every Ollama probe in this test binary.
///
/// Built through `netget`'s own `client_for_endpoint` for the literal-IP DNS
/// bypass, and built **once**: `Client::builder().build()` loads the platform
/// root store, which on macOS reads the keychain through Security.framework
/// synchronously and serialised across processes. Called on the async runtime it
/// parks a tokio worker, and under `--test-threads=100` that is exactly the
/// stall this repository already traced once in the `doh` client suite.
pub fn ollama_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| netget::llm::ollama_client::client_for_endpoint(&ollama_base_url()))
}

/// Retry a condition with exponential backoff until it succeeds or times out
///
/// # Arguments
/// * `condition` - A closure that returns Ok(T) when successful, Err otherwise
/// * `initial_delay` - Initial delay between retries (default: 50ms)
/// * `max_delay` - Maximum delay between retries (default: 1s)
/// * `timeout_duration` - Total timeout for all retries (default: 10s)
///
/// # Returns
/// * `Ok(T)` - The successful result from the condition
/// * `Err(_)` - If timeout is reached before condition succeeds
///
/// # Example
/// ```rust,ignore
/// // Wait for server to be ready
/// retry_with_backoff(
///     || async {
///         match TcpStream::connect(addr).await {
///             Ok(stream) => Ok(stream),
///             Err(e) => Err(e.into()),
///         }
///     },
///     Duration::from_millis(50),
///     Duration::from_secs(1),
///     Duration::from_secs(5),
/// ).await?;
/// ```
#[allow(dead_code)]
pub async fn retry_with_backoff<F, Fut, T, E>(
    mut condition: F,
    initial_delay: Duration,
    max_delay: Duration,
    timeout_duration: Duration,
) -> Result<T, Box<dyn std::error::Error>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::error::Error + 'static,
{
    let start = std::time::Instant::now();
    let mut delay = initial_delay;
    let mut attempts = 0;

    loop {
        attempts += 1;

        match condition().await {
            Ok(result) => {
                if attempts > 1 {
                    println!(
                        "  [RETRY] Condition succeeded after {} attempts in {:?}",
                        attempts,
                        start.elapsed()
                    );
                }
                return Ok(result);
            }
            Err(e) => {
                if start.elapsed() >= timeout_duration {
                    return Err(format!(
                        "Retry timeout after {:?} ({} attempts). Last error: {}",
                        timeout_duration, attempts, e
                    )
                    .into());
                }

                // Sleep with exponential backoff
                sleep(delay).await;
                delay = (delay * 2).min(max_delay);
            }
        }
    }
}

/// Retry a condition with default settings (50ms initial, 1s max, 10s timeout)
#[allow(dead_code)]
pub async fn retry<F, Fut, T, E>(condition: F) -> Result<T, Box<dyn std::error::Error>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::error::Error + 'static,
{
    retry_with_backoff(
        condition,
        Duration::from_millis(50),
        Duration::from_secs(1),
        Duration::from_secs(10),
    )
    .await
}

/// Get an available port for testing
pub async fn get_available_port() -> E2EResult<u16> {
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Replace {AVAILABLE_PORT} placeholders with actual available ports
pub async fn replace_port_placeholders(prompt: &str) -> E2EResult<String> {
    const PLACEHOLDER: &str = "{AVAILABLE_PORT}";

    // Count how many placeholders we need to replace
    let placeholder_count = prompt.matches(PLACEHOLDER).count();

    if placeholder_count == 0 {
        // No placeholders to replace, return original prompt
        return Ok(prompt.to_string());
    }

    // Allocate unique available ports
    let mut ports = Vec::with_capacity(placeholder_count);
    for _ in 0..placeholder_count {
        let port = get_available_port().await?;
        ports.push(port);
    }

    // Replace placeholders one by one
    let mut result = prompt.to_string();
    for port in ports {
        result = result.replacen(PLACEHOLDER, &port.to_string(), 1);
    }

    Ok(result)
}

/// Get the path to the NetGet binary
pub fn get_netget_binary_path() -> E2EResult<PathBuf> {
    // CRITICAL: Use CARGO_BIN_EXE_netget to get the binary built with the SAME features as the test
    // When running `cargo test --features redis`, cargo builds:
    // 1. The test binary with redis feature
    // 2. The netget binary as a dependency with the SAME features
    // CARGO_BIN_EXE_netget points to the correctly-featured binary, not target/debug/netget
    if let Ok(bin_path) = std::env::var("CARGO_BIN_EXE_netget") {
        let path = PathBuf::from(bin_path);
        if path.exists() {
            return Ok(path);
        }
    }

    // Fallback for manual runs (without cargo test)
    let release_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("release")
        .join("netget");

    let debug_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("netget");

    // Check which binaries exist and their modification times
    let release_exists = release_path.exists();
    let debug_exists = debug_path.exists();

    if !release_exists && !debug_exists {
        return Err("NetGet binary not found. Please run 'cargo build --release --all-features' or 'cargo build --all-features' first.".into());
    }

    // If only one exists, use it
    if release_exists && !debug_exists {
        return Ok(release_path);
    }
    if debug_exists && !release_exists {
        return Ok(debug_path);
    }

    // Both exist - use the newer one (most recently built)
    // This ensures we use the binary that matches the current build profile
    let release_mtime = std::fs::metadata(&release_path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let debug_mtime = std::fs::metadata(&debug_path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    if debug_mtime > release_mtime {
        Ok(debug_path)
    } else {
        Ok(release_path)
    }
}

/// Kill netget processes orphaned by an earlier, killed test run.
///
/// This used to be `pkill -f "target/.*/netget"`, which is precisely the command
/// `CLAUDE.md` forbids: the operator's own session is
/// `<repo>/target/release/netget --mcp`, so that pattern matched it and would
/// have killed it. It had no callers, which is the only reason it never did.
///
/// The replacement is `child_guard`'s sweeper, whose predicate requires PPID 1
/// **and** an executable under a build target directory **and** the absence of
/// any `--mcp` flag — see `tests/helpers/child_guard.rs`.
#[allow(dead_code)]
pub async fn cleanup_stray_processes() {
    #[cfg(unix)]
    {
        super::child_guard::sweep_orphaned_netget();
    }
}

/// Helper to build a simple test prompt
#[allow(dead_code)]
pub fn build_prompt(base_stack: &str, port: u16, instructions: &str) -> String {
    if port == 0 {
        format!("listen on port 0 via {}. {}", base_stack, instructions)
    } else {
        format!(
            "listen on port {} via {}. {}",
            port, base_stack, instructions
        )
    }
}

/// Wraps a test function with a timeout to prevent indefinite hangs during parallel execution
///
/// This is especially important for tests that may experience resource contention when run
/// in parallel with high thread counts (e.g., --test-threads=100).
///
/// # Arguments
/// * `test_name` - Name of the test for logging purposes
/// * `timeout` - Maximum duration before timing out (default: 120 seconds)
/// * `test_fn` - The async test function to execute
///
/// # Returns
/// * `Ok(T)` - The successful result from the test
/// * `Err(_)` - If timeout is reached before test completes
///
/// # Example
/// ```rust,ignore
/// #[tokio::test]
/// async fn test_cassandra_connection() -> E2EResult<()> {
///     with_timeout("cassandra_connection", Duration::from_secs(120), async {
///         // ... test code ...
///         Ok(())
///     }).await
/// }
/// ```
#[allow(dead_code)]
pub async fn with_timeout<F, T>(test_name: &str, timeout: Duration, test_fn: F) -> E2EResult<T>
where
    F: Future<Output = E2EResult<T>>,
{
    match tokio::time::timeout(timeout, test_fn).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Test '{}' timed out after {:?}. This may indicate resource contention during parallel execution. \
             Try running with --test-threads=1 or reducing parallel test load.",
            test_name, timeout
        ).into()),
    }
}

/// Default timeout for external client library calls (30 seconds)
pub const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Extended timeout for AWS SDK operations (90 seconds)
/// AWS SDK operations can be slower due to internal retries and connection setup
pub const AWS_SDK_CLIENT_TIMEOUT: Duration = Duration::from_secs(90);

/// Extended timeout for Cassandra operations (90 seconds)
/// Cassandra operations can be slower due to connection pooling and query complexity
pub const CASSANDRA_CLIENT_TIMEOUT: Duration = Duration::from_secs(90);

/// Wraps external async client library calls with a timeout to prevent indefinite hangs
///
/// This wrapper is specifically designed for calls to external libraries (redis-rs, scylla,
/// aws-sdk-dynamodb, etc.) that may hang indefinitely if the server doesn't respond.
///
/// # Arguments
/// * `fut` - The async future to execute (typically a client library call)
///
/// # Returns
/// * `Ok(T)` - The successful result from the future
/// * `Err(_)` - If timeout is reached (30 seconds by default)
///
/// # Example
/// ```rust,ignore
/// // Redis client call with timeout
/// let pong: String = with_client_timeout(
///     redis::cmd("PING").query_async(&mut con)
/// ).await?;
///
/// // Cassandra query with timeout
/// let rows = with_client_timeout(
///     session.query("SELECT * FROM table")
/// ).await?;
///
/// // AWS SDK call with timeout
/// let response = with_client_timeout(
///     client.put_item().send()
/// ).await?;
/// ```
#[allow(dead_code)]
pub async fn with_client_timeout<F, T, E>(fut: F) -> E2EResult<T>
where
    F: Future<Output = Result<T, E>>,
    E: std::error::Error + 'static,
{
    match tokio::time::timeout(DEFAULT_CLIENT_TIMEOUT, fut).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(format!(
            "Client operation timed out after {:?}. Server may not be responding or may be blocked.",
            DEFAULT_CLIENT_TIMEOUT
        ).into()),
    }
}

/// Wraps AWS SDK async calls with an extended timeout (90 seconds)
///
/// AWS SDK operations can be slower due to internal retries, connection setup,
/// and SDK overhead. This function provides a longer timeout than the default.
///
/// # Example
/// ```rust,ignore
/// // AWS SDK DynamoDB call with extended timeout
/// let response = with_aws_sdk_timeout(
///     client.put_item().send()
/// ).await?;
/// ```
#[allow(dead_code)]
pub async fn with_aws_sdk_timeout<F, T, E>(fut: F) -> E2EResult<T>
where
    F: Future<Output = Result<T, E>>,
    E: std::error::Error + 'static,
{
    match tokio::time::timeout(AWS_SDK_CLIENT_TIMEOUT, fut).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(format!(
            "AWS SDK operation timed out after {:?}. Server may not be responding or may be blocked.",
            AWS_SDK_CLIENT_TIMEOUT
        ).into()),
    }
}

/// Wraps Cassandra client async calls with an extended timeout (90 seconds)
///
/// Cassandra operations can be slower due to connection pooling, query compilation,
/// and CQL complexity. This function provides a longer timeout than the default.
///
/// # Example
/// ```rust,ignore
/// // Cassandra query with extended timeout
/// let rows = with_cassandra_timeout(
///     session.query("SELECT * FROM table")
/// ).await?;
/// ```
#[allow(dead_code)]
pub async fn with_cassandra_timeout<F, T, E>(fut: F) -> E2EResult<T>
where
    F: Future<Output = Result<T, E>>,
    E: std::error::Error + 'static,
{
    match tokio::time::timeout(CASSANDRA_CLIENT_TIMEOUT, fut).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(format!(
            "Cassandra operation timed out after {:?}. Server may not be responding or may be blocked.",
            CASSANDRA_CLIENT_TIMEOUT
        ).into()),
    }
}

/// Wait until the server has said, in its own output, that it is bound and listening.
///
/// This is the condition the fixed `sleep` after `start_netget_server` stood in for.
/// `start_netget_server` returns once it has *parsed* the startup conversation, and the
/// line that satisfied it can be "Updated conversation state after server changes" rather
/// than a bind confirmation — so suites slept one or two seconds and hoped. Under
/// `--test-threads=100` that hope is where this repo's load-flakes came from: the sleep is
/// a constant, the scheduling delay is not.
///
/// # Why this does not connect a socket
///
/// The obvious implementation — `TcpStream::connect` in a loop until it succeeds — is a
/// stronger observation and is **wrong here**, which cost a debugging pass to find. A TCP
/// connection the test opens is a *peer*: NNTP greets it, raises `nntp_command_received`
/// with `command: "GREETING"`, and spends an LLM call on it. Two NNTP tests failed with
/// `Rule #0 (GREETING): Expected 1 calls, got 2` — the probe had been served. Every
/// assertion in this suite is a call count, so a readiness check that itself provokes a
/// call corrupts the thing it is protecting.
///
/// A log line cannot be served. Where the confirmation is already in the buffer this
/// returns immediately, which is correct: the bind really has happened. What it adds over
/// deleting the sleep outright is that the assumption is now *checked* — a server that
/// never announced a listener fails here, loudly, instead of failing later as a refused
/// connection somewhere less obvious.
///
/// UDP, raw and off-network protocols announce themselves the same way; for anything that
/// has to wait on an *exchange* rather than a bind, use `wait_for_mocks`.
#[allow(dead_code)]
pub async fn wait_for_server_listening(
    server: &super::server::NetGetServer,
    timeout_duration: Duration,
) -> E2EResult<()> {
    const CONFIRMATIONS: &[&str] = &[
        "listening on",
        "ready on",
        "Server is running",
        "advertising",
        "Press Ctrl+C to stop",
    ];
    let start = std::time::Instant::now();
    loop {
        for needle in CONFIRMATIONS {
            if server.output_contains(needle).await {
                return Ok(());
            }
        }
        if start.elapsed() >= timeout_duration {
            let output = server.get_output().await;
            return Err(format!(
                "server never announced a listener within {:?}. Looked for {:?}. Last 20 lines:\n{}",
                timeout_duration,
                CONFIRMATIONS,
                output
                    .iter()
                    .rev()
                    .take(20)
                    .rev()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            )
            .into());
        }
        sleep(Duration::from_millis(20)).await;
    }
}
