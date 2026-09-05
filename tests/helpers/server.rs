// Server-specific test helpers and backward-compatible wrappers

use super::common::*;
use super::mock_config::MockLlmConfig;
use std::time::Duration;
use tokio::process::Child;
use tokio::time::sleep;

/// A running NetGet server process (backward compatible)
/// This wrapper maintains compatibility with the original NetGetServer struct
pub struct NetGetServer {
    /// The child process
    child: Child,
    /// The port the server is listening on
    pub port: u16,
    /// The actual protocol stack that was started
    #[allow(dead_code)]
    pub stack: String,
    /// Captured server output lines (for verification)
    pub output_lines: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Mock Ollama server (for verification and lifecycle)
    /// IMPORTANT: Must NOT have underscore prefix - field must be kept alive for entire test duration
    #[allow(dead_code)]
    mock_ollama_server: Option<super::mock_ollama::MockOllamaServer>,
    /// Mock configuration (DEPRECATED - kept for backward compat, use mock_ollama_server for verification)
    mock_config: Option<MockLlmConfig>,
    /// Temporary mock config file (DEPRECATED - not used anymore)
    /// IMPORTANT: Must NOT have underscore prefix - field must be kept alive for entire test duration
    #[allow(dead_code)]
    mock_temp_file: Option<tempfile::TempPath>,
    /// Abort handles for background reader tasks
    stdout_reader_handle: tokio::task::JoinHandle<()>,
    stderr_reader_handle: tokio::task::JoinHandle<()>,
}

