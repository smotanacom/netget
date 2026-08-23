//! Named pipe (POSIX FIFO) server implementation.
//!
//! Platform: Unix/Linux/macOS only. The server creates a FIFO with `mkfifo`, reads whatever a
//! writer process puts on it, hands each chunk to the LLM as a `named_pipe_data_received` event,
//! and writes the model's reply to an optional second FIFO. NetGet owns the plumbing; the model
//! owns the payload — the same contract as `socket_file`, addressed by a FIFO path.
#![cfg(unix)]

pub mod actions;

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::error;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::named_pipe::actions::{NamedPipeProtocol, NAMED_PIPE_DATA_RECEIVED_EVENT};
use crate::state::app_state::AppState;
use crate::utils::WireFailure;

/// Removes the FIFO node(s) this server created when the read task ends — including when the task
/// is aborted by `stop_server`, because aborting drops the task future and with it this guard.
struct FifoCleanup(Vec<PathBuf>);

impl Drop for FifoCleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Describe a file type for the "refusing to reuse" error message.
fn describe_file_type(ft: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "symlink"
    } else if ft.is_fifo() {
        "FIFO"
    } else if ft.is_socket() {
        "socket"
    } else if ft.is_char_device() {
        "character device"
    } else if ft.is_block_device() {
        "block device"
    } else {
        "regular file"
    }
}

/// Ensure a FIFO exists at `path`, creating it with `mkfifo` if absent.
///
/// `path` comes from the LLM or an MCP caller, so — exactly like `socket_file` — an existing node
/// is only reused when it really is a FIFO. Anything else (a regular file, a symlink, a socket) is
/// refused with a message naming what was found, rather than being clobbered.
fn ensure_fifo(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            use std::os::unix::fs::FileTypeExt;
            if meta.file_type().is_fifo() {
                // Reuse the existing FIFO.
                return Ok(());
            }
            anyhow::bail!(
                "Refusing to use {:?}: it exists but is not a FIFO (it is a {}). Delete it \
                 yourself if that is really what you want, or pass a different path.",
                path,
                describe_file_type(&meta.file_type())
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| anyhow::anyhow!("invalid FIFO path: {:?}", path))?;
            // 0o600: owner read/write only.
            let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("Failed to create FIFO {:?}", path));
            }
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to stat {:?}", path)),
    }
}

/// Open `path` O_RDWR | O_NONBLOCK.
///
/// O_RDWR on a FIFO succeeds immediately (no blocking wait for a peer) and, by keeping both a
/// reader and a writer reference open, prevents `read()` from returning a spurious EOF every time
/// an external writer closes — which would otherwise busy-loop the AsyncFd read loop. O_NONBLOCK
/// is required so the fd can be driven by tokio's `AsyncFd`.
fn open_fifo_rdwr_nonblocking(path: &Path) -> Result<File> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid FIFO path: {:?}", path))?;
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to open FIFO {:?}", path));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Open `path` O_RDWR (blocking) for writing responses.
///
/// O_RDWR succeeds immediately even before a reader attaches, and lets us buffer a response into
/// the kernel FIFO buffer that the reader drains when it opens the path.
fn open_fifo_rdwr(path: &Path) -> Result<File> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid FIFO path: {:?}", path))?;
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to open response FIFO {:?}", path));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Named pipe server.
pub struct NamedPipeServer;

