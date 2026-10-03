//! Running a real third-party client against a live netget server.
//!
//! Everything here is a separate process speaking the protocol for itself. That
//! is the whole point: a crate NetGet also links is circular evidence, and a
//! client hand-written inside the test is an independent reading of the spec
//! rather than an independent implementation. `dig`, `curl`, `redis-cli`,
//! `psql`, `ldapsearch`, `ipptool`, `whois` and `ftp` are none of those things.
//!
//! # Why this streams instead of calling `wait_with_output`
//!
//! `nc` closes the socket when its stdin reaches EOF, and the obvious
//! implementation — write the payload, drop the handle, wait for the process —
//! hands it EOF immediately. Measured: netget logged `TCP received 11 bytes`
//! and `Connection closed` 300µs apart, then `dropping 11 received bytes`,
//! because the peer had hung up long before the model was asked. Every raw-TCP
//! and UDP case failed as `event_never_reached_model` and the harness was
//! measuring its own probe.
//!
//! So the loop below keeps stdin open, reads both output streams as they
//! arrive, and stops on whichever comes first: the client exiting, or a settle
//! period with no new bytes *after* the first byte. That is the same
//! first-byte-then-idle shape `helpers::llm_live::read_until_idle` uses, moved
//! out to a subprocess.

#![allow(dead_code)]

use super::case::Probe;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// How long the reader waits after the first byte before calling the response
/// complete. These clients do not frame their output for us.
const IDLE_AFTER_FIRST_BYTE: Duration = Duration::from_secs(2);

/// What the client did.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Capture exceeded its explicit limit; this is a harness failure, never a model score.
    pub output_truncated: bool,
    pub elapsed: Duration,
    /// The command line actually run, for the results file.
    pub command: String,
}

impl ProbeOutcome {
    /// Everything the client printed. Checks run against this, because clients
    /// differ wildly about which stream carries the answer (`dig` uses stdout,
    /// `ldapsearch` splits, `ftp` narrates on stderr).
    pub fn combined(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }

    pub fn is_silent(&self) -> bool {
        self.stdout.trim().is_empty() && self.stderr.trim().is_empty()
    }
}

/// Is this client installed?
///
/// An absolute path is checked directly. Some cases name one deliberately —
/// MySQL must use the 8.0 client, because the 9.x one cannot load the
/// `mysql_native_password` plugin NetGet's server offers and dies before a query
/// exists — and a PATH-only search would report those as missing.
pub fn binary_available(bin: &str) -> bool {
    let path = std::path::Path::new(bin);
    if path.components().count() > 1 || path.is_absolute() {
        return executable(path);
    }
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            let candidate = directory.join(bin);
            if executable(&candidate) {
                return true;
            }
            #[cfg(windows)]
            {
                if candidate.extension().is_none() {
                    return std::env::var_os("PATHEXT")
                        .unwrap_or_else(|| ".EXE;.COM;.BAT;.CMD".into())
                        .to_string_lossy()
                        .split(';')
                        .any(|ext| {
                            executable(&candidate.with_extension(ext.trim_start_matches('.')))
                        });
                }
            }
            false
        })
    })
}

