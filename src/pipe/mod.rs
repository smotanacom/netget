//! Pipes: connect one instance's events to another instance's input.
//!
//! Today every server/client is an island. A *pipe* is a deterministic tap that
//! forwards one server's network events into another server's input, so an
//! operator (or the LLM, via a `create_pipe` action) can wire services together
//! without an LLM round-trip per hop.
//!
//! The wiring is data, not code:
//!
//! ```json
//! { "from": <server_id>, "on": "http_request",
//!   "to": <server_id>, "as": "send_tcp_data",
//!   "map": { "data": "{method} {path}\n" } }
//! ```
//!
//! When server `from` raises the `on` event, the `map` templates are rendered
//! against the event's structured data (`{method}`, `{path}`, `{headers.host}`,
//! …) and the resulting payload is delivered into server `to`. This reuses the
//! existing event system ([`crate::protocol::event_type::Event`], tapped once in
//! `call_llm`) rather than adding any per-protocol glue.
//!
//! ## Delivery model (first slice)
//!
//! A server has no *inbound* action executor — its input arrives on a real
//! socket — so a pipe feeds a server sink by opening a short TCP connection to
//! the target's listen address and writing the mapped payload bytes. The payload
//! comes from the mapped `data` field, honouring an optional `encoding`
//! (`"utf8"` default, or `"hex"`), exactly like `send_tcp_data`. This works for
//! any TCP-listening sink (TCP, and by raw bytes any text protocol); richer
//! per-protocol handshakes and *client* sinks (delivered through their own
//! outbound executor) are deliberately left as future work — see the module-level
//! notes in the task report.
//!
//! ## Safety
//!
//! * **Cycle refusal.** Adding a pipe whose graph edge `from -> to` would close a
//!   loop (including the self-loop `A -> A`) is refused by [`would_create_cycle`],
//!   so the obvious `A -> B -> A` flood cannot be declared.
//! * **Bounded fan-out.** Deliveries run under a process-wide semaphore
//!   ([`MAX_INFLIGHT_DELIVERIES`]). A high-rate source feeding a slow sink does
//!   not queue without bound: once the in-flight cap is reached, further
//!   deliveries are **dropped with a `warn!`**, never buffered, so memory stays
//!   bounded. Per-payload size is capped at [`MAX_PAYLOAD_BYTES`].
//! * **Lifecycle.** Pipes touching a server are torn down when that server
//!   closes, mirroring scheduled-task scoping (see
//!   `AppStateInner::teardown_server`).

use crate::protocol::event_type::Event;
use crate::state::app_state::AppState;
use crate::state::server::ServerStatus;
use crate::state::ServerId;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

/// Maximum concurrent in-flight pipe deliveries, process-wide. Deliveries beyond
/// this are dropped-with-log rather than queued, so a slow sink cannot make a
/// fast source exhaust memory.
pub const MAX_INFLIGHT_DELIVERIES: usize = 128;

/// Cap on a single rendered payload. A pipe is for log lines and small records,
/// not bulk transfer.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Limit the intermediate template representation as well as the decoded payload.
/// This allows separated hex while preventing repeated substitutions from allocating
/// arbitrarily large strings before the payload limit can run.
pub const MAX_RENDERED_BYTES: usize = 1024 * 1024;

const DELIVERY_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const DELIVERY_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Process-wide bound on concurrent deliveries. A single NetGet process owns all
/// pipes, so a global cap is exactly the right granularity.
static PIPE_INFLIGHT: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_INFLIGHT_DELIVERIES)));

/// Identifier for a pipe. Allocated from the same unified counter as servers and
/// clients so ids never collide across kinds.
pub type PipeId = u32;

/// A declared pipe: forward `from`'s `on` events into `to` as `as_action`,
/// rendering `map` templates against the event data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PipeSpec {
    /// Assigned pipe id.
    pub id: PipeId,
    /// Source server id whose events are tapped.
    pub from: u32,
    /// Event type id to tap, e.g. `"http_request"`.
    pub on: String,
    /// Target server id that receives the mapped input.
    pub to: u32,
    /// Action the mapping emulates on the target, e.g. `"send_tcp_data"`.
    /// Advisory for the socket-delivery slice: the bytes come from the mapped
    /// `data`/`encoding` fields regardless of the action name.
    #[serde(rename = "as")]
    pub as_action: String,
    /// Field -> template. Templates reference source event fields with
    /// `{field}` / `{dotted.path}`.
    pub map: BTreeMap<String, String>,
}

/// Would adding edge `from -> to` create a directed cycle among `existing`
/// pipes? Detects the self-loop `from == to` and any `to ->* from` path.
pub fn would_create_cycle(existing: &[PipeSpec], from: u32, to: u32) -> bool {
    if from == to {
        return true;
    }
    // Reachable set from `to` following existing from->to edges; a cycle forms
    // iff `from` is already reachable from `to`.
    let mut stack = vec![to];
    let mut seen = vec![to];
    while let Some(node) = stack.pop() {
        for e in existing.iter().filter(|e| e.from == node) {
            if e.to == from {
                return true;
            }
            if !seen.contains(&e.to) {
                seen.push(e.to);
                stack.push(e.to);
            }
        }
    }
    false
}

