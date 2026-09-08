//! NATS server implementation.
//!
//! The NATS client protocol is a line protocol: CRLF-terminated control lines, with
//! `PUB`/`HPUB` followed by a byte-counted payload. A server must send `INFO {json}` before
//! the client says anything, and must answer `PING` with `PONG` or the client tears the
//! connection down.
//!
//! # Two tasks per connection, and why
//!
//! * The **reader** frames the stream and answers everything that is not a decision: `PING`
//!   with `PONG`, and the per-command `+OK` a client that connected with `"verbose": true`
//!   expects. Neither costs an LLM call - `async-nats` writes `CONNECT` and `PING` in one
//!   flush and waits for the reply, so parking the keepalive behind a model round-trip (or
//!   behind a manual handler waiting for a human) would break connect itself.
//! * The **dispatcher** owns the subscription table and makes exactly one LLM call at a
//!   time, in arrival order. It is the state machine the root CLAUDE.md describes: busy is
//!   `Processing`, the bounded channel between the two tasks is the queue, and there is no
//!   `Accumulating` state because NATS frames are self-delimiting - there is never a partial
//!   message to accumulate, so `wait_for_more` would mean nothing here and is not offered.
//!
//! The channel is **bounded**. A publisher faster than the model backs pressure up through
//! the reader into TCP, which is the honest outcome; an unbounded queue would grow until the
//! process died.

pub mod actions;

use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::{
    NatsProtocol, NATS_CONNECT_EVENT, NATS_PUBLISH_EVENT, NATS_SUBSCRIBE_EVENT,
    NATS_UNSUBSCRIBE_EVENT,
};

/// NATS' own default control-line limit. A line longer than this without a CRLF is a peer
/// that is not speaking NATS.
///
/// This bounds one *line*. It does not bound the read buffer, which the previous wording
/// claimed it did: a `PUB` declares its own length, and the reader buffers toward it.
/// `MAX_BUFFERED_BYTES` below is what bounds the buffer.
pub const MAX_CONTROL_LINE: usize = 4096;

/// Hard ceiling on the reader's frame buffer, whatever any frame declares.
///
/// A frame is at most one control line plus `max_payload` plus its terminator, so anything
/// past that with no frame extractable is a peer that will never produce one. Without this,
/// `max_payload` was the only bound in the system and it was applied to a *declared* size,
/// not to what had actually been buffered.
const MAX_BUFFERED_SLACK: usize = MAX_CONTROL_LINE + 16;

/// How many `accept()` failures in a row before the listener is treated as dead.
///
/// Transient failures - descriptor exhaustion, a peer hanging up between SYN and accept -
/// clear on their own, so one of them must not end the accept loop. A listener that is
/// genuinely broken fails every time, and this bounds how long that takes to notice.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 64;

/// How many parsed frames may wait for the dispatcher before the reader stops reading.
const FRAME_QUEUE_CAPACITY: usize = 256;

pub struct NatsServer;

// ============================================================================
// Wire framing
// ============================================================================

/// One decodable client frame.
///
/// `Ping`/`Pong` are here rather than being handled inside the parser so that the parser
/// stays a pure function of the buffer - it is the part worth testing directly
/// (`tests/server/nats/e2e_test.rs` does, with no LLM calls).
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// `CONNECT {json}`
    Connect(serde_json::Value),
    /// `PUB`/`HPUB`. `headers` is empty for a plain `PUB`.
    Publish {
        subject: String,
        reply_to: Option<String>,
        headers: BTreeMap<String, String>,
        payload: Vec<u8>,
    },
    /// `SUB <subject> [queue] <sid>`
    Subscribe {
        subject: String,
        queue_group: Option<String>,
        sid: String,
    },
    /// `UNSUB <sid> [max]`
    Unsubscribe {
        sid: String,
        max_msgs: Option<u64>,
    },
    Ping,
    Pong,
}