impl NamedPipeServer {
    /// Create the FIFO(s), open the read side, and spawn the read/dispatch loop.
    ///
    /// Awaits readiness (both FIFOs created and opened) before returning, so a failure surfaces as
    /// `Err` and `server_startup` records `ServerStatus::Error` rather than a server that lies
    /// about being up.
    pub async fn spawn_with_llm_actions(
        pipe_path: PathBuf,
        response_pipe_path: Option<PathBuf>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<()> {
        ensure_fifo(&pipe_path)?;
        let read_file = open_fifo_rdwr_nonblocking(&pipe_path)?;
        let async_read = AsyncFd::new(read_file).with_context(|| {
            format!(
                "Failed to register FIFO {:?} with the async runtime",
                pipe_path
            )
        })?;

        // Track every node we created so the cleanup guard removes them on stop.
        let mut created: Vec<PathBuf> = vec![pipe_path.clone()];

        let response_file = match &response_pipe_path {
            Some(rp) => {
                ensure_fifo(rp)?;
                created.push(rp.clone());
                Some(open_fifo_rdwr(rp)?)
            }
            None => None,
        };
        let cleanup = FifoCleanup(created);

        Log::new(Some(&status_tx)).info(format!(
            "Named pipe server reading FIFO {:?} (response FIFO: {:?})",
            pipe_path, response_pipe_path
        ));

        let protocol = Arc::new(NamedPipeProtocol::new());
        let task_registrar = app_state.clone();

        let handle = tokio::spawn(async move {
            // Move the cleanup guard into the task so it drops (and unlinks) on abort.
            let _cleanup = cleanup;
            let mut response_file = response_file;
            let mut buffer = vec![0u8; 8192];

            loop {
                let mut guard = match async_read.readable().await {
                    Ok(g) => g,
                    Err(e) => {
                        error!("Named pipe {:?} readable() error: {}", pipe_path, e);
                        break;
                    }
                };

                let read_result = guard.try_io(|inner| {
                    let fd = inner.get_ref().as_raw_fd();
                    let n = unsafe {
                        libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len())
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                });

                let n = match read_result {
                    Ok(Ok(0)) => {
                        // With O_RDWR a real EOF should not occur; guard against a busy loop.
                        continue;
                    }
                    Ok(Ok(n)) => n,
                    Ok(Err(e)) => {
                        error!("Named pipe {:?} read error: {}", pipe_path, e);
                        break;
                    }
                    Err(_would_block) => continue,
                };

                let data = &buffer[..n];
                let (data_str, encoding) = if data
                    .iter()
                    .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
                {
                    (String::from_utf8_lossy(data).to_string(), "utf8")
                } else {
                    (hex::encode(data), "hex")
                };

                Log::new(Some(&status_tx)).debug(format!(
                    "Named pipe {:?} received {} bytes ({})",
                    pipe_path, n, encoding
                ));

                let event = Event::new(
                    &NAMED_PIPE_DATA_RECEIVED_EVENT,
                    serde_json::json!({ "data": data_str, "encoding": encoding }),
                );

                match call_llm(
                    &llm_client,
                    &app_state,
                    server_id,
                    None,
                    &event,
                    protocol.as_ref(),
                )
                .await
                {
                    Ok(result) => {
                        for msg in result.messages {
                            let _ = status_tx.send(msg);
                        }
                        let mut wrote = 0usize;
                        for pr in result.protocol_results {
                            if let ActionResult::Output(bytes) = pr {
                                wrote += bytes.len();
                                Self::write_response(&mut response_file, &bytes, &status_tx);
                            }
                            // CloseConnection / WaitForMore / others are meaningless for a
                            // connectionless FIFO sink and are intentionally ignored.
                        }
                        // Three outcomes must stay apart in the log: the model answered with
                        // bytes, the model deliberately answered with nothing (a real answer for
                        // a one-way sink), and the backend erred (below). Only the last one gets
                        // a netget-authored reply.
                        Log::new(Some(&status_tx)).debug(format!(
                            "Named pipe {:?} decision={} bytes={}",
                            pipe_path,
                            if wrote > 0 {
                                "model_output"
                            } else {
                                "model_no_output"
                            },
                            wrote
                        ));
                    }
                    Err(e) => {
                        // The backend failed. The full error goes to the log and the status
                        // stream; the FIFO reader gets a category only — never the error, the
                        // backend URL, the model name or an anyhow chain (see
                        // `crate::utils::wire_failure`). Non-fatal: the read loop continues to
                        // the next chunk.
                        let failure = WireFailure::classify(&e);
                        let decision = if failure.is_overloaded() {
                            "fail_closed_overloaded"
                        } else {
                            "fail_closed_llm_error"
                        };
                        error!(
                            "Named pipe {:?} decision={} LLM error: {}",
                            pipe_path, decision, e
                        );
                        Log::new(Some(&status_tx)).warn(format!(
                            "Named pipe {:?} decision={}: {}",
                            pipe_path, decision, e
                        ));
                        Self::write_failure_notice(&mut response_file, failure, &status_tx);
                    }
                }
            }
        });

        task_registrar.register_server_task(server_id, handle).await;
        Ok(())
    }

    /// Tell the FIFO reader that netget itself could not answer this chunk.
    ///
    /// A reader parked in `read()` on the response FIFO has no timeout of its own — `cat` blocks
    /// forever — so silence on a backend failure hangs it indefinitely. It gets one attributed,
    /// newline-terminated line carrying a *category* and nothing else: `prefixed_text()` is
    /// `&'static str`, so nothing derived from the error can reach the pipe. The two categories
    /// have distinct wording (`WireFailure::Overloaded` reads as retryable), which is the only
    /// distinction a byte-stream FIFO can carry — it has no status codes.
    ///
    /// When no `response_pipe_path` was configured there is no reply channel at all, and silence
    /// is the only correct behaviour; it is logged, not invented.
    fn write_failure_notice(
        response_file: &mut Option<File>,
        failure: WireFailure,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let log = Log::new(Some(status_tx));
        let Some(f) = response_file else {
            log.debug(
                "Named pipe: no response_pipe_path configured; the failure notice has nowhere to \
                 go and the reader sees nothing"
                    .to_string(),
            );
            return;
        };
        let mut line = String::with_capacity(failure.prefixed_text().len() + 1);
        line.push_str(failure.prefixed_text());
        line.push('\n');
        if let Err(e) = f.write_all(line.as_bytes()).and_then(|_| f.flush()) {
            log.error(format!("Named pipe failure-notice write error: {}", e));
        }
    }

    /// Write `bytes` to the response FIFO, or warn if none was configured.
    fn write_response(
        response_file: &mut Option<File>,
        bytes: &[u8],
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let log = Log::new(Some(status_tx));
        match response_file {
            Some(f) => match f.write_all(bytes).and_then(|_| f.flush()) {
                Ok(()) => {
                    // FileOnly: the write_named_pipe_data action's own log_template already
                    // reports "-> FIFO {data_len}B" to the TUI at INFO.
                    log.debug(format!(
                        "Named pipe wrote {} bytes to response FIFO",
                        bytes.len()
                    ));
                }
                Err(e) => {
                    log.error(format!("Named pipe response write error: {}", e));
                }
            },
            None => {
                log.warn(format!(
                    "Named pipe: model produced {} bytes of output but no response_pipe_path was \
                     configured; dropping it",
                    bytes.len()
                ));
            }
        }
    }
}
