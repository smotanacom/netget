//! Agent-answered LLM backend queue.
//!
//! When NetGet runs as an MCP server in `--llm-agent` mode, protocol servers do
//! not call any model. Instead every LLM request is enqueued here and answered by
//! the calling MCP agent (e.g. Claude Code) via the `get_next_llm_request` /
//! `answer_llm_request` tools. This mirrors the oneshot-rendezvous pattern used by
//! the FIDO2 `ApprovalManager` (`src/server/usb/fido2/approval.rs`) and
//! `WebApprovalRequest` (`src/state/app_state.rs`): the producer submits a request
//! plus a `oneshot::Sender` and awaits the receiver; the consumer resolves it from
//! outside.
//!
//! Two notification paths are offered and the agent picks whichever fits:
//!   - a long-poll: `get_next_llm_request { wait_seconds }` blocks on [`Notify`],
//!   - a best-effort FIFO push: on enqueue the new request id is written to the
//!     configured named pipe so an idle agent can block-read it and get woken.

use crate::utils::clock::{SystemTime, UNIX_EPOCH};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::sync::{oneshot, Notify};
use tracing::debug;

use crate::llm::ollama_client::Message;

/// A queued LLM request awaiting an answer from the calling agent.
#[derive(Clone, Debug)]
pub struct PendingLlmRequest {
    /// Monotonic id assigned on submission (arrival order).
    pub id: u64,
    /// Unix time in milliseconds when the request was enqueued.
    pub created_unix_ms: u64,
    /// Model name recorded on the request (a placeholder like "agent" in this mode).
    pub model: String,
    /// The full prompt: system + user messages the model would have received.
    pub messages: Vec<Message>,
    /// Available actions as tool schemas (OpenAI function-tool format), if any.
    pub tools: Vec<Value>,
    /// Whether a consumer has already claimed (fetched) this request.
    pub claimed: bool,
}

/// One in-flight entry: the request metadata plus the channel that unblocks the
/// waiting protocol server when the agent answers.
struct Entry {
    request: PendingLlmRequest,
    responder: oneshot::Sender<Vec<Value>>,
}

struct Inner {
    /// Ids in arrival order (pending and claimed-but-unanswered).
    order: VecDeque<u64>,
    entries: HashMap<u64, Entry>,
    next_id: u64,
}

/// A queue of pending LLM requests answered out-of-band by the MCP agent.
///
/// Every `inner` lock is taken with `unwrap_or_else(|e| e.into_inner())` rather than
/// `unwrap()`. The queue is process-wide, so a single panic under the lock would poison it
/// and make every later `submit`/`claim_next`/`answer` panic in turn — one transient fault
/// becoming a permanent outage. The guarded state is plain collections that stay coherent.
pub struct LlmRequestQueue {
    inner: Mutex<Inner>,
    notify: Notify,
    pipe_path: Option<PathBuf>,
}

impl LlmRequestQueue {
    /// Create an empty queue. `pipe_path`, if set, receives a `<id>\n` line on each
    /// enqueue (best-effort push notification).
    pub fn new(pipe_path: Option<PathBuf>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                order: VecDeque::new(),
                entries: HashMap::new(),
                next_id: 1,
            }),
            notify: Notify::new(),
            pipe_path,
        }
    }

    /// Configured FIFO path, if any.
    pub fn pipe_path(&self) -> Option<&PathBuf> {
        self.pipe_path.as_ref()
    }

    /// Enqueue a request and return its id plus a receiver the caller awaits for the
    /// agent's answer (a NetGet action JSON array).
    pub fn submit(
        &self,
        model: String,
        messages: Vec<Message>,
        tools: Vec<Value>,
    ) -> (u64, oneshot::Receiver<Vec<Value>>) {
        let (tx, rx) = oneshot::channel();
        let created_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let id = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let id = inner.next_id;
            inner.next_id += 1;
            let request = PendingLlmRequest {
                id,
                created_unix_ms,
                model,
                messages,
                tools,
                claimed: false,
            };
            inner.entries.insert(
                id,
                Entry {
                    request,
                    responder: tx,
                },
            );
            inner.order.push_back(id);
            id
        };

        self.write_pipe_notification(id);
        self.notify.notify_waiters();
        (id, rx)
    }

    /// Claim the oldest not-yet-claimed request, marking it claimed. Returns `None`
    /// if every pending request has already been claimed.
    pub fn claim_next(&self) -> Option<PendingLlmRequest> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let mut target = None;
        for &id in &inner.order {
            if inner.entries.get(&id).map_or(false, |e| !e.request.claimed) {
                target = Some(id);
                break;
            }
        }
        let id = target?;
        let entry = inner.entries.get_mut(&id)?;
        entry.request.claimed = true;
        Some(entry.request.clone())
    }

    /// Long-poll: claim the next request, or wait up to `timeout` for one to arrive.
    /// Returns `None` on timeout. If the RPC that called this is cancelled, the
    /// future is simply dropped (no side effects).
    pub async fn wait_and_claim(&self, timeout: Duration) -> Option<PendingLlmRequest> {
        if let Some(req) = self.claim_next() {
            return Some(req);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Register for notification BEFORE re-checking to avoid a lost wakeup.
            let notified = self.notify.notified();
            if let Some(req) = self.claim_next() {
                return Some(req);
            }
            tokio::select! {
                _ = notified => {
                    if let Some(req) = self.claim_next() {
                        return Some(req);
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return None;
                }
            }
        }
    }

    /// Resolve a request with the agent's answer, unblocking the waiting server.
    /// Errors if the id is unknown (already answered, expired, or never existed) or
    /// if the waiter has already gone away (timed out / connection closed).
    pub fn answer(&self, id: u64, actions: Vec<Value>) -> Result<()> {
        let entry = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let entry = inner.entries.remove(&id).ok_or_else(|| {
                anyhow!(
                    "no pending LLM request with id {} (already answered, expired, or never existed)",
                    id
                )
            })?;
            inner.order.retain(|&x| x != id);
            entry
        };

        entry.responder.send(actions).map_err(|_| {
            anyhow!(
                "LLM request {} is no longer waiting (its connection timed out or closed)",
                id
            )
        })?;
        debug!("agent-queue: request #{} answered", id);
        Ok(())
    }

    /// Drop a timed-out / abandoned request so it can no longer be answered.
    pub fn expire(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entries.remove(&id);
        inner.order.retain(|&x| x != id);
    }

    /// Snapshot of all outstanding requests (pending + claimed-unanswered).
    pub fn list(&self) -> Vec<PendingLlmRequest> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .order
            .iter()
            .filter_map(|id| inner.entries.get(id).map(|e| e.request.clone()))
            .collect()
    }

    /// Best-effort push: write `<id>\n` to the FIFO with a non-blocking open. If no
    /// reader is attached the open fails with ENXIO — that is expected and ignored;
    /// the long-poll tool remains the reliable path.
    fn write_pipe_notification(&self, id: u64) {
        let Some(path) = self.pipe_path.as_ref() else {
            return;
        };
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
            {
                Ok(mut f) => {
                    if let Err(e) = writeln!(f, "{}", id) {
                        debug!("agent-queue: FIFO write for #{} failed: {}", id, e);
                    }
                }
                Err(e) => {
                    debug!(
                        "agent-queue: FIFO not ready for #{} ({}): no reader attached",
                        id, e
                    );
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (path, id);
        }
    }
}