/// A frame the peer cannot be allowed to continue after.
///
/// Each carries the text a real NATS server uses, because clients match on some of them
/// (`Maximum Payload Violation` in particular) and a novel string would be reported as an
/// unknown error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    UnknownProtocolOperation,
    MaximumControlLineExceeded,
    MaximumPayloadViolation,
    InvalidConnectConfig,
    InvalidSubject,
}

impl FrameError {
    pub fn wire_text(self) -> &'static str {
        match self {
            Self::UnknownProtocolOperation => "Unknown Protocol Operation",
            Self::MaximumControlLineExceeded => "Maximum Control Line Exceeded",
            Self::MaximumPayloadViolation => "Maximum Payload Violation",
            Self::InvalidConnectConfig => "Invalid Connect Config",
            Self::InvalidSubject => "Invalid Subject",
        }
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_text())
    }
}

/// Try to decode one frame from the front of `buf`.
///
/// Returns `Ok(None)` when the buffer holds only part of a frame - the caller reads more and
/// tries again - and `Ok(Some((frame, consumed)))` when one is complete. `consumed` is the
/// number of bytes to remove from the front.
///
/// Line endings: the protocol says CRLF, and every real client sends CRLF. A bare LF is
/// accepted too, so that a human poking at the port with `nc` gets sensible behaviour rather
/// than `Unknown Protocol Operation` on every line.
pub fn parse_frame(buf: &[u8], max_payload: u64) -> Result<Option<(Frame, usize)>, FrameError> {
    let skipped = blank_line_prefix_len(buf);
    match parse_one_frame(&buf[skipped..], max_payload)? {
        Some((frame, consumed)) => Ok(Some((frame, skipped + consumed))),
        None => Ok(None),
    }
}

