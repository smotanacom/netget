//! NSQ broker (nsqd's TCP protocol, V2) — the model decides what is accepted and delivered.
//!
//! A client sends the `"  V2"` magic, then commands. NetGet answers IDENTIFY, NOP, CLS, TOUCH,
//! AUTH, heartbeats and every malformed or out-of-state command itself; PUB/MPUB/DPUB raise
//! `nsq_publish` and SUB raises `nsq_subscribe`, which the model accepts or refuses; RDY, FIN
//! and REQ raise `nsq_ready`, `nsq_finish` and `nsq_requeue`, which the model answers with the
//! messages the subscriber should receive, or with nothing.
//!
//! Six properties worth knowing before changing the loop:
//!
//! 1. **Sizes are judged before anything is allocated.** Every body's declared size, and an
//!    MPUB's declared message count, is checked against nsqd's defaults by
//!    [`wire::parse_command`] from the size field alone; a command line is capped at
//!    [`wire::MAX_LINE`]. An overrun gets nsqd's own error and the connection closes.
//! 2. **The model cannot write a frame, and cannot over-deliver.** Its answers are structured;
//!    NetGet assigns each message its id and timestamp and sends no more than the client's RDY
//!    count minus the messages in flight. The rest wait, in order, in a bounded per-connection
//!    queue until a FIN or REQ makes room.
//! 3. **Heartbeats run while the model thinks.** `_heartbeat_` is written on the negotiated
//!    interval whether the loop is waiting for a command or for an answer, because a go-nsq
//!    client times out its reads at twice the interval.
//! 4. **Silence is measured between commands, not during answers.** A client silent for two
//!    heartbeat intervals is closed (nsqd's rule); one that disabled heartbeats is held to
//!    `idle_timeout_secs`. The clock restarts after each answer, so a publish parked for a human
//!    is never closed for the time the human took.
//! 5. **NetGet is not a queue.** Nothing published is stored or routed to other connections;
//!    the model decides what each subscriber receives. NetGet keeps only the connection's flow
//!    state: its subscription, RDY count, the messages in flight (for FIN/REQ/TOUCH) and the
//!    queue of messages the model already chose.
//! 6. **Every close lingers**, so an error frame written before closing over unread input is not
//!    destroyed by an RST.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::executor::ExecutionResult;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use actions::{Answer, Delivery};
use anyhow::Result;
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// How long a new connection may send nothing, not even the magic. nsq clients send it at
/// once; 300 s is the window a `manual` rule gives a human, for a NetGet TCP client parked on
/// its operator. Lower it through `first_byte_timeout_secs` for a public listener.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(300);

/// nsqd's default heartbeat interval (half its 60 s client timeout).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How long a client that disabled heartbeats may stay silent between commands.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Concurrent connections admitted before new ones are refused — the house default.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// The most messages the model may have waiting for RDY capacity on one connection, and their
/// total size. Past either, further deliveries are dropped and logged.
pub const MAX_PENDING_MESSAGES: usize = 1000;
pub const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// How much of a publish the model is shown: the first 20 bodies, each cut at 1000 bytes.
const EVENT_MESSAGES: usize = 20;
const EVENT_BODY_BYTES: usize = 1000;

/// The fixed text a backend failure is answered with, by command and category. No error text
/// reaches the peer.
fn failed_text(command: &str, overloaded: bool) -> &'static str {
    match (command, overloaded) {
        ("MPUB", false) => "MPUB failed: broker backend unavailable",
        ("MPUB", true) => "MPUB failed: broker backend at capacity",
        ("DPUB", false) => "DPUB failed: broker backend unavailable",
        ("DPUB", true) => "DPUB failed: broker backend at capacity",
        ("SUB", false) => "SUB failed: broker backend unavailable",
        ("SUB", true) => "SUB failed: broker backend at capacity",
        (_, false) => "PUB failed: broker backend unavailable",
        (_, true) => "PUB failed: broker backend at capacity",
    }
}

/// The server's bounds, from the startup parameters.
#[derive(Debug, Clone, Copy)]
pub struct NsqConfig {
    pub first_byte_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub idle_timeout: Duration,
}

pub struct NsqServer;

