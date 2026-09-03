//! NetGet wrapper for E2E testing
//!
//! This module provides an extended wrapper around NetGet for testing,
//! with methods to control the process and extract information.

use anyhow::{Context, Result};
use regex::Regex;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Information about a created server
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub id: u32,
    pub protocol: String,
    pub port: u16,
}

/// Extended NetGet wrapper for E2E testing
pub struct NetGetWrapper {
    process: Option<Child>,
    stdin: Option<tokio::process::ChildStdin>,
    output_buffer: Arc<Mutex<String>>,
    binary_path: PathBuf,
    stdout_reader_handle: Option<tokio::task::JoinHandle<()>>,
    stderr_reader_handle: Option<tokio::task::JoinHandle<()>>,
}

impl NetGetWrapper {
    /// Create a new NetGet wrapper
    pub fn new() -> Self {
        // Use cargo's env variable to get the actual binary path
        let binary_path = PathBuf::from(env!("CARGO_BIN_EXE_netget"));
        Self::with_binary(binary_path)
    }

    /// Create with specific binary path
    pub fn with_binary(binary_path: PathBuf) -> Self {
        Self {
            process: None,
            stdin: None,
            output_buffer: Arc::new(Mutex::new(String::new())),
            binary_path,
            stdout_reader_handle: None,
            stderr_reader_handle: None,
        }
    }

    /// Start NetGet with specified model and features
    pub async fn start(&mut self, model: &str, features: Vec<&str>) -> Result<()> {
        if self.process.is_some() {
            anyhow::bail!("NetGet already started");
        }

        // Build command
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("--model").arg(model);

        // Add feature flags
        for feature in features {
            cmd.arg(feature);
        }

        // Add test-specific flags

        // Setup pipes
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Start process
        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to start NetGet binary at {:?}", self.binary_path))?;

        // Take stdin
        self.stdin = child.stdin.take();

        // Start output reader tasks
        if let Some(stdout) = child.stdout.take() {
            let buffer = self.output_buffer.clone();
            self.stdout_reader_handle = Some(tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = buffer.lock().await;
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }));
        }

        if let Some(stderr) = child.stderr.take() {
            let buffer = self.output_buffer.clone();
            self.stderr_reader_handle = Some(tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = buffer.lock().await;
                    buf.push_str("[STDERR] ");
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }));
        }

        self.process = Some(child);

        // Wait for startup
        tokio::time::sleep(Duration::from_secs(2)).await;

        Ok(())
    }

    /// Send user input to NetGet
    pub async fn send_user_input(&mut self, input: &str) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .context("NetGet not started or stdin not available")?;

        stdin
            .write_all(input.as_bytes())
            .await
            .context("Failed to write to stdin")?;

        if !input.ends_with('\n') {
            stdin
                .write_all(b"\n")
                .await
                .context("Failed to write newline")?;
        }

        stdin.flush().await.context("Failed to flush stdin")?;

        Ok(())
    }

    /// Create a server with the given prompt and wait for it to start
    pub async fn create_server(&mut self, prompt: &str) -> Result<ServerInfo> {
        // Clear output buffer
        {
            let mut buf = self.output_buffer.lock().await;
            buf.clear();
        }

        // Send prompt
        self.send_user_input(prompt).await?;

        // Wait for server creation message
        let start_time = std::time::Instant::now();
        let timeout = Duration::from_secs(30);

        while start_time.elapsed() < timeout {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let output = self.get_output().await;

            // Look for server creation pattern
            // Example: "Server #1: HTTP server started on port 8080"
            let re = Regex::new(r"Server #(\d+):\s*(\w+).*port\s+(\d+)")?;
            if let Some(caps) = re.captures(&output) {
                let id = caps[1].parse()?;
                let protocol = caps[2].to_string();
                let port = caps[3].parse()?;

                return Ok(ServerInfo { id, protocol, port });
            }

            // Also check for simpler pattern
            // Example: "Started HTTP on 0.0.0.0:8080"
            let re2 = Regex::new(r"Started\s+(\w+)\s+on\s+.*:(\d+)")?;
            if let Some(caps) = re2.captures(&output) {
                let protocol = caps[1].to_string();
                let port = caps[2].parse()?;

                return Ok(ServerInfo {
                    id: 1, // Assume first server
                    protocol,
                    port,
                });
            }
        }

        anyhow::bail!("Server creation timed out or not detected in output")
    }

    /// Get the current output buffer
    pub async fn get_output(&self) -> String {
        self.output_buffer.lock().await.clone()
    }

    /// Clear the output buffer
    pub async fn clear_output(&self) {
        self.output_buffer.lock().await.clear();
    }

    /// Wait for specific text to appear in output
    pub async fn wait_for_output(&self, text: &str, timeout: Duration) -> Result<()> {
        let start_time = std::time::Instant::now();

        while start_time.elapsed() < timeout {
            let output = self.get_output().await;
            if output.contains(text) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        anyhow::bail!("Timeout waiting for output: {}", text)
    }

    /// Check if NetGet process is still running
    pub fn is_running(&mut self) -> bool {
        if let Some(ref mut process) = self.process {
            matches!(process.try_wait(), Ok(None))
        } else {
            false
        }
    }

    /// Stop NetGet gracefully
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(mut process) = self.process.take() {
            // Try graceful shutdown first
            if let Some(mut stdin) = self.stdin.take() {
                let _ = stdin.write_all(b"exit\n").await;
                let _ = stdin.flush().await;
            }

            // Wait briefly for graceful shutdown
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Force kill if still running
            match process.try_wait() {
                Ok(None) => {
                    process
                        .kill()
                        .await
                        .context("Failed to kill NetGet process")?;
                }
                _ => {} // Already stopped
            }

            process
                .wait()
                .await
                .context("Failed to wait for process exit")?;
        }

        // Abort background reader tasks
        if let Some(handle) = self.stdout_reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_reader_handle.take() {
            handle.abort();
        }

        Ok(())
    }

    /// Get server port by ID from output
    pub async fn get_server_port(&self, server_id: u32) -> Option<u16> {
        let output = self.get_output().await;
        let pattern = format!(r"Server #{}:.*port\s+(\d+)", server_id);
        let re = Regex::new(&pattern).ok()?;

        re.captures(&output).and_then(|caps| caps[1].parse().ok())
    }

    /// Send a command and wait for completion
    pub async fn execute_command(&mut self, command: &str, wait_time: Duration) -> Result<String> {
        self.clear_output().await;
        self.send_user_input(command).await?;
        tokio::time::sleep(wait_time).await;
        Ok(self.get_output().await)
    }
}

impl Drop for NetGetWrapper {
    fn drop(&mut self) {
        // Abort background reader tasks to prevent hanging
        if let Some(handle) = self.stdout_reader_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_reader_handle.take() {
            handle.abort();
        }

        // Try to clean up the process
        if let Some(mut process) = self.process.take() {
            let _ = process.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_info() {
        let info = ServerInfo {
            id: 1,
            protocol: "HTTP".to_string(),
            port: 8080,
        };
        assert_eq!(info.id, 1);
        assert_eq!(info.port, 8080);
    }
}