/// Length of the leading run of blank lines (`\n`, `\r\n`, or a line of only whitespace).
///
/// Real servers ignore blank lines between frames. Skipping them iteratively is not a style
/// choice. The version this replaced recursed into `parse_frame` once per blank line — one
/// stack frame deeper each time, and not in tail position, since the returned offset had to
/// be adjusted after the call. A peer writing 8 KB of newlines, which is one `read`, recursed
/// about 8000 levels and overflowed the stack. A Rust stack overflow is `SIGSEGV`, not a
/// catchable panic, so `tokio::spawn` could not contain it and the whole NetGet process
/// aborted — every other server with it.
///
/// The reader drains this prefix before parsing as well. `parse_frame` folds it into
/// `consumed` when a frame follows, but when nothing follows it reports `Ok(None)` having
/// consumed nothing — so without the eager drain a peer sending only newlines would still
/// grow the read buffer by a chunk on every read, without bound.
pub fn blank_line_prefix_len(buf: &[u8]) -> usize {
    let mut offset = 0;
    while let Some(nl) = buf[offset..].iter().position(|&b| b == b'\n') {
        let line_end = offset + nl;
        let line = &buf[offset..line_end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.iter().all(|b| b.is_ascii_whitespace()) {
            break;
        }
        offset = line_end + 1;
    }
    offset
}

/// Decode one frame from a buffer whose first line is known not to be blank.
fn parse_one_frame(buf: &[u8], max_payload: u64) -> Result<Option<(Frame, usize)>, FrameError> {
    let Some(nl) = buf.iter().position(|&b| b == b'\n') else {
        if buf.len() > MAX_CONTROL_LINE {
            return Err(FrameError::MaximumControlLineExceeded);
        }
        return Ok(None);
    };
    if nl > MAX_CONTROL_LINE {
        return Err(FrameError::MaximumControlLineExceeded);
    }

    let mut line_end = nl;
    if line_end > 0 && buf[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    let after_line = nl + 1;

    let line = std::str::from_utf8(&buf[..line_end])
        .map_err(|_| FrameError::UnknownProtocolOperation)?
        .trim();

    let mut parts = line.split_whitespace();
    let verb = parts.next().ok_or(FrameError::UnknownProtocolOperation)?;
    let args: Vec<&str> = parts.collect();

    if verb.eq_ignore_ascii_case("PING") {
        return Ok(Some((Frame::Ping, after_line)));
    }
    if verb.eq_ignore_ascii_case("PONG") {
        return Ok(Some((Frame::Pong, after_line)));
    }
    if verb.eq_ignore_ascii_case("CONNECT") {
        // The document is the remainder of the line verbatim: splitting on whitespace would
        // break any JSON string containing a space.
        let json_start = line
            .find(char::is_whitespace)
            .ok_or(FrameError::InvalidConnectConfig)?;
        let doc: serde_json::Value = serde_json::from_str(line[json_start..].trim())
            .map_err(|_| FrameError::InvalidConnectConfig)?;
        if !doc.is_object() {
            return Err(FrameError::InvalidConnectConfig);
        }
        return Ok(Some((Frame::Connect(doc), after_line)));
    }
    if verb.eq_ignore_ascii_case("SUB") {
        // SUB <subject> [queue group] <sid>
        let (subject, queue_group, sid) = match args.len() {
            2 => (args[0], None, args[1]),
            3 => (args[0], Some(args[1].to_string()), args[2]),
            _ => return Err(FrameError::UnknownProtocolOperation),
        };
        if subject.is_empty() {
            return Err(FrameError::InvalidSubject);
        }
        return Ok(Some((
            Frame::Subscribe {
                subject: subject.to_string(),
                queue_group,
                sid: sid.to_string(),
            },
            after_line,
        )));
    }
    if verb.eq_ignore_ascii_case("UNSUB") {
        // UNSUB <sid> [max_msgs]
        let sid = args.first().ok_or(FrameError::UnknownProtocolOperation)?;
        let max_msgs = match args.get(1) {
            Some(v) => Some(
                v.parse::<u64>()
                    .map_err(|_| FrameError::UnknownProtocolOperation)?,
            ),
            None => None,
        };
        if args.len() > 2 {
            return Err(FrameError::UnknownProtocolOperation);
        }
        return Ok(Some((
            Frame::Unsubscribe {
                sid: sid.to_string(),
                max_msgs,
            },
            after_line,
        )));
    }

    let is_hpub = verb.eq_ignore_ascii_case("HPUB");
    if is_hpub || verb.eq_ignore_ascii_case("PUB") {
        // PUB  <subject> [reply-to] <#bytes>
        // HPUB <subject> [reply-to] <#header bytes> <#total bytes>
        let (subject, reply_to, header_len, total_len) = if is_hpub {
            match args.len() {
                3 => (args[0], None, parse_len(args[1])?, parse_len(args[2])?),
                4 => (
                    args[0],
                    Some(args[1].to_string()),
                    parse_len(args[2])?,
                    parse_len(args[3])?,
                ),
                _ => return Err(FrameError::UnknownProtocolOperation),
            }
        } else {
            match args.len() {
                2 => (args[0], None, 0, parse_len(args[1])?),
                3 => (args[0], Some(args[1].to_string()), 0, parse_len(args[2])?),
                _ => return Err(FrameError::UnknownProtocolOperation),
            }
        };

        if subject.is_empty() {
            return Err(FrameError::InvalidSubject);
        }
        if header_len > total_len {
            return Err(FrameError::UnknownProtocolOperation);
        }
        // The limit applies to the whole declared size, header block included, which is what
        // nats-server's processHeaderPub does (`c.pa.size > c.mpay`, where `size` is the
        // total). Bounding only `total_len - header_len` left `header_len` itself unbounded,
        // and because `header_len == total_len` yields a zero-length body it passed both
        // checks: `HPUB x 4000000000 4000000000` is 30 bytes on the wire and made the reader
        // buffer its way toward 4 GB, while `HPUB x <usize::MAX> <usize::MAX>` overflowed the
        // `after_line + total_len` index below — a panic in debug, and in release a wrap to a
        // `start > end` slice that panicked one line further on. `PUB` was safe only by
        // accident, its header length being a literal `0`.
        if total_len as u64 > max_payload {
            return Err(FrameError::MaximumPayloadViolation);
        }

        // `max_payload` is caller-supplied and may itself be enormous; a declared size must
        // never be able to wrap the index that reads it.
        let Some(body_end) = after_line.checked_add(total_len) else {
            return Err(FrameError::MaximumPayloadViolation);
        };
        if buf.len() <= body_end {
            return Ok(None);
        }
        let terminator = match buf[body_end] {
            b'\r' => {
                if buf.len() < body_end + 2 {
                    return Ok(None);
                }
                if buf[body_end + 1] != b'\n' {
                    return Err(FrameError::UnknownProtocolOperation);
                }
                2
            }
            b'\n' => 1,
            _ => return Err(FrameError::UnknownProtocolOperation),
        };

        let headers = if header_len > 0 {
            parse_headers(&buf[after_line..after_line + header_len])
        } else {
            BTreeMap::new()
        };
        let payload = buf[after_line + header_len..body_end].to_vec();

        return Ok(Some((
            Frame::Publish {
                subject: subject.to_string(),
                reply_to,
                headers,
                payload,
            },
            body_end + terminator,
        )));
    }

    Err(FrameError::UnknownProtocolOperation)
}

fn parse_len(token: &str) -> Result<usize, FrameError> {
    token
        .parse::<usize>()
        .map_err(|_| FrameError::UnknownProtocolOperation)
}

/// Decode a `NATS/1.0` header block into name/value pairs.
///
/// The version line is skipped, malformed lines are dropped rather than failing the frame -
/// a header a client invented is not a reason to kill its connection - and names are
/// reported as sent.
pub fn parse_headers(block: &[u8]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let text = String::from_utf8_lossy(block);
    for (index, line) in text.split('\n').enumerate() {
        let line = line.trim_end_matches('\r');
        if index == 0 || line.trim().is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty() {
                out.insert(name.to_string(), value.trim().to_string());
            }
        }
    }
    out
}

/// NATS subject matching: `*` matches exactly one token, `>` matches one or more remaining
/// tokens and may only appear last.
pub fn subject_matches(pattern: &str, subject: &str) -> bool {
    let pattern_tokens: Vec<&str> = pattern.split('.').collect();
    let subject_tokens: Vec<&str> = subject.split('.').collect();

    for (i, token) in pattern_tokens.iter().enumerate() {
        if *token == ">" {
            return i + 1 == pattern_tokens.len() && subject_tokens.len() > i;
        }
        if i >= subject_tokens.len() {
            return false;
        }
        if *token != "*" && *token != subject_tokens[i] {
            return false;
        }
    }
    pattern_tokens.len() == subject_tokens.len()
}

// ============================================================================
// Server
// ============================================================================

/// One subscription as the client declared it.
///
/// This is live connection state, not storage: it exists only to tell the model which
/// subscriptions a publish could plausibly go to, and it dies with the connection. Nothing
/// is delivered because of it.
#[derive(Debug, Clone)]
struct Subscription {
    subject: String,
    queue_group: Option<String>,
}

impl NatsServer {
    /// Bind, and return only once the listener is accepting.
    ///
    /// The bind is awaited before returning, so a port that is taken (or privileged) is an
    /// `Err` the caller turns into `ServerStatus::Error` rather than a server that reports
    /// `Running` with no socket.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        server_name: String,
        max_payload: u64,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!(
            "NATS server listening on {} (server_name={}, max_payload={})",
            local_addr, server_name, max_payload
        ));

        let protocol = Arc::new(NatsProtocol::new());
        let task_registrar = app_state.clone();

        let accept_handle = tokio::spawn(async move {
            // `accept()` fails transiently when the process is briefly out of descriptors
            // (EMFILE/ENFILE) or a peer hangs up between SYN and accept (ECONNABORTED).
            // Breaking on those stopped the listener for good while `AppState` still reported
            // the server `Running` - a server that lies about being up, arrived at from the
            // other direction. Retry with a short backoff, and give up only when the failures
            // are relentless enough that the listener really is dead.
            let mut consecutive_accept_errors = 0u32;
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        consecutive_accept_errors = 0;
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);

                        if let Err(e) = accept_connection(
                            stream,
                            peer_addr,
                            local_addr_conn,
                            connection_id,
                            server_id,
                            &llm_client,
                            &app_state,
                            &status_tx,
                            &protocol,
                            &server_name,
                            max_payload,
                        )
                        .await
                        {
                            Log::new(Some(&status_tx)).error(format!(
                                "NATS could not set up connection from {}: {}",
                                peer_addr, e
                            ));
                        }
                    }
                    Err(e) => {
                        consecutive_accept_errors += 1;
                        if consecutive_accept_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                            Log::new(Some(&status_tx)).error(format!(
                                "NATS accept failed {} times in a row ({}); listener stopping",
                                consecutive_accept_errors, e
                            ));
                            break;
                        }
                        Log::new(Some(&status_tx))
                            .warn(format!("NATS accept error: {} - retrying", e));
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

/// Register one accepted connection, greet it with `INFO`, and start its two tasks.
#[allow(clippy::too_many_arguments)]
async fn accept_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    protocol: &Arc<NatsProtocol>,
    server_name: &str,
    max_payload: u64,
) -> Result<()> {
    let (read_half, write_half) = tokio::io::split(stream);
    let write_half = Arc::new(Mutex::new(write_half));

    use crate::state::server::{
        ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
    };
    let now = std::time::Instant::now();
    app_state
        .add_connection_to_server(
            server_id,
            ServerConnectionState {
                id: connection_id,
                remote_addr: peer_addr,
                local_addr,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: now,
                status: ConnectionStatus::Active,
                status_changed_at: now,
                protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                    "state": "Idle"
                })),
            },
        )
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());

    let log = Log::new(Some(status_tx));
    log.info(format!("NATS client connected from {}", peer_addr));

    // Peer messaging, registered before the greeting: a NATS connection can sit for minutes
    // with a parked event, and the operator must be able to reach it meanwhile.
    let peer_rx = crate::server::peer_support::register_peer_channel(
        app_state,
        server_id,
        connection_id.as_u32(),
    )
    .await;
    crate::server::peer_support::spawn_peer_command_task(
        peer_rx,
        protocol.clone(),
        app_state.clone(),
        server_id,
        connection_id.as_u32(),
        write_half.clone(),
        status_tx.clone(),
    );

    // The server speaks first, always, and before the client will say anything at all.
    let info = actions::build_info_json(
        server_name,
        max_payload,
        &local_addr.ip().to_string(),
        local_addr.port(),
        connection_id.as_u32() as u64,
        &peer_addr.ip().to_string(),
    );
    let greeting = format!("INFO {}\r\n", info);
    log.debug(format!("NATS -> {} INFO greeting", peer_addr));
    log.trace(format!("NATS INFO: {}", info));
    write_counted(
        &write_half,
        greeting.as_bytes(),
        app_state,
        server_id,
        connection_id,
    )
    .await?;

    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(FRAME_QUEUE_CAPACITY);
    let (close_tx, close_rx) = oneshot::channel::<()>();

    let reader_handle = tokio::spawn(run_reader(
        read_half,
        write_half.clone(),
        frame_tx,
        close_rx,
        app_state.clone(),
        status_tx.clone(),
        server_id,
        connection_id,
        peer_addr,
        max_payload,
    ));
    let dispatcher_handle = tokio::spawn(run_dispatcher(
        frame_rx,
        close_tx,
        write_half,
        llm_client.clone(),
        app_state.clone(),
        status_tx.clone(),
        server_id,
        connection_id,
        peer_addr,
        protocol.clone(),
    ));

    // Both, not just the accept loop: aborting a task does not abort the tasks it spawned,
    // so a connection whose reader was left registered nowhere would keep the socket alive
    // after stop_server.
    app_state
        .register_server_task(server_id, reader_handle)
        .await;
    app_state
        .register_server_task(server_id, dispatcher_handle)
        .await;

    Ok(())
}