impl NsqServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        config: NsqConfig,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("NSQ broker listening on {}", local_addr));

        let protocol = Arc::new(actions::NsqProtocol::new());
        let ids = Arc::new(AtomicU64::new(0));
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                // A peer over the cap gets no bytes: nsqd has no refusal frame for it, and a
                // frame the client did not ask for would be read as an answer.
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    b"",
                    "NSQ",
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
                            .info(format!("NSQ client connected from {}", peer_addr));

                        let session = Session {
                            peer_addr,
                            llm_client: llm_client.clone(),
                            app_state: app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id,
                            protocol: protocol.clone(),
                            connection_id,
                            config,
                            ids: ids.clone(),
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
                        Log::new(Some(&status_tx)).error(format!("NSQ accept error: {}", e));
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

type Writer = WriteHalf<TcpStream>;

struct Session {
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::NsqProtocol>,
    connection_id: ConnectionId,
    config: NsqConfig,
    ids: Arc<AtomicU64>,
}

/// One connection's flow state. Nothing here outlives the connection.
struct Flow {
    heartbeat: Option<Duration>,
    ticker: Option<tokio::time::Interval>,
    identified: bool,
    client_id: String,
    user_agent: String,
    sub: Option<(String, String)>,
    closing: bool,
    rdy: u64,
    /// Message id → (body, attempts), for FIN/REQ/TOUCH and a requeue event's body.
    in_flight: BTreeMap<String, (String, u16)>,
    pending: VecDeque<Delivery>,
    pending_bytes: usize,
}

fn ticker(every: Duration) -> tokio::time::Interval {
    let mut t = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    t
}

impl Flow {
    fn new(heartbeat: Duration) -> Self {
        Self {
            heartbeat: Some(heartbeat),
            ticker: Some(ticker(heartbeat)),
            identified: false,
            client_id: String::new(),
            user_agent: String::new(),
            sub: None,
            closing: false,
            rdy: 0,
            in_flight: BTreeMap::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
        }
    }

    /// How long the client may stay silent: two heartbeats, or the idle bound without them.
    fn silence_window(&self, idle: Duration) -> Duration {
        match self.heartbeat {
            Some(hb) => hb * 2,
            None => idle,
        }
    }

    fn can_deliver(&self) -> u64 {
        if self.sub.is_none() || self.closing {
            return 0;
        }
        self.rdy.saturating_sub(self.in_flight.len() as u64)
    }

    fn topic(&self) -> (String, String) {
        self.sub.clone().unwrap_or_default()
    }
}

/// Wait for the next heartbeat tick, or forever when heartbeats are off.
async fn tick(ticker: &mut Option<tokio::time::Interval>) {
    match ticker {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Wait for the next injected command, or forever once the channel has closed.
async fn next_injected(rx: &mut Option<mpsc::Receiver<ClientCommand>>) -> Option<ClientCommand> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

enum ReadError {
    Eof,
    TimedOut,
    Wire(wire::WireError),
    Io(std::io::Error),
}

enum Step {
    Continue,
    Close,
}

enum Wake {
    Command(Result<(wire::Command, usize), ReadError>),
    Heartbeat,
    Injected(Option<ClientCommand>),
}

impl Session {
    async fn run(self, socket: TcpStream) {
        let (mut reader, mut writer) = tokio::io::split(socket);
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &self.app_state,
            self.server_id,
            self.connection_id.as_u32(),
        )
        .await;

        {
            let mut framer = Framer::new(&mut reader);
            self.session(&mut framer, &mut writer, peer_rx).await;
        }

        self.app_state
            .remove_peer_handle(self.server_id, self.connection_id.as_u32())
            .await;
        let _ = writer.shutdown().await;
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

    async fn write(&self, writer: &mut Writer, data: &[u8]) -> bool {
        let ok = writer.write_all(data).await.is_ok() && writer.flush().await.is_ok();
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

    /// Write an error frame; `Close` when nsqd would close after it.
    async fn refuse(&self, writer: &mut Writer, code: &'static str, message: &str) -> Step {
        let written = self.write(writer, &wire::error_frame(code, message)).await;
        if written && !wire::is_fatal(code) {
            Step::Continue
        } else {
            Step::Close
        }
    }

    async fn session<R: AsyncRead + Unpin>(
        &self,
        framer: &mut Framer<'_, R>,
        writer: &mut Writer,
        peer_rx: mpsc::Receiver<ClientCommand>,
    ) {
        let log = Log::new(Some(&self.status_tx));
        let magic_deadline = tokio::time::Instant::now() + self.config.first_byte_timeout;
        match framer.magic(magic_deadline).await {
            Ok(true) => {}
            Ok(false) => {
                log.warn(format!(
                    "NSQ peer {} did not send the V2 magic decision=fail_closed_bad_magic",
                    self.peer_addr
                ));
                let _ = self
                    .write(writer, &wire::error_frame("E_BAD_PROTOCOL", ""))
                    .await;
                return;
            }
            Err(ReadError::TimedOut) => {
                log.info(format!(
                    "NSQ client {} sent nothing within {}s; closing",
                    self.peer_addr,
                    self.config.first_byte_timeout.as_secs()
                ));
                return;
            }
            Err(_) => return,
        }

        let mut flow = Flow::new(self.config.heartbeat_interval);
        let mut peer_rx = Some(peer_rx);
        let mut answered_at = tokio::time::Instant::now();
        let heartbeat = wire::response_frame(wire::HEARTBEAT);

        loop {
            let window = flow.silence_window(self.config.idle_timeout);
            let deadline = framer.last_read.max(answered_at) + window;
            let wake = tokio::select! {
                r = framer.next_command(deadline) => Wake::Command(r),
                _ = tick(&mut flow.ticker) => Wake::Heartbeat,
                c = next_injected(&mut peer_rx) => Wake::Injected(c),
            };
            let step = match wake {
                Wake::Heartbeat => {
                    if self.write(writer, &heartbeat).await {
                        Step::Continue
                    } else {
                        Step::Close
                    }
                }
                Wake::Injected(None) => {
                    peer_rx = None;
                    Step::Continue
                }
                Wake::Injected(Some(command)) => self.inject(command, &mut flow, writer).await,
                Wake::Command(Err(ReadError::Eof)) => {
                    log.info(format!("NSQ client {} disconnected", self.peer_addr));
                    Step::Close
                }
                Wake::Command(Err(ReadError::TimedOut)) => {
                    log.info(format!(
                        "NSQ client {} silent for {}s decision=fail_closed_idle; closing",
                        self.peer_addr,
                        window.as_secs()
                    ));
                    Step::Close
                }
                Wake::Command(Err(ReadError::Io(e))) => {
                    log.error(format!("NSQ read error from {}: {}", self.peer_addr, e));
                    Step::Close
                }
                Wake::Command(Err(ReadError::Wire(e))) => {
                    log.warn(format!(
                        "NSQ command from {} refused: {} {} decision={}",
                        self.peer_addr,
                        e.code,
                        e.message,
                        e.decision()
                    ));
                    let _ = self.write(writer, &e.to_frame()).await;
                    Step::Close
                }
                Wake::Command(Ok((command, used))) => {
                    self.app_state
                        .update_connection_stats(
                            self.server_id,
                            self.connection_id,
                            Some(used as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;
                    let step = self.command(command, &mut flow, writer).await;
                    answered_at = tokio::time::Instant::now();
                    step
                }
            };
            if let Step::Close = step {
                return;
            }
        }
    }

    /// Answer one command.
    async fn command(&self, command: wire::Command, flow: &mut Flow, writer: &mut Writer) -> Step {
        use wire::Command as C;
        let log = Log::new(Some(&self.status_tx));
        let name = command.name();
        let subscribed_or_closing = flow.sub.is_some() || flow.closing;
        match command {
            C::Nop => Step::Continue,
            C::Identify(body) => {
                if flow.identified {
                    return self
                        .refuse(writer, "E_INVALID", "cannot IDENTIFY in current state")
                        .await;
                }
                let identify = match wire::parse_identify(&body) {
                    Ok(i) => i,
                    Err(e) => {
                        log.warn(format!(
                            "NSQ IDENTIFY from {} refused: {} decision=fail_closed_bad_identify",
                            self.peer_addr, e.message
                        ));
                        let _ = self.write(writer, &e.to_frame()).await;
                        return Step::Close;
                    }
                };
                flow.identified = true;
                match identify.heartbeat_ms {
                    None => {
                        flow.heartbeat = None;
                        flow.ticker = None;
                    }
                    Some(0) => {}
                    Some(ms) => {
                        let every = Duration::from_millis(ms as u64);
                        flow.heartbeat = Some(every);
                        flow.ticker = Some(ticker(every));
                    }
                }
                flow.client_id = identify.client_id;
                flow.user_agent = identify.user_agent;
                let reply = if identify.feature_negotiation {
                    wire::identify_response(identify.msg_timeout_ms)
                } else {
                    b"OK".to_vec()
                };
                log.debug(format!(
                    "NSQ IDENTIFY from {} (user_agent {:?}, heartbeat {:?})",
                    self.peer_addr, flow.user_agent, flow.heartbeat
                ));
                self.step(self.write(writer, &wire::response_frame(&reply)).await)
            }
            C::Auth(_) => {
                log.warn(format!(
                    "NSQ AUTH from {} refused decision=fail_closed_auth_disabled",
                    self.peer_addr
                ));
                self.refuse(writer, "E_AUTH_DISABLED", "AUTH disabled")
                    .await
            }
            C::Cls => {
                if flow.sub.is_none() {
                    return self
                        .refuse(writer, "E_INVALID", "cannot CLS in current state")
                        .await;
                }
                flow.closing = true;
                flow.pending.clear();
                flow.pending_bytes = 0;
                self.step(
                    self.write(writer, &wire::response_frame(b"CLOSE_WAIT"))
                        .await,
                )
            }
            C::Touch(id) => {
                if !subscribed_or_closing {
                    return self
                        .refuse(writer, "E_INVALID", "cannot TOUCH in current state")
                        .await;
                }
                if flow.in_flight.contains_key(&id) {
                    Step::Continue
                } else {
                    self.refuse(
                        writer,
                        "E_TOUCH_FAILED",
                        &format!("TOUCH {id} failed ID not in flight"),
                    )
                    .await
                }
            }
            C::Sub { topic, channel } => {
                if flow.sub.is_some() || flow.closing {
                    return self
                        .refuse(writer, "E_INVALID", "cannot SUB in current state")
                        .await;
                }
                self.subscribe(topic, channel, flow, writer).await
            }
            C::Rdy(count) => {
                if flow.closing {
                    return Step::Continue;
                }
                if flow.sub.is_none() {
                    return self
                        .refuse(writer, "E_INVALID", "cannot RDY in current state")
                        .await;
                }
                flow.rdy = count;
                if !self.flush(flow, writer).await {
                    return Step::Close;
                }
                if flow.can_deliver() == 0 {
                    return Step::Continue;
                }
                let (topic, _) = flow.topic();
                let mut data = self.flow_data(flow);
                data["count"] = json!(count);
                data["answer_with"] = json!(actions::deliver_answer_with(
                    &topic,
                    flow.can_deliver(),
                    flow.pending.len()
                ));
                self.flow_event(&actions::NSQ_READY_EVENT, data, name, flow, writer)
                    .await
            }
            C::Fin(id) => {
                if !subscribed_or_closing {
                    return self
                        .refuse(writer, "E_INVALID", "cannot FIN in current state")
                        .await;
                }
                if flow.in_flight.remove(&id).is_none() {
                    return self
                        .refuse(
                            writer,
                            "E_FIN_FAILED",
                            &format!("FIN {id} failed ID not in flight"),
                        )
                        .await;
                }
                if !self.flush(flow, writer).await {
                    return Step::Close;
                }
                if flow.closing {
                    return Step::Continue;
                }
                let (topic, _) = flow.topic();
                let mut data = self.flow_data(flow);
                data["message_id"] = json!(id);
                data["answer_with"] = json!(actions::deliver_answer_with(
                    &topic,
                    flow.can_deliver(),
                    flow.pending.len()
                ));
                self.flow_event(&actions::NSQ_FINISH_EVENT, data, name, flow, writer)
                    .await
            }
            C::Req { id, timeout_ms } => {
                if !subscribed_or_closing {
                    return self
                        .refuse(writer, "E_INVALID", "cannot REQ in current state")
                        .await;
                }
                let Some((body, attempts)) = flow.in_flight.remove(&id) else {
                    return self
                        .refuse(
                            writer,
                            "E_REQ_FAILED",
                            &format!("REQ {id} failed ID not in flight"),
                        )
                        .await;
                };
                if !self.flush(flow, writer).await {
                    return Step::Close;
                }
                if flow.closing {
                    return Step::Continue;
                }
                let mut data = self.flow_data(flow);
                data["message_id"] = json!(id);
                data["timeout_ms"] = json!(timeout_ms);
                data["attempts"] = json!(attempts);
                data["body"] = json!(crate::utils::truncate_for_llm(&body, EVENT_BODY_BYTES));
                data["answer_with"] = json!(actions::requeue_answer_with(attempts));
                self.flow_event(&actions::NSQ_REQUEUE_EVENT, data, name, flow, writer)
                    .await
            }
            C::Pub { topic, body } => {
                self.publish(name, topic, vec![body], None, flow, writer)
                    .await
            }
            C::Dpub {
                topic,
                defer_ms,
                body,
            } => {
                self.publish(name, topic, vec![body], Some(defer_ms), flow, writer)
                    .await
            }
            C::Mpub { topic, messages } => {
                self.publish(name, topic, messages, None, flow, writer)
                    .await
            }
        }
    }

    fn step(&self, written: bool) -> Step {
        if written {
            Step::Continue
        } else {
            Step::Close
        }
    }

    fn flow_data(&self, flow: &Flow) -> serde_json::Value {
        let (topic, channel) = flow.topic();
        json!({
            "topic": topic,
            "channel": channel,
            "ready": flow.rdy,
            "in_flight": flow.in_flight.len(),
            "pending": flow.pending.len(),
            "can_deliver": flow.can_deliver(),
        })
    }

    /// Ask the model, writing heartbeats while it thinks.
    async fn ask(
        &self,
        event: &Event,
        flow: &mut Flow,
        writer: &mut Writer,
    ) -> Result<ExecutionResult> {
        let heartbeat = wire::response_frame(wire::HEARTBEAT);
        let call = call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            event,
            self.protocol.as_ref(),
        );
        tokio::pin!(call);
        loop {
            tokio::select! {
                result = &mut call => return result,
                _ = tick(&mut flow.ticker) => {
                    // A failed write is found again by the answer's own write.
                    let _ = self.write(writer, &heartbeat).await;
                }
            }
        }
    }

    /// Flatten the model's results into answers, in order.
    fn answers(&self, result: ExecutionResult) -> Vec<Answer> {
        let log = Log::new(Some(&self.status_tx));
        for message in &result.messages {
            log.info(message);
        }
        let mut out = Vec::new();
        let mut stack = result.protocol_results;
        stack.reverse();
        while let Some(item) = stack.pop() {
            match item {
                ActionResult::Multiple(inner) => stack.extend(inner.into_iter().rev()),
                other => match Answer::from_result(&other) {
                    Some(answer) => out.push(answer),
                    None => log.debug(format!("NSQ: ignoring non-NSQ result {other:?}")),
                },
            }
        }
        out
    }

    async fn subscribe(
        &self,
        topic: String,
        channel: String,
        flow: &mut Flow,
        writer: &mut Writer,
    ) -> Step {
        let log = Log::new(Some(&self.status_tx));
        let event = Event::new(
            &actions::NSQ_SUBSCRIBE_EVENT,
            json!({
                "topic": topic,
                "channel": channel,
                "ephemeral": wire::is_ephemeral(&channel),
                "client_id": flow.client_id,
                "user_agent": flow.user_agent,
                "answer_with": actions::subscribe_answer_with(&topic, &channel),
            }),
        );
        let answers = match self.ask(&event, flow, writer).await {
            Ok(result) => self.answers(result),
            Err(e) => {
                let failure = crate::utils::WireFailure::classify(&e);
                log.warn(format!(
                    "NSQ SUB {topic} {channel} from {} decision=fail_closed_llm_error category={:?}",
                    self.peer_addr, failure
                ));
                log.debug(format!("NSQ LLM call failed: {}", e));
                let text = failed_text("SUB", failure.is_overloaded());
                return self.refuse(writer, "E_INVALID", text).await;
            }
        };
        let step = match reply_of(&answers) {
            Some(Answer::Ok) => {
                log.info(format!(
                    "NSQ SUB {topic} {channel} from {} decision=model_answer",
                    self.peer_addr
                ));
                flow.sub = Some((topic, channel));
                self.step(self.write(writer, &wire::response_frame(b"OK")).await)
            }
            Some(Answer::Error { code, message }) => {
                log.info(format!(
                    "NSQ SUB {topic} {channel} from {} decision=model_reject code={code}",
                    self.peer_addr
                ));
                self.refuse(writer, static_code(code), message).await
            }
            _ => {
                log.warn(format!(
                    "NSQ SUB {topic} {channel} from {} decision=model_silent; refusing",
                    self.peer_addr
                ));
                return self.refuse(writer, "E_INVALID", "SUB failed").await;
            }
        };
        self.finish_answers(step, &answers, flow, writer).await
    }

    async fn publish(
        &self,
        command: &'static str,
        topic: String,
        bodies: Vec<Vec<u8>>,
        defer_ms: Option<i64>,
        flow: &mut Flow,
        writer: &mut Writer,
    ) -> Step {
        let log = Log::new(Some(&self.status_tx));
        let failed_code: &'static str = match command {
            "MPUB" => "E_MPUB_FAILED",
            "DPUB" => "E_DPUB_FAILED",
            _ => "E_PUB_FAILED",
        };
        let total_bytes: usize = bodies.iter().map(Vec::len).sum();
        let shown: Vec<String> = bodies
            .iter()
            .take(EVENT_MESSAGES)
            .map(|b| crate::utils::truncate_for_llm(&String::from_utf8_lossy(b), EVENT_BODY_BYTES))
            .collect();
        let mut data = json!({
            "command": command,
            "topic": topic,
            "messages": shown,
            "message_count": bodies.len(),
            "total_bytes": total_bytes,
            "answer_with": actions::publish_answer_with(command, &topic, bodies.len()),
        });
        if let Some(defer) = defer_ms {
            data["defer_ms"] = json!(defer);
        }
        let event = Event::new(&actions::NSQ_PUBLISH_EVENT, data);
        let answers = match self.ask(&event, flow, writer).await {
            Ok(result) => self.answers(result),
            Err(e) => {
                let failure = crate::utils::WireFailure::classify(&e);
                log.warn(format!(
                    "NSQ {command} {topic} from {} decision=fail_closed_llm_error category={:?}",
                    self.peer_addr, failure
                ));
                log.debug(format!("NSQ LLM call failed: {}", e));
                let text = failed_text(command, failure.is_overloaded());
                return self.refuse(writer, failed_code, text).await;
            }
        };
        let step = match reply_of(&answers) {
            Some(Answer::Ok) => {
                log.info(format!(
                    "NSQ {command} {topic} ({} message(s)) from {} decision=model_answer",
                    bodies.len(),
                    self.peer_addr
                ));
                self.step(self.write(writer, &wire::response_frame(b"OK")).await)
            }
            Some(Answer::Error { code, message }) => {
                log.info(format!(
                    "NSQ {command} {topic} from {} decision=model_reject code={code}",
                    self.peer_addr
                ));
                self.refuse(writer, static_code(code), message).await
            }
            _ => {
                log.warn(format!(
                    "NSQ {command} {topic} from {} decision=model_silent; refusing",
                    self.peer_addr
                ));
                return self
                    .refuse(writer, failed_code, &format!("{command} failed"))
                    .await;
            }
        };
        self.finish_answers(step, &answers, flow, writer).await
    }

    /// After a command's one reply: queue any deliveries and honour a close.
    async fn finish_answers(
        &self,
        step: Step,
        answers: &[Answer],
        flow: &mut Flow,
        writer: &mut Writer,
    ) -> Step {
        if let Step::Close = step {
            return Step::Close;
        }
        let deliveries: Vec<Delivery> = answers
            .iter()
            .filter_map(|a| match a {
                Answer::Deliver(d) => Some(d.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        if !deliveries.is_empty() {
            self.enqueue(flow, deliveries);
            if !self.flush(flow, writer).await {
                return Step::Close;
            }
        }
        if answers.iter().any(|a| matches!(a, Answer::Close)) {
            return Step::Close;
        }
        Step::Continue
    }

    /// RDY, FIN and REQ: the model delivers messages, refuses, or says nothing. None of the
    /// three takes a reply in nsqd, so nothing is written unless the model delivers or refuses.
    async fn flow_event(
        &self,
        event_type: &'static EventType,
        data: serde_json::Value,
        command: &'static str,
        flow: &mut Flow,
        writer: &mut Writer,
    ) -> Step {
        let log = Log::new(Some(&self.status_tx));
        let event = Event::new(event_type, data);
        let answers = match self.ask(&event, flow, writer).await {
            Ok(result) => self.answers(result),
            Err(e) => {
                log.warn(format!(
                    "NSQ {command} from {} decision=fail_closed_llm_error category={:?}; \
                     delivering nothing",
                    self.peer_addr,
                    crate::utils::WireFailure::classify(&e)
                ));
                log.debug(format!("NSQ LLM call failed: {}", e));
                return Step::Continue;
            }
        };
        if let Some(Answer::Error { code, message }) =
            answers.iter().find(|a| matches!(a, Answer::Error { .. }))
        {
            log.info(format!(
                "NSQ {command} from {} decision=model_reject code={code}",
                self.peer_addr
            ));
            if let Step::Close = self.refuse(writer, static_code(code), message).await {
                return Step::Close;
            }
        }
        let offered: usize = answers
            .iter()
            .map(|a| match a {
                Answer::Deliver(d) => d.len(),
                _ => 0,
            })
            .sum();
        let step = self
            .finish_answers(Step::Continue, &answers, flow, writer)
            .await;
        if offered > 0 {
            log.info(format!(
                "NSQ {command} from {} decision=model_answer offered={offered} in_flight={} \
                 pending={}",
                self.peer_addr,
                flow.in_flight.len(),
                flow.pending.len()
            ));
        } else if !answers.iter().any(|a| matches!(a, Answer::Error { .. })) {
            log.info(format!(
                "NSQ {command} from {} decision=model_silent (nothing delivered)",
                self.peer_addr
            ));
        }
        step
    }

    /// Queue the model's deliveries behind any already waiting, within the pending bounds.
    fn enqueue(&self, flow: &mut Flow, deliveries: Vec<Delivery>) {
        let log = Log::new(Some(&self.status_tx));
        if flow.sub.is_none() || flow.closing {
            log.warn(format!(
                "NSQ: {} message(s) for {} dropped - the connection is not subscribed{} \
                 decision=fail_closed_not_subscribed",
                deliveries.len(),
                self.peer_addr,
                if flow.closing { " (closing)" } else { "" }
            ));
            return;
        }
        let mut dropped = 0usize;
        for delivery in deliveries {
            if flow.pending.len() >= MAX_PENDING_MESSAGES
                || flow.pending_bytes + delivery.body.len() > MAX_PENDING_BYTES
            {
                dropped += 1;
                continue;
            }
            flow.pending_bytes += delivery.body.len();
            flow.pending.push_back(delivery);
        }
        if dropped > 0 {
            log.warn(format!(
                "NSQ: {dropped} message(s) for {} dropped over the pending bound ({} messages, \
                 {} bytes) decision=fail_closed_pending_full",
                self.peer_addr, MAX_PENDING_MESSAGES, MAX_PENDING_BYTES
            ));
        }
    }

    /// Send waiting messages while the client's RDY count allows. `false` if a write failed.
    async fn flush(&self, flow: &mut Flow, writer: &mut Writer) -> bool {
        self.flush_counting(flow, writer).await.is_some()
    }

    /// [`Session::flush`], returning the bytes written; `None` if a write failed.
    async fn flush_counting(&self, flow: &mut Flow, writer: &mut Writer) -> Option<usize> {
        let mut written = 0usize;
        while flow.can_deliver() > 0 {
            let Some(delivery) = flow.pending.pop_front() else {
                break;
            };
            flow.pending_bytes -= delivery.body.len();
            let n = self.ids.fetch_add(1, Ordering::SeqCst) + 1;
            let id = wire::message_id_for(n);
            let timestamp = crate::utils::clock::SystemTime::now()
                .duration_since(crate::utils::clock::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            let frame =
                wire::message_frame(timestamp, delivery.attempts, &id, delivery.body.as_bytes());
            if !self.write(writer, &frame).await {
                return None;
            }
            written += frame.len();
            flow.in_flight.insert(
                String::from_utf8_lossy(&id).into_owned(),
                (delivery.body, delivery.attempts),
            );
        }
        Some(written)
    }

    /// An action injected from the dashboard (or `send_to_peer`), outside any event.
    async fn inject(&self, command: ClientCommand, flow: &mut Flow, writer: &mut Writer) -> Step {
        let action = command.action.clone();
        let result = crate::llm::actions::executor::execute_actions(
            vec![action.clone()],
            &self.app_state,
            Some(self.protocol.as_ref()),
            Some(self.server_id),
            None,
        )
        .await;
        let (outcome, step) = match result {
            Err(e) => (Err(e), Step::Continue),
            Ok(result) => {
                let failures = result.failure_summary();
                let answers = self.answers(result);
                if answers.is_empty() {
                    let error = failures.unwrap_or_else(|| "no NSQ action".to_string());
                    (Ok(ClientSendOutcome::Rejected { error }), Step::Continue)
                } else {
                    let mut bytes = 0usize;
                    let mut step = Step::Continue;
                    for answer in &answers {
                        let frame = match answer {
                            Answer::Ok => wire::response_frame(b"OK"),
                            Answer::Error { code, message } => {
                                let code = static_code(code);
                                if wire::is_fatal(code) {
                                    step = Step::Close;
                                }
                                wire::error_frame(code, message)
                            }
                            Answer::Close => {
                                step = Step::Close;
                                continue;
                            }
                            Answer::Deliver(d) => {
                                self.enqueue(flow, d.clone());
                                match self.flush_counting(flow, writer).await {
                                    Some(n) => bytes += n,
                                    None => step = Step::Close,
                                }
                                continue;
                            }
                        };
                        if self.write(writer, &frame).await {
                            bytes += frame.len();
                        } else {
                            step = Step::Close;
                        }
                    }
                    let outcome = match step {
                        Step::Close => ClientSendOutcome::Disconnected,
                        Step::Continue if bytes > 0 => {
                            ClientSendOutcome::Sent { bytes_sent: bytes }
                        }
                        Step::Continue => ClientSendOutcome::Executed {
                            detail: format!("queued; {} pending for RDY", flow.pending.len()),
                        },
                    };
                    (Ok(outcome), step)
                }
            }
        };
        let outcome_json = match &outcome {
            Ok(o) => serde_json::to_value(o).unwrap_or(serde_json::Value::Null),
            Err(e) => json!({"error": e.to_string()}),
        };
        self.app_state
            .record_access_log(
                crate::state::AccessLogOwner::Server(self.server_id.as_u32()),
                "NSQ",
                Some(self.connection_id.as_u32()),
                "injected_action",
                action,
                vec![outcome_json],
            )
            .await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());
        let _ = command.reply_tx.send(outcome);
        step
    }
}

/// The first reply-shaped answer (OK or an error), in the model's order.
fn reply_of(answers: &[Answer]) -> Option<&Answer> {
    answers
        .iter()
        .find(|a| matches!(a, Answer::Ok | Answer::Error { .. }))
}

/// The executor accepts only codes from [`wire::ERROR_CODES`]; map back to the static string.
fn static_code(code: &str) -> &'static str {
    wire::ERROR_CODES
        .iter()
        .copied()
        .find(|c| *c == code)
        .unwrap_or("E_INVALID")
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

/// Reads the magic, then commands, keeping anything past what was asked for. Parsing is a pure
/// function of the buffer, so dropping a `next_command` future (a heartbeat or an injected
/// action won the race) loses nothing.
struct Framer<'a, R> {
    reader: &'a mut R,
    pending: Vec<u8>,
    chunk: Vec<u8>,
    last_read: tokio::time::Instant,
}

impl<'a, R: AsyncRead + Unpin> Framer<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            chunk: vec![0u8; 16 * 1024],
            last_read: tokio::time::Instant::now(),
        }
    }

    async fn fill(&mut self, deadline: tokio::time::Instant) -> Result<(), ReadError> {
        match tokio::time::timeout_at(deadline, self.reader.read(&mut self.chunk)).await {
            Err(_) => Err(ReadError::TimedOut),
            Ok(Ok(0)) => Err(ReadError::Eof),
            Ok(Ok(n)) => {
                self.pending.extend_from_slice(&self.chunk[..n]);
                self.last_read = tokio::time::Instant::now();
                Ok(())
            }
            Ok(Err(e)) => Err(ReadError::Io(e)),
        }
    }

    /// `Ok(true)` for the V2 magic, `Ok(false)` for anything else.
    async fn magic(&mut self, deadline: tokio::time::Instant) -> Result<bool, ReadError> {
        while self.pending.len() < wire::MAGIC_V2.len() {
            self.fill(deadline).await?;
        }
        let ok = &self.pending[..4] == wire::MAGIC_V2;
        self.pending.drain(..4);
        Ok(ok)
    }

    async fn next_command(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Result<(wire::Command, usize), ReadError> {
        loop {
            match wire::parse_command(&self.pending) {
                Ok(Some((command, used))) => {
                    self.pending.drain(..used);
                    return Ok((command, used));
                }
                Ok(None) => self.fill(deadline).await?,
                Err(e) => return Err(ReadError::Wire(e)),
            }
        }
    }
}
