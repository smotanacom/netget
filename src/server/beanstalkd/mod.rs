//! Beanstalkd work-queue server — the model is the queue.
//!
//! The client speaks first, one command per CRLF line; `put` is followed by a body of the
//! declared length. The commands whose answer is *queue content* — put, reserve, the job
//! commands, stats, list-tubes — raise an event and the model answers with a structured action.
//! The commands whose answer is *this connection's own state* — `use`, `watch`, `ignore`,
//! `list-tube-used`, `list-tubes-watched`, `quit` — and every malformed or unknown line are
//! answered here without consulting anyone.
//!
//! Five properties worth knowing before changing the loop:
//!
//! 1. **Sizes are checked before anything is buffered.** A command line is capped at upstream's
//!    224 bytes ([`wire::MAX_LINE_BYTES`]); a `put` body at 65535 ([`wire::MAX_JOB_BYTES`]),
//!    judged from the *declared* `<bytes>` before a single body byte is read.
//! 2. **The model cannot write framing, and cannot answer the wrong command.** Its actions are
//!    rendered by [`wire`]; the loop then checks the reply fits the command
//!    ([`wire::reply_fits`]). A reply that does not fit, no reply, and a backend failure all
//!    answer `INTERNAL_ERROR` (`OUT_OF_MEMORY` — upstream's "try again later" — when the
//!    backend is at capacity), and the session continues: every one of those is a complete
//!    beanstalkd answer to one command.
//! 3. **A worker waiting in `reserve` is not idle.** When the model answers `reserve` with
//!    `wait_for_beanstalkd_job`, the server owes the worker an answer, so the idle deadline
//!    does not apply. `reserve-with-timeout` is answered `TIMED_OUT` by NetGet when its own
//!    timeout runs out; a plain `reserve` waits until a reply is written to the connection from
//!    elsewhere (the dashboard's `[ message ]`) or the worker hangs up. The connection cap is
//!    what bounds how many can wait.
//! 4. **The deadlines wrap the read, not the answer.** A command parked for a human under a
//!    `manual` rule is closed by neither `first_byte_timeout_secs` nor `idle_timeout_secs`.
//! 5. **Every close lingers** (see [`linger`]), so a refusal written just before closing reaches
//!    a peer that still has pipelined input in flight instead of being destroyed by an RST.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use anyhow::Result;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex, Notify};
use wire::Command;

pub use wire::{MAX_JOB_BYTES, MAX_LINE_BYTES};

/// How long a new connection may take to send its first command.
///
/// beanstalkd is client-speaks-first and real clients send a command as soon as they connect
/// (greenstalk sends `use`/`watch` from its constructor, or nothing until the first call). 300
/// seconds is the window a `manual` rule gives a human, for a NetGet TCP client parked on its
/// operator. A listener exposed to strangers should lower it through `first_byte_timeout_secs`.
const FIRST_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the server waits for the next command after answering one — also the bound on
/// each read of a `put` body. Upstream has no idle timeout at all; a producer that goes quiet
/// for five minutes reconnects. A worker waiting in `reserve` is exempt (see the module docs).
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Concurrent connections admitted before new ones are refused — the house default. It is
/// also what bounds the number of workers that can wait in `reserve` at once, since a waiting
/// reserve has no deadline of its own.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] reads as the answer to its first command:
/// `OUT_OF_MEMORY`, upstream's "the server cannot take this now; try again later".
const CONNECTION_CAP_REFUSAL: &[u8] = b"OUT_OF_MEMORY\r\n";

/// The peer-visible answers when the model could not or did not answer. Fixed literals, so
/// nothing derived from an error reaches the wire.
const UNAVAILABLE_REPLY: &[u8] = b"INTERNAL_ERROR\r\n";
const OVERLOADED_REPLY: &[u8] = b"OUT_OF_MEMORY\r\n";

pub struct BeanstalkdServer;