/// Render a `{field}` / `{dotted.path}` template against structured event data.
/// A missing field renders empty (and is not an error — the payload is still
/// well-formed). Literal braces are not supported in this first slice.
struct RenderBuffer(Vec<u8>);

impl Write for RenderBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RENDERED_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other(format!(
                "pipe rendered mapping exceeds the {MAX_RENDERED_BYTES}-byte cap"
            )));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn render_template(mut tmpl: &str, data: &Value) -> Result<String> {
    let mut out = RenderBuffer(Vec::new());
    while let Some(open) = tmpl.find('{') {
        out.write_all(tmpl[..open].as_bytes())?;
        let rest = &tmpl[open + 1..];
        let Some(close) = rest.find('}') else {
            // Preserve an unterminated placeholder verbatim.
            out.write_all(tmpl[open..].as_bytes())?;
            return Ok(String::from_utf8(out.0)?);
        };
        let mut value = Some(data);
        for segment in rest[..close].trim().split('.') {
            value = value.and_then(|v| v.get(segment));
        }
        match value {
            Some(Value::String(value)) => out.write_all(value.as_bytes())?,
            None | Some(Value::Null) => {}
            // Serialize directly into the bounded buffer; to_string() would allocate
            // an entire arbitrarily large object before applying the limit.
            Some(value) => serde_json::to_writer(&mut out, value)?,
        }
        tmpl = &rest[close + 1..];
    }
    out.write_all(tmpl.as_bytes())?;
    Ok(String::from_utf8(out.0)?)
}

/// Decode a rendered `data` string into wire bytes per `encoding`.
///
/// Mirrors `send_tcp_data`: `"utf8"` (default) sends the characters as-is,
/// `"hex"` decodes hex (tolerating whitespace, `:` separators and a leading
/// `0x`). Any other value is an error rather than a silent guess.
fn decode_payload(data: &str, encoding: &str) -> Result<Vec<u8>> {
    match encoding {
        "utf8" | "" => Ok(data.as_bytes().to_vec()),
        "hex" => {
            let cleaned: String = data
                .trim()
                .trim_start_matches("0x")
                .chars()
                .filter(|c| !c.is_ascii_whitespace() && *c != ':')
                .collect();
            hex::decode(&cleaned).map_err(|e| {
                anyhow!(
                    "pipe payload declared encoding \"hex\" but is not valid hex ({e}); \
                     for text, use \"utf8\" or omit encoding"
                )
            })
        }
        other => bail!(
            "invalid pipe payload encoding {other:?}; valid values are \"utf8\" (default) and \"hex\""
        ),
    }
}

/// Render a spec's mapping into the payload bytes to deliver to a socket sink.
///
/// Requires the mapping to produce a `data` field (the bytes); `encoding` is
/// optional. Kept separate from delivery so it is unit-testable without a socket.
pub fn render_payload(spec: &PipeSpec, event_data: &Value) -> Result<Vec<u8>> {
    let data_tmpl = spec.map.get("data").ok_or_else(|| {
        anyhow!(
            "pipe #{} map must produce a 'data' field to deliver to a socket sink",
            spec.id
        )
    })?;
    let data = render_template(data_tmpl, event_data)?;
    let encoding = spec
        .map
        .get("encoding")
        .map(|t| render_template(t, event_data))
        .transpose()?
        .unwrap_or_else(|| "utf8".to_string());
    let bytes = decode_payload(&data, &encoding)?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        bail!(
            "pipe #{} payload is {} bytes, over the {}-byte cap",
            spec.id,
            bytes.len(),
            MAX_PAYLOAD_BYTES
        );
    }
    Ok(bytes)
}

/// The deliverable address of a server sink: its listen address, but only while
/// it is actually running.
fn deliverable_addr(status: &ServerStatus, local_addr: Option<SocketAddr>) -> Option<SocketAddr> {
    match status {
        ServerStatus::Running => local_addr,
        _ => None,
    }
}

/// Open a short TCP connection to `addr` and write `payload`, with timeouts.
async fn deliver(addr: SocketAddr, payload: &[u8]) -> Result<()> {
    let mut stream = tokio::time::timeout(
        DELIVERY_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_| anyhow!("connect to {addr} timed out"))??;

    tokio::time::timeout(DELIVERY_WRITE_TIMEOUT, stream.write_all(payload))
        .await
        .map_err(|_| anyhow!("write to {addr} timed out"))??;
    // Flush and shut down the write side so the sink sees a clean message.
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
    Ok(())
}