fn executable(path: &std::path::Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn substitute(template: &str, port: u16) -> String {
    template
        .replace("{PORT}", &port.to_string())
        .replace("{HOST}", "127.0.0.1")
        .replace("{ADDR}", &format!("127.0.0.1:{}", port))
}

/// Maximum retained bytes per output stream. Overflow is explicit in the
/// outcome and cannot be counted as a passing or failing model response.
pub const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

pub async fn run(probe: &Probe, port: u16, timeout: Duration) -> Result<ProbeOutcome, String> {
    use netget::scripting::process_io::ProcessGroup;
    let args: Vec<String> = probe.args.iter().map(|a| substitute(a, port)).collect();
    let command_line = format!("{} {}", probe.bin, args.join(" "));
    let mut cmd = Command::new(probe.bin);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    ProcessGroup::configure(&mut cmd);
    for (key, value) in &probe.env {
        cmd.env(key, substitute(value, port));
    }
    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not spawn {:?}: {e}", probe.bin))?;
    let group = ProcessGroup::new(&child).map_err(|e| format!("own probe process group: {e}"))?;
    struct Untie(Option<u32>);
    impl Drop for Untie {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                crate::helpers::child_guard::untie_child(pid);
            }
        }
    }
    let probe_pid = child.id();
    let _untie = Untie(probe_pid);
    if let Some(pid) = probe_pid {
        crate::helpers::child_guard::tie_child(pid);
    }
    let mut stdin = child.stdin.take();
    let payload = probe.stdin.as_ref().map(|data| substitute(data, port));
    let mut input = Box::pin(async move {
        if let (Some(sink), Some(data)) = (stdin.as_mut(), payload.as_ref()) {
            // Broken pipe is a normal early peer exit. Crucially, writing is
            // polled alongside both readers and the same overall deadline.
            let _ = sink.write_all(data.as_bytes()).await;
            let _ = sink.flush().await;
        }
        if probe.hold_stdin {
            stdin
        } else {
            None
        }
    });
    let mut held_stdin = None;
    let mut input_done = false;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let mut last_data_at: Option<Instant> = None;
    let mut exited = None;
    let mut timed_out = false;
    let mut output_truncated = false;
    let mut out_chunk = [0u8; 8192];
    let mut err_chunk = [0u8; 8192];
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        if !probe.until_exit
            && input_done
            && last_data_at.is_some_and(|t| t.elapsed() >= IDLE_AFTER_FIRST_BYTE)
        {
            break;
        }
        if stdout.is_none() && stderr.is_none() && exited.is_some() {
            break;
        }
        tokio::select! {
            biased;
            _ = tokio::time::sleep(remaining) => { timed_out = true; break; }
            result = child.wait(), if exited.is_none() => {
                exited = Some(result.map_err(|e| format!("wait for probe: {e}"))?);
                // Kill descendants that inherited pipes; buffered output is still
                // drained concurrently below, under the original deadline.
                group.kill();
            }
            handle = &mut input, if !input_done => { held_stdin = handle; input_done = true; }
            result = async { stdout.as_mut().unwrap().read(&mut out_chunk).await }, if stdout.is_some() => {
                let n = result.map_err(|e| format!("read probe stdout: {e}"))?;
                if n == 0 { stdout = None; }
                else {
                    let retain = n.min(MAX_CAPTURE_BYTES.saturating_sub(out_buf.len()));
                    out_buf.extend_from_slice(&out_chunk[..retain]);
                    last_data_at = Some(Instant::now());
                    if retain < n { output_truncated = true; break; }
                }
            }
            result = async { stderr.as_mut().unwrap().read(&mut err_chunk).await }, if stderr.is_some() => {
                let n = result.map_err(|e| format!("read probe stderr: {e}"))?;
                if n == 0 { stderr = None; }
                else {
                    let retain = n.min(MAX_CAPTURE_BYTES.saturating_sub(err_buf.len()));
                    err_buf.extend_from_slice(&err_chunk[..retain]);
                    last_data_at = Some(Instant::now());
                    if retain < n { output_truncated = true; break; }
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
    drop(input);
    drop(held_stdin);
    group.kill();
    if exited.is_none() {
        let _ = child.start_kill();
        if let Ok(Ok(status)) = tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
            exited = Some(status);
        }
    }
    let mut stderr_text = String::from_utf8_lossy(&err_buf).into_owned();
    if output_truncated {
        stderr_text
            .push_str("\n[HARNESS: probe output exceeded the 1 MiB per-stream capture limit]");
    } else if timed_out && out_buf.is_empty() && err_buf.is_empty() {
        stderr_text = format!("(client killed after {timeout:?} with no answer)");
    }
    Ok(ProbeOutcome {
        stdout: String::from_utf8_lossy(&out_buf).into_owned(),
        stderr: stderr_text,
        exit_code: exited.and_then(|s| s.code()),
        timed_out,
        output_truncated,
        elapsed: started.elapsed(),
        command: command_line,
    })
}