/// Write one frame and count it against the connection.
///
/// The guard is dropped before the stats update, so nothing awaits an `AppState` lock while
/// holding the write half.
async fn write_counted<W>(
    write_half: &Arc<Mutex<W>>,
    data: &[u8],
    app_state: &AppState,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    {
        let mut writer = write_half.lock().await;
        writer.write_all(data).await?;
        writer.flush().await?;
    }
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            None,
            Some(data.len() as u64),
            None,
            Some(1),
        )
        .await;
    Ok(())
}

/// Frame the inbound stream, answer the non-decisions, hand everything else to the
/// dispatcher.
#[allow(clippy::too_many_arguments)]
async fn run_reader(
    mut read_half: tokio::io::ReadHalf<TcpStream>,
    write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    frame_tx: mpsc::Sender<Frame>,
    mut close_rx: oneshot::Receiver<()>,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    peer_addr: SocketAddr,
    max_payload: u64,
) {
    let log = Log::new(Some(&status_tx));
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = vec![0u8; 8192];
    // Set from the client's CONNECT. In verbose mode a real server acknowledges every
    // command with +OK, and clients that asked for it wait for those acknowledgements.
    let mut verbose = false;

    'session: loop {
        let n = tokio::select! {
            // Resolves when the dispatcher closed the connection, and also (as an Err) when
            // the dispatcher task is gone. Either way there is nothing left to read for.
            _ = &mut close_rx => break 'session,
            result = read_half.read(&mut chunk) => match result {
                Ok(0) => {
                    log.info(format!("NATS client {} disconnected", peer_addr));
                    break 'session;
                }
                Ok(n) => n,
                Err(e) => {
                    log.error(format!("NATS read error from {}: {}", peer_addr, e));
                    break 'session;
                }
            }
        };

        buf.extend_from_slice(&chunk[..n]);
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(n as u64),
                None,
                Some(1),
                None,
            )
            .await;
        log.debug(format!("NATS received {} bytes from {}", n, peer_addr));

        loop {
            // `parse_frame` folds a leading run of blank lines into `consumed` when a frame
            // follows one, but reports `Ok(None)` having consumed nothing when none does.
            // Draining here is what stops a peer that sends only newlines from growing `buf`
            // by a chunk on every read for as long as it cares to keep typing.
            let blank = blank_line_prefix_len(&buf);
            if blank > 0 {
                buf.drain(..blank);
            }
            match parse_frame(&buf, max_payload) {
                Ok(None) => {
                    // Nothing extractable, and more buffered than any single frame could
                    // ever need. The peer is not going to complete one.
                    if buf.len() as u64 > max_payload.saturating_add(MAX_BUFFERED_SLACK as u64) {
                        log.warn(format!(
                            "NATS buffered {} bytes from {} with no complete frame - \
                             closing connection",
                            buf.len(),
                            peer_addr
                        ));
                        let _ = write_counted(
                            &write_half,
                            format!(
                                "-ERR '{}'\r\n",
                                FrameError::MaximumPayloadViolation.wire_text()
                            )
                            .as_bytes(),
                            &app_state,
                            server_id,
                            connection_id,
                        )
                        .await;
                        let _ = write_half.lock().await.shutdown().await;
                        break 'session;
                    }
                    break;
                }
                Ok(Some((frame, consumed))) => {
                    buf.drain(..consumed);

                    // PING/PONG and the verbose acknowledgements are protocol bookkeeping,
                    // never a decision, and are answered here without an LLM call.
                    match &frame {
                        Frame::Ping => {
                            log.debug(format!("NATS PING from {} -> PONG", peer_addr));
                            if write_counted(
                                &write_half,
                                b"PONG\r\n",
                                &app_state,
                                server_id,
                                connection_id,
                            )
                            .await
                            .is_err()
                            {
                                break 'session;
                            }
                            continue;
                        }
                        Frame::Pong => {
                            log.trace(format!("NATS PONG from {}", peer_addr));
                            continue;
                        }
                        Frame::Connect(doc) => {
                            verbose = doc
                                .get("verbose")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                        }
                        _ => {}
                    }

                    if verbose
                        && write_counted(
                            &write_half,
                            b"+OK\r\n",
                            &app_state,
                            server_id,
                            connection_id,
                        )
                        .await
                        .is_err()
                    {
                        break 'session;
                    }

                    if frame_tx.send(frame).await.is_err() {
                        // The dispatcher is gone; nothing can answer any further frame.
                        break 'session;
                    }
                }
                Err(e) => {
                    // A malformed frame is unrecoverable: the byte stream is no longer
                    // aligned to a frame boundary. A real server answers -ERR and hangs up,
                    // and so does this one. Nothing here is derived from an internal error.
                    log.warn(format!(
                        "NATS protocol error from {}: {} - closing connection",
                        peer_addr, e
                    ));
                    let _ = write_counted(
                        &write_half,
                        format!("-ERR '{}'\r\n", e.wire_text()).as_bytes(),
                        &app_state,
                        server_id,
                        connection_id,
                    )
                    .await;
                    let _ = write_half.lock().await.shutdown().await;
                    break 'session;
                }
            }
        }
    }

    app_state
        .remove_peer_handle(server_id, connection_id.as_u32())
        .await;
    app_state
        .close_connection_on_server(server_id, connection_id)
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());
}