/// Tap point: called once per event in `call_llm`, before any handler runs, so a
/// pipe fires regardless of how the source server answers its own peer (and even
/// if the source's LLM call later fails).
///
/// This does no network I/O inline — it resolves the target address and renders
/// the payload cheaply, then hands the actual connect+write to a bounded
/// background task. It never blocks the source's response path and never holds a
/// lock across an `.await` doing I/O.
pub async fn dispatch_pipes(state: &AppState, from: ServerId, event: &Event) {
    let specs = state.pipes_matching(from, event.id()).await;
    if specs.is_empty() {
        return;
    }

    for spec in specs {
        let payload = match render_payload(&spec, &event.data) {
            Ok(p) => p,
            Err(e) => {
                warn!("pipe #{}: mapping failed, dropping: {}", spec.id, e);
                continue;
            }
        };

        // Resolve the current target address (a target may have gone away since
        // the pipe was declared — that is a clean runtime drop, not an error).
        let to_id = ServerId::new(spec.to);
        let addr = match state.get_server(to_id).await {
            Some(s) => match deliverable_addr(&s.status, s.local_addr) {
                Some(a) => a,
                None => {
                    warn!(
                        "pipe #{}: target server #{} is not running/bound, dropping delivery",
                        spec.id, spec.to
                    );
                    continue;
                }
            },
            None => {
                warn!(
                    "pipe #{}: target server #{} no longer exists, dropping delivery",
                    spec.id, spec.to
                );
                continue;
            }
        };

        // Bounded fan-out: acquire a permit or drop-with-log. Never queue.
        let permit = match PIPE_INFLIGHT.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                warn!(
                    "pipe #{}: {} deliveries already in flight (cap), dropping this one",
                    spec.id, MAX_INFLIGHT_DELIVERIES
                );
                continue;
            }
        };

        let pipe_id = spec.id;
        tokio::spawn(async move {
            let _permit = permit; // released when the delivery finishes
            match deliver(addr, &payload).await {
                Ok(()) => debug!(
                    "pipe #{}: delivered {} bytes to server #{} ({})",
                    pipe_id,
                    payload.len(),
                    to_id.as_u32(),
                    addr
                ),
                Err(e) => warn!("pipe #{}: delivery to {} failed: {}", pipe_id, addr, e),
            }
        });
    }
}

/// Execute a `create_pipe` / `remove_pipe` action emitted by the LLM or an
/// operator. Handled here (and dispatched from the action executor) rather than
/// as a `CommonAction` variant, keeping the enum and its many exhaustive matches
/// untouched. `default_from` is the server context the action ran in; a
/// `create_pipe` may omit `from` to mean "this server".
pub async fn execute_pipe_action(
    action_name: &str,
    action: &Value,
    state: &AppState,
    default_from: Option<ServerId>,
) -> Result<()> {
    match action_name {
        "create_pipe" => {
            let on = action
                .get("on")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("create_pipe requires an 'on' event type"))?
                .to_string();
            let to = action
                .get("to")
                .and_then(json_as_u32)
                .ok_or_else(|| anyhow!("create_pipe requires a numeric 'to' server id"))?;
            let as_action = action
                .get("as")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("create_pipe requires an 'as' action"))?
                .to_string();
            let from =
                match action.get("from") {
                    Some(value) => ServerId::new(json_as_u32(value).ok_or_else(|| {
                        anyhow!("create_pipe requires a numeric 'from' server id")
                    })?),
                    None => default_from.ok_or_else(|| {
                        anyhow!("create_pipe has no 'from' and no server context to default to")
                    })?,
                };
            let map: BTreeMap<String, String> = match action.get("map") {
                Some(Value::Object(obj)) => obj
                    .iter()
                    .map(|(k, v)| {
                        v.as_str()
                            .map(|s| (k.clone(), s.to_string()))
                            .ok_or_else(|| {
                                anyhow!("create_pipe map field {k:?} must be a string template")
                            })
                    })
                    .collect::<Result<_>>()?,
                _ => bail!("create_pipe requires a 'map' object of string templates"),
            };
            let id = state
                .add_pipe(from, on, ServerId::new(to), as_action, map)
                .await?;
            debug!("create_pipe: created pipe #{}", id);
            Ok(())
        }
        "remove_pipe" => {
            let pipe_id = action
                .get("pipe_id")
                .and_then(json_as_u32)
                .ok_or_else(|| anyhow!("remove_pipe requires a numeric 'pipe_id'"))?;
            if state.remove_pipe(pipe_id).await.is_none() {
                bail!("remove_pipe: no pipe #{pipe_id}");
            }
            Ok(())
        }
        other => bail!("unknown pipe action {other:?}"),
    }
}

/// Accept a u32 from either a JSON number or a numeric string (the LLM emits
/// both).
fn json_as_u32(v: &Value) -> Option<u32> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}