impl NetGetServer {
    /// Create a new NetGetServer instance
    pub(crate) fn new(
        child: Child,
        port: u16,
        stack: String,
        output_lines: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
        mock_ollama_server: Option<super::mock_ollama::MockOllamaServer>,
        mock_config: Option<MockLlmConfig>,
        mock_temp_file: Option<tempfile::TempPath>,
        stdout_reader_handle: tokio::task::JoinHandle<()>,
        stderr_reader_handle: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            child,
            port,
            stack,
            output_lines,
            mock_ollama_server,
            mock_config,
            mock_temp_file,
            stdout_reader_handle,
            stderr_reader_handle,
        }
    }

    /// Stop the server gracefully
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

    /// Check if the server is still running
    #[allow(dead_code)]
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Check if output contains a specific string
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub async fn count_in_output(&self, needle: &str) -> usize {
        let lines = self.output_lines.lock().await;
        lines.iter().filter(|line| line.contains(needle)).count()
    }

    /// Get all output lines
    pub async fn get_output(&self) -> Vec<String> {
        self.output_lines.lock().await.clone()
    }

    /// Wait for a specific log pattern to appear in the output
    /// Returns when the pattern is found or times out
    #[allow(dead_code)]
    pub async fn wait_for_log(&self, pattern: &str, timeout_secs: u64) -> E2EResult<()> {
        let start = std::time::Instant::now();
        let timeout_duration = Duration::from_secs(timeout_secs);

        loop {
            // Check if pattern exists in current output
            {
                let lines = self.output_lines.lock().await;
                if lines.iter().any(|line| line.contains(pattern)) {
                    println!("[DEBUG] Found log pattern: '{}'", pattern);
                    return Ok(());
                }
            }

            // Check timeout
            if start.elapsed() > timeout_duration {
                let lines = self.output_lines.lock().await;
                return Err(format!(
                    "Timeout waiting for log pattern '{}' after {}s\nCaptured output:\n{}",
                    pattern,
                    timeout_secs,
                    lines.join("\n")
                )
                .into());
            }

            // Sleep briefly before checking again
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Wait for a log pattern to appear N times
    #[allow(dead_code)]
    pub async fn wait_for_log_count(
        &self,
        pattern: &str,
        min_count: usize,
        timeout_secs: u64,
    ) -> E2EResult<()> {
        let start = std::time::Instant::now();
        let timeout_duration = Duration::from_secs(timeout_secs);

        loop {
            // Count occurrences in current output
            {
                let lines = self.output_lines.lock().await;
                let count = lines.iter().filter(|line| line.contains(pattern)).count();
                if count >= min_count {
                    println!(
                        "[DEBUG] Found log pattern '{}' {} times (needed {})",
                        pattern, count, min_count
                    );
                    return Ok(());
                }
            }

            // Check timeout
            if start.elapsed() > timeout_duration {
                let lines = self.output_lines.lock().await;
                let count = lines.iter().filter(|line| line.contains(pattern)).count();
                return Err(format!(
                    "Timeout waiting for log pattern '{}' to appear {} times (found {} times) after {}s",
                    pattern,
                    min_count,
                    count,
                    timeout_secs
                )
                .into());
            }

            // Sleep briefly before checking again
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Verify all mock expectations were met
    ///
    /// Must be called before dropping the server if mocks were configured.
    /// Fails if any expectation is not met.
    ///
    /// # Example
    /// ```ignore
    /// let server = start_netget_server(config).await?;
    /// // ... test logic ...
    /// server.verify_mocks().await?;  // MANDATORY if mocks configured
    /// server.stop().await?;
    /// ```
    #[allow(dead_code)]
    /// Wait until every mock expectation is satisfied, or `timeout_secs` elapses.
    ///
    /// See `NetGetClient::wait_for_mocks` -- same reasoning, server side. Returns quietly
    /// on timeout; `verify_mocks` remains the thing that asserts.
    #[allow(dead_code)]
    pub async fn wait_for_mocks(&self, timeout_secs: u64) {
        if let Some(ref server) = self.mock_ollama_server {
            server.wait_for_expectations(timeout_secs).await;
            return;
        }
        if let Some(ref mock_config) = self.mock_config {
            super::mock_config::wait_for_mock_expectations(mock_config, timeout_secs).await;
        }
    }

    pub async fn verify_mocks(&self) -> E2EResult<()> {
        // Prefer mock Ollama server if available (new approach)
        if let Some(ref server) = self.mock_ollama_server {
            return server.verify_calls().await;
        }

        // Fallback to old method (DEPRECATED - for backward compat)
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
            eprintln!();

            return Err(format!("Mock verification failed:\n{}", errors.join("\n")).into());
        }

        println!("✅ All mock expectations verified successfully");
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

impl Drop for NetGetServer {
    fn drop(&mut self) {
        // Abort background reader tasks to prevent hanging
        self.stdout_reader_handle.abort();
        self.stderr_reader_handle.abort();

        if let Some(ref mock_config) = self.mock_config {
            if !mock_config.is_verified() {
                super::mock_config::report_unverified_on_drop("server", mock_config);
            }
        }
    }
}

/// Configuration for a server test (re-export for backward compatibility)
pub use super::netget::NetGetConfig as ServerConfig;

/// Start a NetGet server with the given configuration (backward compatible wrapper)
/// Asserts exactly 1 server and 0 clients were started
pub async fn start_netget_server(config: ServerConfig) -> E2EResult<NetGetServer> {
    let instance = super::netget::start_netget(config).await?;

    // Validate expectations
    if instance.servers.len() != 1 {
        return Err(format!(
            "Expected exactly 1 server, got {}. Use start_netget() for multiple servers.",
            instance.servers.len()
        )
        .into());
    }

    if !instance.clients.is_empty() {
        return Err(format!(
            "Expected 0 clients, got {}. Prompt started unexpected clients.",
            instance.clients.len()
        )
        .into());
    }

    // Use ManuallyDrop to prevent Drop from running when we move fields out
    let mut instance = std::mem::ManuallyDrop::new(instance);
    let server = instance.servers.drain(..).next().unwrap();

    // SAFETY: We're manually managing the lifecycle. The fields are moved to NetGetServer
    // which has its own Drop implementation that will clean them up.
    Ok(NetGetServer::new(
        unsafe { std::ptr::read(&instance.child) },
        server.port,
        server.stack,
        instance.output_lines.clone(),
        unsafe { std::ptr::read(&instance.mock_ollama_server) },
        unsafe { std::ptr::read(&instance.mock_config) },
        unsafe { std::ptr::read(&instance.mock_temp_file) },
        unsafe { std::ptr::read(&instance.stdout_reader_handle) },
        unsafe { std::ptr::read(&instance.stderr_reader_handle) },
    ))
}

/// Wait for server to be ready and responsive
///
/// This function waits for the server to have specific output indicating it's ready.
/// The server is considered ready when output contains the protocol name.
///
/// # Arguments
/// * `server` - The NetGetServer instance to check
/// * `timeout_duration` - Maximum time to wait for server readiness
/// * `protocol_name` - Expected protocol name to find in output (e.g., "IMAP", "HTTP")
///
/// # Returns
/// * `Ok(())` if server is ready within timeout
/// * `Err(_)` if timeout expires before server is ready
///
/// # Example
/// ```rust,ignore
/// let server = start_netget_server(config).await?;
/// wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;
/// ```
#[allow(dead_code)]
pub async fn wait_for_server_startup(
    server: &NetGetServer,
    timeout_duration: Duration,
    protocol_name: &str,
) -> E2EResult<()> {
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        // Check if server output contains the protocol name
        if server.output_contains(protocol_name).await {
            println!(
                "  [WAIT] Server ready with {} protocol after {:?}",
                protocol_name,
                start.elapsed()
            );
            return Ok(());
        }

        // Check for common ready indicators
        if server.output_contains("Server is running").await
            || server.output_contains("listening on").await
            || server.output_contains("advertising").await
        {
            println!("  [WAIT] Server ready after {:?}", start.elapsed());
            return Ok(());
        }

        // Small delay before checking again
        sleep(Duration::from_millis(100)).await;
    }

    // Timeout - show output for debugging
    let output = server.get_output().await;
    eprintln!("  [ERROR] Server startup timeout. Last 20 lines of output:");
    for line in output.iter().rev().take(20).rev() {
        eprintln!("    {}", line);
    }

    Err(format!(
        "Server did not become ready within {:?}. Expected to see '{}' in output.",
        timeout_duration, protocol_name
    )
    .into())
}

/// Assert that the server is using the expected protocol stack
///
/// # Arguments
/// * `server` - The NetGetServer instance to check
/// * `expected_stack` - Expected stack name (e.g., "TCP", "HTTP", "IMAP")
///
/// # Panics
/// * If the server is not using the expected stack
///
/// # Example
/// ```rust,ignore
/// let server = start_netget_server(config).await?;
/// assert_stack_name(&server, "HTTP");
/// ```
#[allow(dead_code)]
pub fn assert_stack_name(server: &NetGetServer, expected_stack: &str) {
    assert_eq!(
        server.stack, expected_stack,
        "Expected stack '{}' but got '{}'",
        expected_stack, server.stack
    );
}

/// Get all captured output lines from the server
///
/// This is a convenience function that returns owned Vec<String> instead of
/// requiring async/await for simple access patterns.
///
/// # Arguments
/// * `server` - The NetGetServer instance
///
/// # Returns
/// * Vector of output lines captured since server start
///
/// # Example
/// ```rust,ignore
/// let server = start_netget_server(config).await?;
/// tokio::time::sleep(Duration::from_secs(2)).await;
/// let output = get_server_output(&server).await;
/// assert!(output.iter().any(|line| line.contains("ready")));
/// ```
#[allow(dead_code)]
pub async fn get_server_output(server: &NetGetServer) -> Vec<String> {
    server.get_output().await
}

/// Extract base_stack value from open_server prompt
#[allow(dead_code)]
pub fn extract_base_stack_from_prompt(prompt: &str) -> Option<String> {
    // Look for "base_stack <value>" pattern in the prompt
    let prompt_lower = prompt.to_lowercase();

    if let Some(stack_pos) = prompt_lower.find("base_stack ") {
        let after_stack = &prompt[stack_pos + "base_stack ".len()..];
        // Extract until we hit a period, newline, or other terminator
        let stack_value: String = after_stack
            .chars()
            .take_while(|c| !matches!(c, '.' | '\n' | ',' | ';'))
            .collect();

        let trimmed = stack_value.trim();
        if !trimmed.is_empty() {
            // Use the protocol registry to parse the stack name
            return netget::protocol::server_registry::registry().parse_from_str(trimmed);
        }
    }

    None
}

/// Extract port number from prompt
#[allow(dead_code)]
pub fn extract_port_from_prompt(prompt: &str) -> u16 {
    let prompt_lower = prompt.to_lowercase();

    // Look for "port <number>" pattern
    if let Some(port_pos) = prompt_lower.find("port ") {
        let after_port = &prompt_lower[port_pos + 5..];
        if let Some(end_pos) = after_port.find(|c: char| !c.is_ascii_digit()) {
            if let Ok(port) = after_port[..end_pos].parse::<u16>() {
                return port;
            }
        } else if let Ok(port) = after_port.parse::<u16>() {
            return port;
        }
    }

    0 // Default to 0 for dynamic allocation
}