impl BeanstalkdServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        first_byte_timeout_secs: Option<u64>,
        idle_timeout_secs: Option<u64>,
    ) -> Result<SocketAddr> {
        let first_timeout = first_byte_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(FIRST_COMMAND_TIMEOUT);
        let idle_timeout = idle_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(IDLE_TIMEOUT);
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Beanstalkd server listening on {}", local_addr));

        let protocol = Arc::new(actions::BeanstalkdProtocol::new());
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "Beanstalkd",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((socket, peer_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = crate::utils::clock::Instant::now();
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
                                    protocol_info: ProtocolConnectionInfo::empty(),
                                },
                            )
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        Log::new(Some(&status_tx))
                            .info(format!("Beanstalkd client connected from {}", peer_addr));

                        let session = Session {
                            peer_addr,
                            llm_client: llm_client.clone(),
                            app_state: app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id,
                            protocol: protocol.clone(),
                            connection_id,
                            first_timeout,
                            idle_timeout,
                        };
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Released when the connection ends; this task is the whole
                                // connection, so MAX_CONNECTIONS caps live connections.
                                let _permit = permit;
                                session.run(socket).await
                            })
                            .await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Beanstalkd accept error: {}", e));
                        break;
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

/// Counts writes to one connection and wakes whoever is waiting on them.
///
/// A waiting `reserve` must end when *anything* answers it — including a reply injected from
/// the dashboard, which is written by `peer_support`'s own task and never passes through the
/// session loop. Watching the writer is the one place both paths meet.
#[derive(Default)]
struct WriteWatch {
    writes: AtomicU64,
    notify: Notify,
}

impl WriteWatch {
    fn count(&self) -> u64 {
        self.writes.load(Ordering::SeqCst)
    }
}

/// The connection's write half, reporting every non-empty write to its [`WriteWatch`].
struct WatchedWriter<W> {
    inner: W,
    watch: Arc<WriteWatch>,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for WatchedWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let polled = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &polled {
            if *n > 0 {
                self.watch.writes.fetch_add(1, Ordering::SeqCst);
                self.watch.notify.notify_waiters();
            }
        }
        polled
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

type Writer = Arc<Mutex<WatchedWriter<tokio::io::WriteHalf<tokio::net::TcpStream>>>>;

struct Session {
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::BeanstalkdProtocol>,
    connection_id: ConnectionId,
    first_timeout: Duration,
    idle_timeout: Duration,
}

/// The connection's own tube state: the one piece of state NetGet keeps, because it is about
/// this connection rather than about the queue.
struct Tubes {
    used: String,
    watched: Vec<String>,
}

/// What handling one command decided.
enum Next {
    Continue,
    /// The model left a reserve waiting for a job.
    Wait,
    Close,
}

impl Session {
    async fn run(self, socket: tokio::net::TcpStream) {
        let (mut reader, write_half) = tokio::io::split(socket);
        let watch = Arc::new(WriteWatch::default());
        let writer: Writer = Arc::new(Mutex::new(WatchedWriter {
            inner: write_half,
            watch: watch.clone(),
        }));

        // Registered before the first command, so the operator can reach this connection
        // while a command is parked for them or a worker waits in reserve.
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &self.app_state,
            self.server_id,
            self.connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            self.protocol.clone(),
            self.app_state.clone(),
            self.server_id,
            self.connection_id.as_u32(),
            writer.clone(),
            self.status_tx.clone(),
        );

        {
            let mut framer = Framer::new(&mut reader);
            self.session(&mut framer, &writer, &watch).await;
        }

