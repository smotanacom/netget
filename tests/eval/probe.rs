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
    if bin.contains('/') {
        return std::path::Path::new(bin).is_file();
    }
    which_path(bin).is_some()
}

fn which_path(bin: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = std::path::Path::new(dir).join(bin);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn substitute(template: &str, port: u16) -> String {
    template
        .replace("{PORT}", &port.to_string())
        .replace("{HOST}", "127.0.0.1")
        .replace("{ADDR}", &format!("127.0.0.1:{}", port))
}

/// Read whatever an exited client left in a pipe, bounded so a pipe some
/// grandchild still holds open cannot stall the run.
async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: &mut R, buf: &mut Vec<u8>) {
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), pipe.read_to_end(&mut rest)).await;
    buf.extend_from_slice(&rest);
}

/// Run the client against `127.0.0.1:port` and capture everything it said.
///
/// A timeout is not an error here — it is an observation ("the client waited and
/// got nothing"), which is exactly what a model that never answered looks like
/// from the peer's side. The classifier decides what it means.
pub async fn run(probe: &Probe, port: u16, timeout: Duration) -> Result<ProbeOutcome, String> {
    let args: Vec<String> = probe.args.iter().map(|a| substitute(a, port)).collect();
    let command_line = format!("{} {}", probe.bin, args.join(" "));

    let mut cmd = Command::new(probe.bin);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in &probe.env {
        cmd.env(key, substitute(value, port));
    }

    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not spawn {:?}: {}", probe.bin, e))?;

    // A probe client can block indefinitely on a server that never answers —
    // which is precisely the case this harness exists to measure — so a sweep
    // killed mid-run would otherwise leave `nc`, `psql`, `redis-cli` behind on
    // the loopback ports the next run wants. `kill_on_drop` covers the panic
    // path only; this covers a hard-killed parent. See
    // `tests/helpers/child_guard.rs`.
    let probe_pid = child.id();
    if let Some(pid) = probe_pid {
        crate::helpers::child_guard::tie_child(pid);
    }

    // Write the payload but KEEP the handle: dropping it is EOF, and for `nc`
    // EOF is a socket close. Held until the reader is done.
    let mut stdin_handle = child.stdin.take();
    if let (Some(sink), Some(data)) = (stdin_handle.as_mut(), probe.stdin.as_ref()) {
        let payload = substitute(data, port);
        // A client that has already closed its end makes this fail; that is
        // normal (the exchange is over), not a harness error.
        let _ = sink.write_all(payload.as_bytes()).await;
        let _ = sink.flush().await;
    }
    if !probe.hold_stdin {
        // Clients that manage their own connection (curl, dig, psql…) want EOF
        // so they stop reading their script and get on with it.
        drop(stdin_handle.take());
    }

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    // Time of the most recent byte, not of the first: a client that is still
    // streaming must not be cut off two seconds into a long answer.
    let mut last_data_at: Option<Instant> = None;
    let deadline = started + timeout;
    let mut timed_out = false;
    let mut exited = None;

    loop {
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        // Settle: something arrived and then went quiet, so the response is
        // complete as far as any of these clients will tell us. Not for a
        // client that talks before the exchange — see `Probe::until_exit`.
        if !probe.until_exit && last_data_at.is_some_and(|t| t.elapsed() >= IDLE_AFTER_FIRST_BYTE) {
            break;
        }

        let mut out_chunk = [0u8; 8192];
        let mut err_chunk = [0u8; 8192];
        let step = Duration::from_millis(250);

        let mut progressed = false;
        if let Some(pipe) = stdout.as_mut() {
            if let Ok(Ok(n)) = tokio::time::timeout(step, pipe.read(&mut out_chunk)).await {
                if n > 0 {
                    out_buf.extend_from_slice(&out_chunk[..n]);
                    last_data_at = Some(Instant::now());
                    progressed = true;
                } else {
                    stdout = None;
                }
            }
        }
        if let Some(pipe) = stderr.as_mut() {
            if let Ok(Ok(n)) = tokio::time::timeout(step, pipe.read(&mut err_chunk)).await {
                if n > 0 {
                    err_buf.extend_from_slice(&err_chunk[..n]);
                    last_data_at = Some(Instant::now());
                    progressed = true;
                } else {
                    stderr = None;
                }
            }
        }

        // Both pipes at EOF: the client is finished whatever its exit status.
        if stdout.is_none() && stderr.is_none() {
            exited = child.try_wait().ok().flatten().and_then(|s| s.code());
            break;
        }
        if !progressed {
            if let Ok(Some(status)) = child.try_wait() {
                exited = status.code();
                break;
            }
        }
    }

    // A client that exited may still have output sitting in its pipes: the
    // loop notices the exit between two 250ms reads, and whatever the client
    // wrote last — for `ipptool`, the entire response — arrived after the read
    // that timed out. Without this drain a response that reached the client
    // was recorded as a client that printed nothing after the request echo.
    if exited.is_some() {
        if let Some(p) = stdout.as_mut() {
            drain(p, &mut out_buf).await;
        }
        if let Some(p) = stderr.as_mut() {
            drain(p, &mut err_buf).await;
        }
    }

    // Release stdin, then make sure the process is gone. `kill_on_drop` covers
    // the panic path; this covers the normal one and lets us read the status.
    drop(stdin_handle);
    if exited.is_none() {
        match child.try_wait() {
            Ok(Some(status)) => exited = status.code(),
            _ => {
                let _ = child.start_kill();
                if let Ok(Ok(status)) =
                    tokio::time::timeout(Duration::from_secs(5), child.wait()).await
                {
                    exited = status.code();
                }
            }
        }
    }
    if let Some(pid) = probe_pid {
        crate::helpers::child_guard::untie_child(pid);
    }

    let stderr_text = if timed_out && out_buf.is_empty() && err_buf.is_empty() {
        format!("(client killed after {:?} with no answer)", timeout)
    } else {
        String::from_utf8_lossy(&err_buf).to_string()
    };

    Ok(ProbeOutcome {
        stdout: String::from_utf8_lossy(&out_buf).to_string(),
        stderr: stderr_text,
        exit_code: exited,
        timed_out,
        elapsed: started.elapsed(),
        command: command_line,
    })
}
