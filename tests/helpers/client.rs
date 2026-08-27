// Client-specific test helpers

use super::common::*;
use super::mock_config::{wait_for_mock_expectations, MockLlmConfig};
use super::netget::NetGetConfig;
use std::time::Duration;
use tokio::process::Child;
use tokio::time::sleep;

/// A running NetGet client process
#[allow(dead_code)]
pub struct NetGetClient {
    /// The child process
    child: Child,
    /// Client ID
    pub id: String,
    /// Protocol name (e.g., "TCP", "HTTP")
    pub protocol: String,
    /// Remote address being connected to
    pub remote_addr: String,
    /// Local address (after connection succeeds)
    pub local_addr: Option<String>,
    /// Captured client output lines (for verification)
    pub output_lines: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Mock configuration (if mocks were used)
    mock_config: Option<MockLlmConfig>,
    /// Abort handles for background reader tasks
    stdout_reader_handle: tokio::task::JoinHandle<()>,
    stderr_reader_handle: tokio::task::JoinHandle<()>,
}

impl NetGetClient {
    /// Create a new NetGetClient instance
    #[allow(dead_code)]
    pub(crate) fn new(
        child: Child,
        id: String,
        protocol: String,
        remote_addr: String,
        local_addr: Option<String>,
        output_lines: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
        mock_config: Option<MockLlmConfig>,
        stdout_reader_handle: tokio::task::JoinHandle<()>,
        stderr_reader_handle: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            child,
            id,
            protocol,
            remote_addr,
            local_addr,
            output_lines,
            mock_config,
            stdout_reader_handle,
            stderr_reader_handle,
        }
    }

    /// Stop the client gracefully
    pub async fn stop(mut self) -> E2EResult<()> {
        // Try to stop gracefully with Ctrl+C
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;

            if let Some(pid) = self.child.id() {
                let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGINT);
            }
        }

        // Give it time to shutdown gracefully
        let shutdown = async {
            sleep(Duration::from_millis(500)).await;
            self.child.wait().await
        };

        let result = match tokio::time::timeout(Duration::from_secs(5), shutdown).await {
            Ok(Ok(_)) => Ok(()),
            _ => {
                // Force kill if graceful shutdown failed
                self.child.kill().await?;
                Ok(())
            }
        };

        // Abort background reader tasks to prevent hanging
        self.stdout_reader_handle.abort();
        self.stderr_reader_handle.abort();

        // Wait briefly for tasks to abort
        let _ = tokio::time::timeout(Duration::from_millis(100), async {
            let _ = (&mut self.stdout_reader_handle).await;
            let _ = (&mut self.stderr_reader_handle).await;
        })
        .await;

        result
    }

    /// Check if the client is still running
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Check if output contains a specific string
    /// Wait until ANY of `needles` appears in the output, or the deadline passes.
    ///
    /// Returns quietly on timeout rather than erroring: callers use it immediately before
    /// an `assert!` that already reports the condition and dumps the output, so failing
    /// here would replace a good message with a worse one. Its job is to remove the race,
    /// not to do the asserting.
    ///
    /// These suites used to wait with a fixed `sleep` and then assert. One second is enough
    /// when a test runs alone and not when a hundred run together, so they reported a client
    /// as never connecting when it simply had not connected yet.
    #[allow(dead_code)]
    pub async fn wait_for_any(&self, needles: &[&str], timeout_secs: u64) {
        let start = std::time::Instant::now();
        let deadline = std::time::Duration::from_secs(timeout_secs);
        loop {
            {
                let lines = self.output_lines.lock().await;
                if lines.iter().any(|l| needles.iter().any(|n| l.contains(n))) {
                    return;
                }
            }
            if start.elapsed() >= deadline {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    pub async fn output_contains(&self, needle: &str) -> bool {
        let lines = self.output_lines.lock().await;
        lines.iter().any(|line| line.contains(needle))
    }

    /// Count occurrences of a pattern in output
    pub async fn count_in_output(&self, needle: &str) -> usize {
        let lines = self.output_lines.lock().await;
        lines.iter().filter(|line| line.contains(needle)).count()
    }

    /// Get all output lines
    pub async fn get_output(&self) -> Vec<String> {
        self.output_lines.lock().await.clone()
    }

    /// Wait until every mock expectation is satisfied, or `timeout_secs` elapses.
    ///
    /// The thing a protocol exchange actually finishes with is the last LLM call it
    /// provokes, and that is what the expectations describe -- so this waits on the
    /// exchange itself rather than on a sleep long enough to probably cover it. A test
    /// that slept 2s and then verified was reporting "expected 1, got 0" for a step that
    /// completed a few hundred milliseconds later.
    ///
    /// Returns quietly on timeout: `verify_mocks` is still the thing that asserts, and it
    /// reports which rule fell short. This only removes the race.
    #[allow(dead_code)]
    pub async fn wait_for_mocks(&self, timeout_secs: u64) {
        let Some(ref mock_config) = self.mock_config else {
            return;
        };
        wait_for_mock_expectations(mock_config, timeout_secs).await;
    }

    /// Verify all mock expectations were met
    ///
    /// Must be called before dropping the client if mocks were configured.
    /// Fails if any expectation is not met.
    pub async fn verify_mocks(&self) -> E2EResult<()> {
        let Some(ref mock_config) = self.mock_config else {
            // No mocks configured, nothing to verify
            return Ok(());
        };

        // Mark as verified
        mock_config.mark_verified();

        let mut errors = Vec::new();

        for (idx, rule) in mock_config.rules.iter().enumerate() {
            let actual = rule.actual_calls.load(std::sync::atomic::Ordering::SeqCst);

            // Check exact count
            if let Some(expected) = rule.expected_calls {
                if actual != expected {
                    errors.push(format!(
                        "Rule #{} ({}): Expected {} calls, got {}",
                        idx,
                        rule.describe(),
                        expected,
                        actual
                    ));
                }
            }

            // Check minimum
            if let Some(min) = rule.min_calls {
                if actual < min {
                    errors.push(format!(
                        "Rule #{} ({}): Expected at least {} calls, got {}",
                        idx,
                        rule.describe(),
                        min,
                        actual
                    ));
                }
            }

            // Check maximum
            if let Some(max) = rule.max_calls {
                if actual > max {
                    errors.push(format!(
                        "Rule #{} ({}): Expected at most {} calls, got {}",
                        idx,
                        rule.describe(),
                        max,
                        actual
                    ));
                }
            }
        }

        // Harness diagnostics are informational: they explain *why* counts are
        // off, but on their own they do not fail a test that otherwise met its
        // expectations.
        let harness_report = mock_config.harness_diagnostics_report().await;

        if !errors.is_empty() {
            // Print detailed diagnostics
            eprintln!("\n❌ Mock verification failed:");
            for error in &errors {
                eprintln!("  {}", error);
            }
            if let Some(ref report) = harness_report {
                eprintln!();
                eprint!("{}", report);
                errors.push(report.trim_end().to_string());
            }
            eprintln!("\nAll LLM call history:");
            let history = mock_config.call_history.lock().await;
            for (idx, call) in history.iter().enumerate() {
                eprintln!("  Call #{}: {}", idx + 1, call.describe());
            }

            return Err(format!("Mock verification failed: {} errors", errors.len()).into());
        }

        Ok(())
    }

    /// Wait for a log line containing the exact pattern with timeout
    pub async fn wait_for_pattern(&self, pattern: &str, timeout: Duration) -> E2EResult<String> {
        let start = std::time::Instant::now();
        loop {
            {
                let lines = self.output_lines.lock().await;
                if let Some(line) = lines.iter().find(|line| line.contains(pattern)) {
                    return Ok(line.clone());
                }
            }
            if start.elapsed() >= timeout {
                let lines = self.output_lines.lock().await;
                return Err(format!(
                    "Timeout waiting for pattern '{}' after {:?}.\nLast 20 lines:\n{}",
                    pattern,
                    timeout,
                    lines
                        .iter()
                        .rev()
                        .take(20)
                        .rev()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for a log line matching a regex pattern with timeout
    pub async fn wait_for_regex(
        &self,
        regex: &regex::Regex,
        timeout: Duration,
    ) -> E2EResult<String> {
        let start = std::time::Instant::now();
        loop {
            {
                let lines = self.output_lines.lock().await;
                if let Some(line) = lines.iter().find(|line| regex.is_match(line)) {
                    return Ok(line.clone());
                }
            }
            if start.elapsed() >= timeout {
                return Err(
                    format!("Timeout waiting for regex pattern after {:?}", timeout).into(),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for multiple patterns in order with timeout
    pub async fn wait_for_patterns(
        &self,
        patterns: &[&str],
        timeout: Duration,
    ) -> E2EResult<Vec<String>> {
        let mut results = Vec::new();
        for pattern in patterns {
            let line = self.wait_for_pattern(pattern, timeout).await?;
            results.push(line);
        }
        Ok(results)
    }
}

impl Drop for NetGetClient {
    fn drop(&mut self) {
        // Abort background reader tasks to prevent hanging
        self.stdout_reader_handle.abort();
        self.stderr_reader_handle.abort();

        if let Some(ref mock_config) = self.mock_config {
            if !mock_config.is_verified() {
                super::mock_config::report_unverified_on_drop("client", mock_config);
            }
        }
    }
}

/// Start a NetGet client with the given configuration
/// Asserts exactly 1 client and 0 servers were started
#[allow(dead_code)]
pub async fn start_netget_client(config: NetGetConfig) -> E2EResult<NetGetClient> {
    let instance = super::netget::start_netget(config).await?;

    // Validate expectations
    if !instance.servers.is_empty() {
        return Err(format!(
            "Expected 0 servers, got {}. Prompt started unexpected servers.",
            instance.servers.len()
        )
        .into());
    }

    if instance.clients.len() != 1 {
        return Err(format!(
            "Expected exactly 1 client, got {}. Use start_netget() for multiple clients.",
            instance.clients.len()
        )
        .into());
    }

    // Use ManuallyDrop to prevent Drop from running when we move fields out
    let mut instance = std::mem::ManuallyDrop::new(instance);
    let client = instance.clients.drain(..).next().unwrap();

    // SAFETY: We're manually managing the lifecycle. The fields are moved to NetGetClient
    // which has its own Drop implementation that will clean them up.
    Ok(NetGetClient::new(
        unsafe { std::ptr::read(&instance.child) },
        client.id,
        client.protocol,
        client.remote_addr,
        client.local_addr,
        instance.output_lines.clone(),
        unsafe { std::ptr::read(&instance.mock_config) },
        unsafe { std::ptr::read(&instance.stdout_reader_handle) },
        unsafe { std::ptr::read(&instance.stderr_reader_handle) },
    ))
}

/// Wait for client to be ready and responsive
///
/// This function waits for the client to have specific output indicating it's ready.
/// The client is considered ready when output contains "connected" or similar messages.
///
/// # Arguments
/// * `client` - The NetGetClient instance to check
/// * `timeout_duration` - Maximum time to wait for client readiness
///
/// # Returns
/// * `Ok(())` if client is ready within timeout
/// * `Err(_)` if timeout expires before client is ready
///
/// # Example
/// ```rust,ignore
/// let client = start_netget_client(config).await?;
/// wait_for_client_startup(&client, Duration::from_secs(10)).await?;
/// ```
#[allow(dead_code)]
pub async fn wait_for_client_startup(
    client: &NetGetClient,
    timeout_duration: Duration,
) -> E2EResult<()> {
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        // Check if client output contains connection confirmation
        if client.output_contains("connected").await {
            println!(
                "  [WAIT] Client {} ready after {:?}",
                client.protocol,
                start.elapsed()
            );
            return Ok(());
        }

        // Check for common ready indicators
        if client.output_contains("Client is connected").await
            || client.output_contains("Connection established").await
        {
            println!("  [WAIT] Client ready after {:?}", start.elapsed());
            return Ok(());
        }

        // Small delay before checking again
        sleep(Duration::from_millis(100)).await;
    }

    // Timeout - show output for debugging
    let output = client.get_output().await;
    eprintln!("  [ERROR] Client startup timeout. Last 20 lines of output:");
    for line in output.iter().rev().take(20).rev() {
        eprintln!("    {}", line);
    }

    Err(format!("Client did not become ready within {:?}", timeout_duration).into())
}

/// Assert that the client is using the expected protocol
///
/// # Arguments
/// * `client` - The NetGetClient instance to check
/// * `expected_protocol` - Expected protocol name (e.g., "TCP", "HTTP")
///
/// # Panics
/// * If the client is not using the expected protocol
///
/// # Example
/// ```rust,ignore
/// let client = start_netget_client(config).await?;
/// assert_protocol(&client, "TCP");
/// ```
#[allow(dead_code)]
pub fn assert_protocol(client: &NetGetClient, expected_protocol: &str) {
    assert_eq!(
        client.protocol, expected_protocol,
        "Expected protocol '{}' but got '{}'",
        expected_protocol, client.protocol
    );
}

/// Get all captured output lines from the client
///
/// This is a convenience function that returns owned Vec<String> instead of
/// requiring async/await for simple access patterns.
///
/// # Arguments
/// * `client` - The NetGetClient instance
///
/// # Returns
/// * Vector of output lines captured since client start
///
/// # Example
/// ```rust,ignore
/// let client = start_netget_client(config).await?;
/// tokio::time::sleep(Duration::from_secs(2)).await;
/// let output = get_client_output(&client).await;
/// assert!(output.iter().any(|line| line.contains("connected")));
/// ```
#[allow(dead_code)]
pub async fn get_client_output(client: &NetGetClient) -> Vec<String> {
    client.get_output().await
}