        self.app_state
            .remove_peer_handle(self.server_id, self.connection_id.as_u32())
            .await;
        let _ = writer.lock().await.shutdown().await;
        linger(&mut reader).await;
        self.app_state
            .update_connection_status(
                self.server_id,
                self.connection_id,
                crate::state::server::ConnectionStatus::Closed,
            )
            .await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());
    }

    async fn write(&self, writer: &Writer, data: &[u8]) -> std::io::Result<()> {
        {
            let mut w = writer.lock().await;
            w.write_all(data).await?;
            w.flush().await?;
        }
        self.record_sent(data.len()).await;
        Ok(())
    }

    async fn record_sent(&self, n: usize) {
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                None,
                Some(n as u64),
                None,
                Some(1),
            )
            .await;
    }

    async fn record_received(&self, n: usize) {
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                Some(n as u64),
                None,
                Some(1),
                None,
            )
            .await;
    }

    async fn session<R: AsyncRead + Unpin>(
        &self,
        framer: &mut Framer<'_, R>,
        writer: &Writer,
        watch: &Arc<WriteWatch>,
    ) {
        let log = Log::new(Some(&self.status_tx));
        let mut tubes = Tubes {
            used: "default".to_string(),
            watched: vec!["default".to_string()],
        };
        let mut answered_one = false;

        loop {
            let timeout = if answered_one {
                self.idle_timeout
            } else {
                self.first_timeout
            };
            let (line, n) = match framer.next_line(timeout).await {
                LineRead::Line(line, n) => (line, n),
                LineRead::Eof => {
                    log.info(format!("Beanstalkd client {} disconnected", self.peer_addr));
                    return;
                }
                LineRead::TimedOut => {
                    log.info(format!(
                        "Beanstalkd client {} sent no command within {}s; closing",
                        self.peer_addr,
                        timeout.as_secs()
                    ));
                    return;
                }
                LineRead::TooLong => {
                    log.warn(format!(
                        "Beanstalkd command from {} exceeded {} bytes \
                         decision=fail_closed_line_too_long",
                        self.peer_addr, MAX_LINE_BYTES
                    ));
                    let _ = self.write(writer, b"BAD_FORMAT\r\n").await;
                    return;
                }
                LineRead::Failed(e) => {
                    log.error(format!(
                        "Beanstalkd read error from {}: {}",
                        self.peer_addr, e
                    ));
                    return;
                }
            };
            self.record_received(n).await;
            log.trace(format!(
                "Beanstalkd command from {}: {:?}",
                self.peer_addr, line
            ));
            answered_one = true;
            let command = wire::parse_command(&line);

            // Commands about this connection's own state, and every malformed line.
            let fixed: Option<Vec<u8>> = match &command {
                Command::Use(tube) => {
                    tubes.used = tube.clone();
                    Some(format!("USING {tube}\r\n").into_bytes())
                }
                Command::Watch(tube) => {
                    if !tubes.watched.contains(tube) {
                        tubes.watched.push(tube.clone());
                    }
                    Some(format!("WATCHING {}\r\n", tubes.watched.len()).into_bytes())
                }
                Command::Ignore(tube) => {
                    if tubes.watched.len() == 1 && tubes.watched[0] == *tube {
                        Some(b"NOT_IGNORED\r\n".to_vec())
                    } else {
                        tubes.watched.retain(|t| t != tube);
                        Some(format!("WATCHING {}\r\n", tubes.watched.len()).into_bytes())
                    }
                }
                Command::ListTubeUsed => Some(format!("USING {}\r\n", tubes.used).into_bytes()),
                Command::ListTubesWatched => Some(
                    wire::render_tube_list(&tubes.watched)
                        .map(String::into_bytes)
                        .unwrap_or_else(|_| UNAVAILABLE_REPLY.to_vec()),
                ),
                Command::Quit => return,
                Command::BadFormat => Some(b"BAD_FORMAT\r\n".to_vec()),
                Command::Unknown => Some(b"UNKNOWN_COMMAND\r\n".to_vec()),
                _ => None,
            };
            if let Some(reply) = fixed {
                if self.write(writer, &reply).await.is_err() {
                    return;
                }
                continue;
            }

            // put: the body, judged by its declared size before any of it is read.
            let mut body: Option<Vec<u8>> = None;
            if let Command::Put { bytes, .. } = &command {
                let declared = *bytes;
                match declared {
                    Some(n) if n <= MAX_JOB_BYTES as u64 => {
                        match framer.read_exact(n as usize + 2, self.idle_timeout).await {
                            Ok(mut data) => {
                                self.record_received(data.len()).await;
                                if !data.ends_with(b"\r\n") {
                                    log.warn(format!(
                                        "Beanstalkd put from {}: body not followed by CRLF \
                                         decision=fail_closed_expected_crlf",
                                        self.peer_addr
                                    ));
                                    if self.write(writer, b"EXPECTED_CRLF\r\n").await.is_err() {
                                        return;
                                    }
                                    continue;
                                }
                                data.truncate(n as usize);
                                body = Some(data);
                            }
                            Err(reason) => {
                                log.info(format!(
                                    "Beanstalkd put from {}: body never arrived ({reason}); \
                                     closing",
                                    self.peer_addr
                                ));
                                return;
                            }
                        }
                    }
                    _ => {
                        log.warn(format!(
                            "Beanstalkd put from {} declared {} bytes, over max-job-size {} \
                             decision=fail_closed_job_too_big",
                            self.peer_addr,
                            declared
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "more than 2^64".to_string()),
                            MAX_JOB_BYTES
                        ));
                        if self.write(writer, b"JOB_TOO_BIG\r\n").await.is_err() {
                            return;
                        }
                        // Stay in step with a client that sends the body anyway, as upstream
                        // does — unless the body is so large that skipping it is not worth it.
                        match declared.map(|n| n.saturating_add(2)) {
                            Some(skip) if skip <= wire::MAX_DISCARD_BYTES => {
                                if !framer.discard(skip, self.idle_timeout).await {
                                    return;
                                }
                                continue;
                            }
                            _ => return,
                        }
                    }
                }
            }

            match self
                .answer_with_model(&command, body.as_deref(), &tubes, writer)
                .await
            {
                Next::Continue => {}
                Next::Close => return,
                Next::Wait => {
                    if !self.wait_in_reserve(&command, framer, writer, watch).await {
                        return;
                    }
                }
            }
        }
    }

    /// Hold a worker whose reserve the model left waiting. Returns `false` when the connection
    /// should close.
    async fn wait_in_reserve<R: AsyncRead + Unpin>(
        &self,
        command: &Command,
        framer: &mut Framer<'_, R>,
        writer: &Writer,
        watch: &Arc<WriteWatch>,
    ) -> bool {
        let log = Log::new(Some(&self.status_tx));
        let deadline = match command {
            Command::ReserveWithTimeout(secs) => {
                Some(tokio::time::Instant::now() + Duration::from_secs(u64::from(*secs)))
            }
            _ => None,
        };
        let start = watch.count();
        loop {
            let notified = watch.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if watch.count() != start {
                log.info(format!(
                    "Beanstalkd reserve from {} answered by a reply written to the connection",
                    self.peer_addr
                ));
                return true;
            }
            let timer = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                read = framer.fill() => match read {
                    Ok(0) => {
                        log.info(format!(
                            "Beanstalkd client {} hung up while waiting in reserve",
                            self.peer_addr
                        ));
                        return false;
                    }
                    Ok(n) => {
                        log.warn(format!(
                            "Beanstalkd client {} sent {n} bytes while its reserve was waiting; \
                             the reserve is abandoned and the command is read",
                            self.peer_addr
                        ));
                        return true;
                    }
                    Err(e) => {
                        log.error(format!("Beanstalkd read error from {}: {}", self.peer_addr, e));
                        return false;
                    }
                },
                _ = timer => {
                    // Written only if nothing answered the reserve meanwhile — checked under
                    // the writer lock, which an injected reply also takes.
                    let wrote = {
                        let mut w = writer.lock().await;
                        if watch.count() != start {
                            false
                        } else {
                            if w.write_all(b"TIMED_OUT\r\n").await.is_err()
                                || w.flush().await.is_err()
                            {
                                return false;
                            }
                            true
                        }
                    };
                    if wrote {
                        self.record_sent(11).await;
                        log.info(format!(
                            "Beanstalkd reserve from {} decision=model_wait_timed_out",
                            self.peer_addr
                        ));
                    }
                    return true;
                }
                _ = &mut notified => continue,
            }
        }
    }

    /// Raise the command's event and write what the model decided.
    async fn answer_with_model(
        &self,
        command: &Command,
        body: Option<&[u8]>,
        tubes: &Tubes,
        writer: &Writer,
    ) -> Next {
        let log = Log::new(Some(&self.status_tx));
        let Some((event_type, data)) = event_for(command, body, tubes) else {
            return Next::Continue;
        };
        let event = Event::new(event_type, data);

        let result = match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let (category, reply) = match crate::utils::WireFailure::classify(&e) {
                    crate::utils::WireFailure::Overloaded => ("overloaded", OVERLOADED_REPLY),
                    crate::utils::WireFailure::Unavailable => ("unavailable", UNAVAILABLE_REPLY),
                };
                log.warn(format!(
                    "Beanstalkd {:?} from {} decision=fail_closed_llm_error category={}",
                    command, self.peer_addr, category
                ));
                log.debug(format!("Beanstalkd LLM call failed: {}", e));
                return match self.write(writer, reply).await {
                    Ok(()) => Next::Continue,
                    Err(_) => Next::Close,
                };
            }
        };
        for message in &result.messages {
            log.info(message);
        }

        let mut replies: Vec<Vec<u8>> = Vec::new();
        let mut close = false;
        let mut wait = false;
        let mut stack = result.protocol_results;
        stack.reverse();
        while let Some(item) = stack.pop() {
            match item {
                ActionResult::Output(bytes) => replies.push(bytes),
                ActionResult::CloseConnection => close = true,
                ActionResult::WaitForMore => wait = true,
                ActionResult::Multiple(items) => stack.extend(items.into_iter().rev()),
                _ => {}
            }
        }

        let is_reserve = matches!(command, Command::Reserve | Command::ReserveWithTimeout(_));
        let Some(reply) = replies.first() else {
            if wait && is_reserve {
                log.info(format!(
                    "Beanstalkd {:?} from {} decision=model_wait",
                    command, self.peer_addr
                ));
                return if close { Next::Close } else { Next::Wait };
            }
            let decision = if wait {
                "fail_closed_mismatched_reply"
            } else {
                "model_silent"
            };
            log.warn(format!(
                "Beanstalkd {:?} from {} decision={} ({} failed action(s)); answering \
                 INTERNAL_ERROR",
                command,
                self.peer_addr,
                decision,
                result.failures.len()
            ));
            return match self.write(writer, UNAVAILABLE_REPLY).await {
                Ok(()) if !close => Next::Continue,
                _ => Next::Close,
            };
        };

        if !wire::reply_fits(command, reply) {
            let (word, _) = wire::reply_head(reply);
            log.warn(format!(
                "Beanstalkd {:?} from {} decision=fail_closed_mismatched_reply reply={}; \
                 answering INTERNAL_ERROR",
                command, self.peer_addr, word
            ));
            return match self.write(writer, UNAVAILABLE_REPLY).await {
                Ok(()) if !close => Next::Continue,
                _ => Next::Close,
            };
        }
        if replies.len() > 1 {
            log.warn(format!(
                "Beanstalkd {:?}: the model produced {} replies to one command; sending the first",
                command,
                replies.len()
            ));
        }
        let (word, _) = wire::reply_head(reply);
        let decision = if wire::is_refusal(reply) {
            "model_reject"
        } else {
            "model_answer"
        };
        log.info(format!(
            "Beanstalkd {:?} from {} decision={} reply={}",
            command, self.peer_addr, decision, word
        ));
        if self.write(writer, reply).await.is_err() || close {
            return Next::Close;
        }
        Next::Continue
    }
}

