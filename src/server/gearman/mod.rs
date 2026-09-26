//! Gearman job server — the model is the worker for every function.
//!
//! Clients speak the binary protocol (`\0REQ` packets) and administrators the text protocol
//! (`status`, `workers`, …); a leading `\0` says which one the next message is, so a connection
//! may mix them. A submitted job gets a NetGet-generated handle in `JOB_CREATED` at once, then
//! raises `gearman_job_submitted`; the model's answer becomes `WORK_STATUS`/`WORK_DATA` and one
//! outcome packet.
//!
//! Five properties worth knowing before changing the loop:
//!
//! 1. **Sizes are judged before anything is allocated.** A packet's declared size is checked
//!    against [`wire::MAX_PACKET_BYTES`] from its 12-byte header; an admin line is capped at
//!    [`wire::MAX_ADMIN_LINE`]. Either overrun is refused and the connection closed.
//! 2. **The model cannot write a packet, and cannot answer another job.** Its answers are
//!    rendered by [`wire`] with the handle of the job being answered; an answer that names
//!    another handle, or a packet that is not a job answer, is refused. A foreground job always
//!    ends with exactly one outcome: if the model gave none, NetGet sends `WORK_FAIL`.
//! 3. **Worker connections are refused, not half-served.** `CAN_DO`, `GRAB_JOB` and the rest
//!    get `ERROR not_supported` and the connection closes: the model is the worker, and a real
//!    worker that registered here would sleep forever waiting for a job NetGet never queues.
//! 4. **The deadlines wrap reads, not answers.** A client waiting for its job's result — the
//!    model thinking, or the event parked for a human — is not idle.
//! 5. **NetGet keeps only the jobs in flight** (handle, function, last progress), for
//!    `GET_STATUS` and the admin `status`; a job leaves the table when its answer is written.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::Work;
use anyhow::Result;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

pub use wire::MAX_PACKET_BYTES;

/// How long a new connection may send nothing. The `gearman` CLI writes its request at once;
/// 300 s is the window a `manual` rule gives a human, for a NetGet TCP client parked on its
/// operator. Lower it through `first_byte_timeout_secs` for a public listener.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the server waits for the next message after answering one, and for the rest of a
/// message that has started arriving.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Concurrent connections admitted before new ones are refused — the house default.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// Fixed `WORK_EXCEPTION` texts for a backend failure, for clients that enabled exceptions.
const EXCEPTION_UNAVAILABLE: &[u8] = b"job server backend unavailable";
const EXCEPTION_OVERLOADED: &[u8] = b"job server backend at capacity";

/// A job the model is working on.
#[derive(Debug, Clone)]
struct InFlight {
    function: String,
    numerator: u64,
    denominator: u64,
}

/// The jobs in flight on one server, by handle, and the handle counter.
#[derive(Default)]
struct Jobs {
    next: AtomicU64,
    table: std::sync::Mutex<BTreeMap<Vec<u8>, InFlight>>,
}

impl Jobs {
    fn start(&self, function: &str) -> Vec<u8> {
        let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let handle = format!("H:netget:{n}").into_bytes();
        self.table.lock().unwrap().insert(
            handle.clone(),
            InFlight {
                function: function.to_string(),
                numerator: 0,
                denominator: 0,
            },
        );
        handle
    }
    fn finish(&self, handle: &[u8]) {
        self.table.lock().unwrap().remove(handle);
    }
    fn get(&self, handle: &[u8]) -> Option<InFlight> {
        self.table.lock().unwrap().get(handle).cloned()
    }
    /// `(function, total, running, available workers)` for the admin `status`.
    fn by_function(&self) -> Vec<(String, u64, u64, u64)> {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for job in self.table.lock().unwrap().values() {
            *counts.entry(job.function.clone()).or_default() += 1;
        }
        counts.into_iter().map(|(f, n)| (f, n, n, 0)).collect()
    }
}

pub struct GearmanServer;

impl GearmanServer {
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
            .unwrap_or(FIRST_BYTE_TIMEOUT);
        let idle_timeout = idle_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(IDLE_TIMEOUT);
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Gearman job server listening on {}", local_addr));