/// Ask the model about each decision-bearing frame, in order, one at a time.
#[allow(clippy::too_many_arguments)]
async fn run_dispatcher(
    mut frame_rx: mpsc::Receiver<Frame>,
    close_tx: oneshot::Sender<()>,
    write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    connection_id: ConnectionId,
    peer_addr: SocketAddr,
    protocol: Arc<NatsProtocol>,
) {
    let log = Log::new(Some(&status_tx));
    // Owned by this task alone, so there is no lock to hold across the LLM call.
    let mut subscriptions: HashMap<String, Subscription> = HashMap::new();
    let mut close_tx = Some(close_tx);

    while let Some(frame) = frame_rx.recv().await {
        let event = match &frame {
            Frame::Connect(doc) => Event::new(
                &NATS_CONNECT_EVENT,
                serde_json::json!({
                    "options": doc,
                    "client_name": doc.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    "lang": doc.get("lang").and_then(|v| v.as_str()).unwrap_or(""),
                    "verbose": doc.get("verbose").and_then(|v| v.as_bool()).unwrap_or(false),
                }),
            ),
            Frame::Publish {
                subject,
                reply_to,
                headers,
                payload,
            } => {
                // Same convention as tcp_data_received: printable text stays text, anything
                // else is hex, and `payload_encoding` says which so the model can echo it
                // back symmetrically.
                let (payload_str, encoding) = if payload
                    .iter()
                    .all(|&b| b.is_ascii_graphic() || b.is_ascii_whitespace())
                {
                    (String::from_utf8_lossy(payload).to_string(), "utf8")
                } else {
                    (hex::encode(payload), "hex")
                };

                let matching: Vec<serde_json::Value> = subscriptions
                    .iter()
                    .filter(|(_, sub)| subject_matches(&sub.subject, subject))
                    .map(|(sid, sub)| {
                        serde_json::json!({
                            "sid": sid,
                            "subject": sub.subject,
                            "queue_group": sub.queue_group,
                        })
                    })
                    .collect();

                Event::new(
                    &NATS_PUBLISH_EVENT,
                    serde_json::json!({
                        "subject": subject,
                        "reply_to": reply_to,
                        "payload": payload_str,
                        "payload_encoding": encoding,
                        "headers": headers,
                        "matching_subscriptions": matching,
                    }),
                )
            }
            Frame::Subscribe {
                subject,
                queue_group,
                sid,
            } => {
                subscriptions.insert(
                    sid.clone(),
                    Subscription {
                        subject: subject.clone(),
                        queue_group: queue_group.clone(),
                    },
                );
                Event::new(
                    &NATS_SUBSCRIBE_EVENT,
                    serde_json::json!({
                        "subject": subject,
                        "queue_group": queue_group,
                        "sid": sid,
                    }),
                )
            }
            Frame::Unsubscribe { sid, max_msgs } => {
                // An UNSUB with a max is an auto-unsubscribe *after* that many more
                // messages. Deliveries are not counted here (see CLAUDE.md), so the
                // subscription stays targetable and the model is told the threshold; a
                // plain UNSUB removes it.
                let subject = subscriptions.get(sid).map(|s| s.subject.clone());
                if max_msgs.is_none() {
                    subscriptions.remove(sid);
                }
                Event::new(
                    &NATS_UNSUBSCRIBE_EVENT,
                    serde_json::json!({
                        "sid": sid,
                        "subject": subject,
                        "max_msgs": max_msgs,
                    }),
                )
            }
            // Answered by the reader; never forwarded.
            Frame::Ping | Frame::Pong => continue,
        };

        let outcome = call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await;

        let mut should_close = false;
        match outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }

                let mut wrote_output = false;
                let mut stack: Vec<ActionResult> = result.protocol_results;
                stack.reverse();
                while let Some(item) = stack.pop() {
                    match item {
                        ActionResult::Output(bytes) => {
                            log.debug(format!("NATS sent {} bytes to {}", bytes.len(), peer_addr));
                            log.trace(format!(
                                "NATS sent: {}",
                                crate::utils::truncate_for_log(
                                    &String::from_utf8_lossy(&bytes),
                                    512
                                )
                            ));
                            if write_counted(
                                &write_half,
                                &bytes,
                                &app_state,
                                server_id,
                                connection_id,
                            )
                            .await
                            .is_err()
                            {
                                should_close = true;
                                break;
                            }
                            wrote_output = true;
                        }
                        ActionResult::Multiple(items) => {
                            for inner in items.into_iter().rev() {
                                stack.push(inner);
                            }
                        }
                        ActionResult::CloseConnection => should_close = true,
                        _ => {}
                    }
                }

                // Saying nothing is a legitimate answer in NATS - most publishes to a
                // subject nobody subscribed to produce no frame at all - so this is a debug
                // line, distinguishable in the log from a backend failure below.
                if !wrote_output && !should_close {
                    log.debug(format!(
                        "No NATS frame for {} on {}: decision=model_no_actions",
                        event.id(),
                        connection_id
                    ));
                }
            }
            Err(e) => {
                let failure = crate::utils::WireFailure::classify(&e);
                let class = if failure.is_overloaded() {
                    "overloaded"
                } else {
                    "unavailable"
                };
                // The peer gets a category; the operator's log gets the error.
                log.warn(format!(
                    "NATS LLM error for {} on {}: decision=fail_closed_llm_error class={class} error={e}",
                    event.id(),
                    connection_id
                ));
                let _ = write_counted(
                    &write_half,
                    format!("-ERR '{}'\r\n", failure.prefixed_text()).as_bytes(),
                    &app_state,
                    server_id,
                    connection_id,
                )
                .await;
                // NATS clients treat -ERR as fatal, and this server has nothing further to
                // say on a connection it cannot answer for: hang up so the peer learns now
                // rather than at its own timeout.
                should_close = true;
            }
        }

        if should_close {
            let _ = write_half.lock().await.shutdown().await;
            app_state
                .remove_peer_handle(server_id, connection_id.as_u32())
                .await;
            app_state
                .close_connection_on_server(server_id, connection_id)
                .await;
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            log.info(format!(
                "NATS connection {} closed by the server",
                connection_id
            ));
            break;
        }
    }

    // Wakes the reader out of its read(), whether we closed the connection or simply ran
    // out of frames.
    if let Some(tx) = close_tx.take() {
        let _ = tx.send(());
    }
}