/// The event a command raises and its data, for the commands the model answers.
fn event_for(
    command: &Command,
    body: Option<&[u8]>,
    tubes: &Tubes,
) -> Option<(&'static EventType, serde_json::Value)> {
    use actions::{
        BEANSTALKD_JOB_COMMAND_EVENT, BEANSTALKD_PUT_EVENT, BEANSTALKD_RESERVE_EVENT,
        BEANSTALKD_STATS_EVENT,
    };
    use serde_json::json;
    let used = &tubes.used;
    let job = |name: &str, id: Option<u64>| {
        let mut data = json!({"command": name, "tube": used});
        if let Some(id) = id {
            data["job_id"] = json!(id);
        }
        (&*BEANSTALKD_JOB_COMMAND_EVENT, data)
    };
    Some(match command {
        Command::Put {
            priority,
            delay,
            ttr,
            ..
        } => {
            let body = body.unwrap_or_default();
            (
                &*BEANSTALKD_PUT_EVENT,
                json!({
                    "tube": used,
                    "priority": priority,
                    "delay": delay,
                    "ttr": ttr,
                    "body": String::from_utf8_lossy(body),
                    "body_bytes": body.len(),
                }),
            )
        }
        Command::Reserve => (&*BEANSTALKD_RESERVE_EVENT, json!({"tubes": tubes.watched})),
        Command::ReserveWithTimeout(secs) => (
            &*BEANSTALKD_RESERVE_EVENT,
            json!({"tubes": tubes.watched, "timeout_secs": secs}),
        ),
        Command::ReserveJob(id) => job("reserve-job", Some(*id)),
        Command::Delete(id) => job("delete", Some(*id)),
        Command::Release {
            id,
            priority,
            delay,
        } => {
            let (event, mut data) = job("release", Some(*id));
            data["priority"] = json!(priority);
            data["delay"] = json!(delay);
            (event, data)
        }
        Command::Bury { id, priority } => {
            let (event, mut data) = job("bury", Some(*id));
            data["priority"] = json!(priority);
            (event, data)
        }
        Command::Touch(id) => job("touch", Some(*id)),
        Command::Peek(id) => job("peek", Some(*id)),
        Command::PeekReady => job("peek-ready", None),
        Command::PeekDelayed => job("peek-delayed", None),
        Command::PeekBuried => job("peek-buried", None),
        Command::Kick(bound) => {
            let (event, mut data) = job("kick", None);
            data["bound"] = json!(bound);
            (event, data)
        }
        Command::KickJob(id) => job("kick-job", Some(*id)),
        Command::PauseTube { tube, delay } => (
            &*BEANSTALKD_JOB_COMMAND_EVENT,
            json!({"command": "pause-tube", "tube": tube, "delay": delay}),
        ),
        Command::Stats => (&*BEANSTALKD_STATS_EVENT, json!({"scope": "server"})),
        Command::StatsTube(tube) => (
            &*BEANSTALKD_STATS_EVENT,
            json!({"scope": "tube", "tube": tube}),
        ),
        Command::StatsJob(id) => (
            &*BEANSTALKD_STATS_EVENT,
            json!({"scope": "job", "job_id": id}),
        ),
        Command::ListTubes => (&*BEANSTALKD_STATS_EVENT, json!({"scope": "tubes"})),
        _ => return None,
    })
}