        let protocol = Arc::new(actions::GearmanProtocol::new());
        let jobs = Arc::new(Jobs::default());
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                // A peer over the cap gets no bytes: a packet it did not ask for would be read
                // as the answer to its first request.
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    b"",
                    "Gearman",
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
                            .info(format!("Gearman client connected from {}", peer_addr));

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
                            jobs: jobs.clone(),
                        };
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                let _permit = permit;
                                session.run(socket).await
                            })
                            .await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Gearman accept error: {}", e));
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

type Writer = Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>;

struct Session {
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::GearmanProtocol>,
    connection_id: ConnectionId,
    first_timeout: Duration,
    idle_timeout: Duration,
    jobs: Arc<Jobs>,
}

/// One message read from the connection.
enum Message {
    Packet(wire::Header, Vec<u8>),
    Admin(String),
}

enum ReadError {
    Eof,
    TimedOut,
    TooLarge(u32),
    BadMagic,
    LineTooLong,
    Io(std::io::Error),
}

impl Session {
    async fn run(self, socket: tokio::net::TcpStream) {
        let (mut reader, write_half) = tokio::io::split(socket);
        let writer: Writer = Arc::new(Mutex::new(write_half));

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
            self.session(&mut framer, &writer).await;
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

    async fn write(&self, writer: &Writer, data: &[u8]) -> bool {
        let ok = {
            let mut w = writer.lock().await;
            w.write_all(data).await.is_ok() && w.flush().await.is_ok()
        };
        if ok {
            self.app_state
                .update_connection_stats(
                    self.server_id,
                    self.connection_id,
                    None,
                    Some(data.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
        ok
    }

    async fn error(&self, writer: &Writer, code: &str, text: &str) -> bool {
        match wire::error(code, text) {
            Ok(packet) => self.write(writer, &packet).await,
            Err(_) => false,
        }
    }

    async fn session<R: AsyncRead + Unpin>(&self, framer: &mut Framer<'_, R>, writer: &Writer) {
        let log = Log::new(Some(&self.status_tx));
        let mut exceptions = false;
        let mut first = true;

        loop {
            let timeout = if first {
                self.first_timeout
            } else {
                self.idle_timeout
            };
            let message = match framer.next_message(timeout, self.idle_timeout).await {
                Ok(m) => m,
                Err(ReadError::Eof) => {
                    log.info(format!("Gearman client {} disconnected", self.peer_addr));
                    return;
                }
                Err(ReadError::TimedOut) => {
                    log.info(format!(
                        "Gearman client {} sent nothing within {}s; closing",
                        self.peer_addr,
                        timeout.as_secs()
                    ));
                    return;
                }
                Err(ReadError::TooLarge(size)) => {
                    log.warn(format!(
                        "Gearman packet from {} declared {} bytes, over {} \
                         decision=fail_closed_too_large",
                        self.peer_addr,
                        size,
                        wire::MAX_PACKET_BYTES
                    ));
                    self.error(
                        writer,
                        "too_large",
                        "packet exceeds the server's size limit",
                    )
                    .await;
                    return;
                }
                Err(ReadError::BadMagic) => {
                    log.warn(format!(
                        "Gearman peer {} sent a packet without \\0REQ decision=fail_closed_bad_magic",
                        self.peer_addr
                    ));
                    return;
                }
                Err(ReadError::LineTooLong) => {
                    log.warn(format!(
                        "Gearman admin line from {} exceeded {} bytes \
                         decision=fail_closed_line_too_long",
                        self.peer_addr,
                        wire::MAX_ADMIN_LINE
                    ));
                    let _ = self
                        .write(
                            writer,
                            wire::admin_error("LINE_TOO_LONG", "Command line too long").as_bytes(),
                        )
                        .await;
                    return;
                }
                Err(ReadError::Io(e)) => {
                    log.error(format!("Gearman read error from {}: {}", self.peer_addr, e));
                    return;
                }
            };
            first = false;

            let keep_going = match message {
                Message::Admin(line) => self.admin(&line, writer).await,
                Message::Packet(header, data) => {
                    self.app_state
                        .update_connection_stats(
                            self.server_id,
                            self.connection_id,
                            Some((wire::HEADER_LEN + data.len()) as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;
                    self.packet(&header, &data, &mut exceptions, writer).await
                }
            };
            if !keep_going {
                return;
            }
        }
    }

    /// Answer one admin line. `false` closes the connection.
    async fn admin(&self, line: &str, writer: &Writer) -> bool {
        let log = Log::new(Some(&self.status_tx));
        if line.trim().is_empty() {
            return true;
        }
        let reply = match wire::parse_admin(line) {
            wire::AdminCommand::Status => wire::admin_status(&self.jobs.by_function()),
            wire::AdminCommand::Workers => ".\n".to_string(),
            wire::AdminCommand::Version => {
                format!("OK netget-{}\n", env!("CARGO_PKG_VERSION"))
            }
            wire::AdminCommand::MaxQueue | wire::AdminCommand::Shutdown => {
                log.warn(format!(
                    "Gearman admin {:?} from {} refused decision=fail_closed_admin_write",
                    line.trim(),
                    self.peer_addr
                ));
                wire::admin_error("NOT_SUPPORTED", "This server does not change its state")
            }
            wire::AdminCommand::Unknown => {
                wire::admin_error("UNKNOWN_COMMAND", "Unknown server command")
            }
        };
        self.write(writer, reply.as_bytes()).await
    }

    /// Answer one binary packet. `false` closes the connection.
    async fn packet(
        &self,
        header: &wire::Header,
        data: &[u8],
        exceptions: &mut bool,
        writer: &Writer,
    ) -> bool {
        let log = Log::new(Some(&self.status_tx));
        let request = match wire::parse_request(header, data) {
            Ok(r) => r,
            Err(e) => {
                log.warn(format!(
                    "Gearman packet type {} from {}: {:?} decision=fail_closed_bad_arguments",
                    header.packet_type, self.peer_addr, e
                ));
                return self
                    .error(writer, "invalid_arguments", "malformed packet arguments")
                    .await;
            }
        };
        match request {
            wire::Request::Echo(data) => {
                self.write(writer, &wire::response(wire::ECHO_RES, &[&data]))
                    .await
            }
            wire::Request::Option(name) if name == b"exceptions" => {
                *exceptions = true;
                self.write(writer, &wire::response(wire::OPTION_RES, &[&name]))
                    .await
            }
            wire::Request::Option(_) => {
                self.error(writer, "unknown_option", "the only option is exceptions")
                    .await
            }
            wire::Request::SetClientId => true,
            wire::Request::GetStatus(handle) => {
                let reply = match self.jobs.get(&handle) {
                    Some(job) => {
                        wire::status_res(&handle, true, true, job.numerator, job.denominator)
                    }
                    None => wire::status_res(&handle, false, false, 0, 0),
                };
                self.write(writer, &reply).await
            }
            wire::Request::Worker(t) => {
                log.warn(format!(
                    "Gearman worker packet {} from {} refused decision=fail_closed_worker",
                    t, self.peer_addr
                ));
                self.error(
                    writer,
                    "not_supported",
                    "this server runs jobs itself; worker connections are not accepted",
                )
                .await;
                false
            }
            wire::Request::Unsupported(t) => {
                log.warn(format!(
                    "Gearman packet type {} from {} is not supported \
                     decision=fail_closed_unsupported",
                    t, self.peer_addr
                ));
                self.error(writer, "not_supported", "packet type not supported")
                    .await
            }
            wire::Request::Submit {
                function,
                unique,
                workload,
                priority,
                background,
            } => {
                let handle = self.jobs.start(&function);
                if !self.write(writer, &wire::job_created(&handle)).await {
                    self.jobs.finish(&handle);
                    return false;
                }
                let event = Event::new(
                    &actions::GEARMAN_JOB_SUBMITTED_EVENT,
                    serde_json::json!({
                        "function": function,
                        "unique_id": unique,
                        "workload": String::from_utf8_lossy(&workload),
                        "workload_bytes": workload.len(),
                        "priority": priority.name(),
                        "background": background,
                        "job_handle": String::from_utf8_lossy(&handle),
                    }),
                );
                let (packets, close) = self.run_job(&event, &handle, background, *exceptions).await;
                let mut ok = true;
                if !background {
                    for packet in &packets {
                        if let Some((wire::WORK_STATUS, args)) = wire::read_response(packet) {
                            let n = String::from_utf8_lossy(&args[1]).parse().unwrap_or(0);
                            let d = String::from_utf8_lossy(&args[2]).parse().unwrap_or(0);
                            if let Some(job) =
                                self.jobs.table.lock().unwrap().get_mut(handle.as_slice())
                            {
                                job.numerator = n;
                                job.denominator = d;
                            }
                        }
                        if !self.write(writer, packet).await {
                            ok = false;
                            break;
                        }
                    }
                }
                self.jobs.finish(&handle);
                ok && !close
            }
        }
    }

    /// Ask the model to run a job; returns the packets to write (empty for a background job)
    /// and whether to close afterwards.
    async fn run_job(
        &self,
        event: &Event,
        handle: &[u8],
        background: bool,
        exceptions: bool,
    ) -> (Vec<Vec<u8>>, bool) {
        let log = Log::new(Some(&self.status_tx));
        let shown = String::from_utf8_lossy(handle).into_owned();
        let result = match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let (category, text) = match crate::utils::WireFailure::classify(&e) {
                    crate::utils::WireFailure::Overloaded => ("overloaded", EXCEPTION_OVERLOADED),
                    crate::utils::WireFailure::Unavailable => {
                        ("unavailable", EXCEPTION_UNAVAILABLE)
                    }
                };
                log.warn(format!(
                    "Gearman job {} from {} decision=fail_closed_llm_error category={}",
                    shown, self.peer_addr, category
                ));
                log.debug(format!("Gearman LLM call failed: {}", e));
                let packet = if exceptions {
                    wire::work_exception(handle, text)
                } else {
                    wire::work_fail(handle)
                };
                return (if background { vec![] } else { vec![packet] }, false);
            }
        };
        for message in &result.messages {
            log.info(message);
        }

        let mut items = Vec::new();
        let mut close = false;
        let mut stack = result.protocol_results;
        stack.reverse();
        while let Some(item) = stack.pop() {
            match item {
                ActionResult::CloseConnection => close = true,
                ActionResult::Multiple(inner) => stack.extend(inner.into_iter().rev()),
                other => items.push(other),
            }
        }

        let mut packets: Vec<Vec<u8>> = Vec::new();
        let mut outcome: Option<u32> = None;
        let mut mismatch = None;
        for item in items {
            if outcome.is_some() {
                log.warn(format!(
                    "Gearman job {shown}: the model answered after the job's outcome; dropped"
                ));
                break;
            }
            let packet = match item {
                ActionResult::Output(bytes) => match wire::read_response(&bytes) {
                    Some((wire::ERROR, _)) => bytes,
                    Some((
                        wire::WORK_STATUS
                        | wire::WORK_DATA
                        | wire::WORK_COMPLETE
                        | wire::WORK_FAIL
                        | wire::WORK_EXCEPTION,
                        args,
                    )) if args[0] == handle => bytes,
                    other => {
                        mismatch = Some(format!("{:?}", other.map(|(t, _)| t)));
                        break;
                    }
                },
                ActionResult::Custom { name, data } if name == actions::WORK_RESULT => {
                    match Work::from_json(&data) {
                        Some(work) => work.render(handle),
                        None => {
                            mismatch = Some("unreadable work result".to_string());
                            break;
                        }
                    }
                }
                other => {
                    mismatch = Some(format!("{other:?}"));
                    break;
                }
            };
            let t = wire::read_response(&packet).map(|(t, _)| t).unwrap_or(0);
            let packet = if t == wire::WORK_EXCEPTION && !exceptions {
                // The client did not ask for exceptions: gearmand sends it WORK_FAIL.
                wire::work_fail(handle)
            } else {
                packet
            };
            if matches!(
                t,
                wire::WORK_COMPLETE | wire::WORK_FAIL | wire::WORK_EXCEPTION | wire::ERROR
            ) {
                outcome = Some(t);
            }
            packets.push(packet);
        }

        let decision = if let Some(what) = mismatch {
            log.warn(format!(
                "Gearman job {shown} from {} decision=fail_closed_mismatched_reply ({what}); \
                 answering WORK_FAIL",
                self.peer_addr
            ));
            packets = vec![wire::work_fail(handle)];
            None
        } else {
            match outcome {
                Some(wire::WORK_COMPLETE) => Some("model_answer"),
                Some(_) => Some("model_reject"),
                None if packets.is_empty() && !close => {
                    log.warn(format!(
                        "Gearman job {shown} from {} decision=model_silent ({} failed \
                         action(s)); answering WORK_FAIL",
                        self.peer_addr,
                        result.failures.len()
                    ));
                    packets.push(wire::work_fail(handle));
                    None
                }
                None if close => Some("model_close"),
                None => {
                    log.warn(format!(
                        "Gearman job {shown} from {} decision=fail_closed_unfinished: \
                         progress without an outcome; answering WORK_FAIL",
                        self.peer_addr
                    ));
                    packets.push(wire::work_fail(handle));
                    None
                }
            }
        };
        if let Some(decision) = decision {
            log.info(format!(
                "Gearman job {shown} from {} decision={} background={}",
                self.peer_addr, decision, background
            ));
        }
        (if background { Vec::new() } else { packets }, close)
    }
}

/// Keep reading briefly after the last reply and the half-close, so the close is a FIN rather
/// than an RST that could destroy that reply. Bounded both ways.
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

/// Reads packets and admin lines, keeping anything past what was asked for.
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
            chunk: vec![0u8; 8192],
        }
    }

    async fn fill(&mut self, timeout: Duration) -> Result<(), ReadError> {
        match tokio::time::timeout(timeout, self.reader.read(&mut self.chunk)).await {
            Err(_) => Err(ReadError::TimedOut),
            Ok(Ok(0)) => Err(ReadError::Eof),
            Ok(Ok(n)) => {
                self.pending.extend_from_slice(&self.chunk[..n]);
                Ok(())
            }
            Ok(Err(e)) => Err(ReadError::Io(e)),
        }
    }

    /// `first` bounds the wait for the message's first byte; `idle` every wait after it.
    async fn next_message(
        &mut self,
        first: Duration,
        idle: Duration,
    ) -> Result<Message, ReadError> {
        if self.pending.is_empty() {
            self.fill(first).await?;
        }
        if self.pending[0] == 0 {
            while self.pending.len() < wire::HEADER_LEN {
                self.fill(idle).await?;
            }
            let header = match wire::parse_header(&self.pending[..wire::HEADER_LEN]) {
                Ok(h) => h,
                Err(wire::HeaderError::TooLarge(size)) => return Err(ReadError::TooLarge(size)),
                Err(wire::HeaderError::BadMagic) => return Err(ReadError::BadMagic),
            };
            // Bounded by parse_header: at most MAX_PACKET_BYTES.
            let total = wire::HEADER_LEN + header.size as usize;
            while self.pending.len() < total {
                self.fill(idle).await?;
            }
            let packet: Vec<u8> = self.pending.drain(..total).collect();
            return Ok(Message::Packet(header, packet[wire::HEADER_LEN..].to_vec()));
        }
        loop {
            if let Some(at) = self.pending.iter().position(|b| *b == b'\n') {
                if at + 1 > wire::MAX_ADMIN_LINE {
                    return Err(ReadError::LineTooLong);
                }
                let line: Vec<u8> = self.pending.drain(..=at).collect();
                let text = String::from_utf8_lossy(&line[..at]);
                return Ok(Message::Admin(text.trim_end_matches('\r').to_string()));
            }
            if self.pending.len() >= wire::MAX_ADMIN_LINE {
                return Err(ReadError::LineTooLong);
            }
            self.fill(idle).await?;
        }
    }
}
