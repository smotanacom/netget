// Shared test utilities for NetGet E2E testing

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

/// Result type for e2e tests
pub type E2EResult<T> = Result<T, Box<dyn std::error::Error>>;

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

/// Kill all running netget processes (useful for cleanup)
#[allow(dead_code)]
pub async fn cleanup_stray_processes() {
    #[cfg(unix)]
    {
        let _ = Command::new("pkill")
            .arg("-f")
            .arg("target/.*/netget")
            .output()
            .await;
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

/// Wait until a loopback TCP port accepts a connection.
///
/// This is the condition the fixed `sleep` after `start_netget_server` stood in for.
/// `start_netget_server` returns as soon as it has *parsed* the startup conversation —
/// which can be a "Updated conversation state after server changes" line rather than a
/// bound socket — so suites slept one or two seconds and hoped. Under
/// `--test-threads=100` that hope is where this repo's load-flakes came from: the sleep
/// is a constant, the scheduling delay is not.
///
/// Connecting and immediately dropping is a real observation of the accept loop, not a
/// restatement of something the harness already knew, so it is a legitimate replacement
/// rather than a wait on a condition that is already true.
///
/// Only for protocols that listen on TCP. UDP, raw and off-network protocols have no
/// such condition — wait on the exchange they provoke (`wait_for_mocks`) instead.
#[allow(dead_code)]
pub async fn wait_for_tcp_port(port: u16, timeout_duration: Duration) -> E2EResult<()> {
    let start = std::time::Instant::now();
    let addr = format!("127.0.0.1:{}", port);
    let mut last_err = String::new();
    while start.elapsed() < timeout_duration {
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(e) => last_err = e.to_string(),
        }
        sleep(Duration::from_millis(20)).await;
    }
    Err(format!(
        "port {} never accepted a connection within {:?} (last error: {})",
        port, timeout_duration, last_err
    )
    .into())
}