/// How long, and how much, the server keeps reading after it has sent its last reply and
/// half-closed. Closing a socket with unread input makes the kernel send RST instead of FIN,
/// and an RST can destroy a reply the peer has not read yet — a `BAD_FORMAT` for an over-long
/// line with pipelined commands behind it would arrive as "connection reset". Bounded both
/// ways so a peer that keeps sending cannot hold the task open.
const LINGER_TIME: Duration = Duration::from_secs(2);
const LINGER_BYTES: usize = 64 * 1024;

async fn linger<R: AsyncRead + Unpin>(reader: &mut R) {
    let deadline = tokio::time::Instant::now() + LINGER_TIME;
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < LINGER_BYTES {
        match tokio::time::timeout_at(deadline, reader.read(&mut sink)).await {
            Ok(Ok(n)) if n > 0 => drained += n,
            _ => break,
        }
    }
}

enum LineRead {
    /// A line without its terminator, and the wire bytes it consumed.
    Line(String, usize),
    Eof,
    TimedOut,
    /// The line exceeded [`MAX_LINE_BYTES`] including its CRLF.
    TooLong,
    Failed(std::io::Error),
}

/// Accumulates reads into lines and bodies, keeping anything past what was asked for — a
/// client sends `put …\r\n<body>\r\n` in one write, and may pipeline further commands.
///
/// `pending` never holds more than one read past [`MAX_LINE_BYTES`] while a line is being
/// looked for, or one read past `MAX_JOB_BYTES + 2` while a body is.
struct Framer<'a, R> {
    reader: &'a mut R,
    pending: Vec<u8>,
    chunk: Vec<u8>,
}

impl<'a, R: AsyncRead + Unpin> Framer<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            chunk: vec![0u8; 4096],
        }
    }

    /// One read into `pending`. `Ok(0)` is EOF. Cancel-safe: a read that has not completed
    /// has consumed nothing.
    async fn fill(&mut self) -> std::io::Result<usize> {
        let n = self.reader.read(&mut self.chunk).await?;
        self.pending.extend_from_slice(&self.chunk[..n]);
        Ok(n)
    }

    /// `timeout` bounds the wait for *more bytes*, not the whole line.
    async fn next_line(&mut self, timeout: Duration) -> LineRead {
        loop {
            if let Some(idx) = self.pending.iter().position(|b| *b == b'\n') {
                let consumed = idx + 1;
                if consumed > MAX_LINE_BYTES {
                    return LineRead::TooLong;
                }
                let line: Vec<u8> = self.pending.drain(..consumed).collect();
                let text = String::from_utf8_lossy(&line[..idx]);
                return LineRead::Line(text.trim_end_matches('\r').to_string(), consumed);
            }
            // No newline in MAX_LINE_BYTES bytes: even if the next byte is one, the line is
            // longer than upstream allows.
            if self.pending.len() >= MAX_LINE_BYTES {
                return LineRead::TooLong;
            }
            match tokio::time::timeout(timeout, self.fill()).await {
                Err(_) => return LineRead::TimedOut,
                Ok(Ok(0)) => {
                    if self.pending.is_empty() {
                        return LineRead::Eof;
                    }
                    let line = std::mem::take(&mut self.pending);
                    let consumed = line.len();
                    let text = String::from_utf8_lossy(&line);
                    return LineRead::Line(text.trim_end_matches('\r').to_string(), consumed);
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return LineRead::Failed(e),
            }
        }
    }

    /// Exactly `n` bytes; `timeout` bounds each wait for more.
    async fn read_exact(&mut self, n: usize, timeout: Duration) -> Result<Vec<u8>, String> {
        while self.pending.len() < n {
            match tokio::time::timeout(timeout, self.fill()).await {
                Err(_) => return Err(format!("no bytes for {}s", timeout.as_secs())),
                Ok(Ok(0)) => return Err("EOF".to_string()),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e.to_string()),
            }
        }
        Ok(self.pending.drain(..n).collect())
    }

    /// Read and drop `n` bytes without keeping them. `false` if they never came.
    async fn discard(&mut self, mut n: u64, timeout: Duration) -> bool {
        loop {
            let take = (self.pending.len() as u64).min(n) as usize;
            self.pending.drain(..take);
            n -= take as u64;
            if n == 0 {
                return true;
            }
            match tokio::time::timeout(timeout, self.fill()).await {
                Ok(Ok(k)) if k > 0 => {}
                _ => return false,
            }
        }
    }
}
