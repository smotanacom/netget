//! AMQP 1.0 container. Rust owns the protocol headers, SASL, the connection / session / link
//! state machines, link credit, transfers (multi-frame both ways), settlement and the relay of
//! accepted messages to receivers of the same address; the handler admits connections and
//! links, settles each message and produces messages for receivers.
pub mod actions;
pub mod frame;
pub mod message;
pub mod types;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use crate::utils::task_guard::AbortOnDrop;
use anyhow::{bail, ensure, Context, Result};
use frame::*;
use serde_json::{json, Value as Json};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use types::{field, Value};

pub const DEFAULT_CONTAINER: &str = "netget";
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CHANNEL_MAX: u16 = 15;
const HANDLE_MAX: u32 = 63;
const RECEIVE_CREDIT: u32 = 100;
const HELD_PER_LINK: usize = 1000;
const INBOX: usize = 4096;

/// A delivery for one of a connection's sending links: (channel, handle, encoded message).
type Delivery = (u16, u32, Arc<Vec<u8>>);

struct Subscriber {
    conn: ConnectionId,
    channel: u16,
    handle: u32,
    tx: mpsc::Sender<Delivery>,
}

struct Shared {
    ctx: SpawnContext,
    container: String,
    require_sasl: bool,
    idle: Duration,
    /// address → receivers attached to it
    topics: Mutex<HashMap<String, Vec<Subscriber>>>,
}

impl Shared {
    /// Hand a message to every receiver of `address`; how many it reached.
    fn relay(&self, address: &str, encoded: Arc<Vec<u8>>) -> usize {
        let mut topics = self.topics.lock().unwrap_or_else(|e| e.into_inner());
        let Some(subs) = topics.get_mut(address) else {
            return 0;
        };
        subs.retain(|s| !s.tx.is_closed());
        subs.iter()
            .filter(|s| {
                s.tx.try_send((s.channel, s.handle, encoded.clone()))
                    .is_ok()
            })
            .count()
    }
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let container = p
        .map(|p| p.get_optional_string("container_id"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_CONTAINER.to_owned());
    ensure!(
        !container.is_empty()
            && container.len() <= 256
            && !crate::utils::sanitize::has_controls(&container),
        "container_id is 1 to 256 printable characters"
    );
    let require_sasl = p
        .map(|p| p.get_optional_bool("require_sasl"))
        .transpose()?
        .flatten()
        .unwrap_or(false);
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    ensure!(
        (1..=3600).contains(&idle),
        "idle_timeout_secs must be 1..=3600"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("AMQP 1.0 container {container} on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        container,
        require_sasl,
        idle: Duration::from_secs(idle),
        topics: Mutex::default(),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                b"",
                "AMQP1",
                Some(&shared.ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: local,
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
            let child = shared.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = connection(&child, id, stream).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("AMQP1 connection {id}: {e:#}"));
                    }
                    {
                        let mut topics = child.topics.lock().unwrap_or_else(|e| e.into_inner());
                        for subs in topics.values_mut() {
                            subs.retain(|s| s.conn != id);
                        }
                        topics.retain(|_, s| !s.is_empty());
                    }
                    child
                        .ctx
                        .state
                        .remove_peer_handle(server_id, id.as_u32())
                        .await;
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("AMQP1 connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// Ask the handler; its answers in order, or the reason it gave none.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Vec<Json>, String> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::Amqp1Protocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(crate::utils::WireFailure::classify(&e).text().to_owned());
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, operation, "fail_closed_invalid_reply");
        return Err("the server could not decide this request".into());
    }
    let mut answers = Vec::new();
    let mut pending: Vec<ActionResult> = result.protocol_results.into_iter().rev().collect();
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items.into_iter().rev()),
            _ => {}
        }
    }
    if answers.is_empty() {
        outcome(ctx, id, operation, "model_silent");
        return Err("the server could not decide this request".into());
    }
    let decision = if answers
        .iter()
        .any(|a| a["type"] == "amqp1_reject" || a["type"] == "amqp1_release")
    {
        "model_reject"
    } else {
        "model_answer"
    };
    outcome(ctx, id, operation, decision);
    Ok(answers)
}

enum Role {
    /// The client publishes; we receive and grant credit.
    Receiving {
        credit: u32,
        delivery_count: u32,
        buffer: Vec<u8>,
        delivery_id: Option<u32>,
        settled: bool,
    },
    /// The client consumes; we send within the credit it grants.
    Sending {
        credit: u32,
        delivery_count: u32,
        held: VecDeque<Arc<Vec<u8>>>,
        asked: bool,
        settled_mode: bool,
    },
}

struct Link {
    address: String,
    role: Role,
}

struct Session {
    /// Our next transfer ID (delivery IDs share it).
    next_delivery: u32,
    /// The peer's next transfer ID, as its begin and transfers say.
    next_incoming: u32,
    links: HashMap<u32, Link>,
}

struct Conn<'a> {
    shared: &'a Shared,
    id: ConnectionId,
    w: tokio::io::WriteHalf<TcpStream>,
    peer_max_frame: u32,
    sessions: HashMap<u16, Session>,
    inbox: mpsc::Sender<Delivery>,
}

impl Conn<'_> {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.w.write_all(bytes).await?;
        self.shared
            .ctx
            .state
            .update_connection_stats(
                self.shared.ctx.server_id,
                self.id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }
    async fn send(&mut self, channel: u16, perf: Value, payload: &[u8]) -> Result<()> {
        let bytes = frame::encode(TYPE_AMQP, channel, Some(&perf), payload);
        self.write(&bytes).await
    }
    /// A link flow carrying this session's real transfer counters.
    async fn flow(
        &mut self,
        channel: u16,
        handle: u32,
        delivery_count: u32,
        credit: u32,
    ) -> Result<()> {
        let (next_in, next_out) = self
            .sessions
            .get(&channel)
            .map(|s| (s.next_incoming, s.next_delivery))
            .unwrap_or((0, 0));
        let perf = Value::described(
            FLOW,
            vec![
                Value::Uint(next_in),
                Value::Uint(2048),
                Value::Uint(next_out),
                Value::Uint(2048),
                Value::Uint(handle),
                Value::Uint(delivery_count),
                Value::Uint(credit),
            ],
        );
        self.send(channel, perf, &[]).await
    }
    async fn close(&mut self, condition: &str, description: &str) -> Result<()> {
        self.send(
            0,
            Value::described(CLOSE, vec![frame::error(condition, description)]),
            &[],
        )
        .await
    }

    /// Send `msg` on one of our sending links, split to the peer's max-frame-size.
    async fn transfer(&mut self, channel: u16, handle: u32, msg: &[u8]) -> Result<()> {
        let Some(session) = self.sessions.get_mut(&channel) else {
            return Ok(());
        };
        let delivery_id = session.next_delivery;
        session.next_delivery = session.next_delivery.wrapping_add(1);
        let settled = matches!(
            session.links.get(&handle).map(|l| &l.role),
            Some(Role::Sending {
                settled_mode: true,
                ..
            })
        );
        let tag = Value::Binary(delivery_id.to_be_bytes().to_vec());
        let room = (self.peer_max_frame as usize).saturating_sub(64).max(256);
        let pieces: Vec<&[u8]> = if msg.is_empty() {
            vec![&[][..]]
        } else {
            msg.chunks(room).collect()
        };
        let n = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            let more = i + 1 < n;
            let perf = if i == 0 {
                Value::described(
                    TRANSFER,
                    vec![
                        Value::Uint(handle),
                        Value::Uint(delivery_id),
                        tag.clone(),
                        Value::Uint(0),
                        Value::Bool(settled),
                        Value::Bool(more),
                    ],
                )
            } else {
                Value::described(
                    TRANSFER,
                    vec![
                        Value::Uint(handle),
                        Value::Null,
                        Value::Null,
                        Value::Null,
                        Value::Bool(settled),
                        Value::Bool(more),
                    ],
                )
            };
            self.send(channel, perf, piece).await?;
        }
        Ok(())
    }

    /// Deliver what a sending link holds while it has credit; true when it is now idle with
    /// credit left (worth asking the handler for messages).
    async fn pump(&mut self, channel: u16, handle: u32) -> Result<bool> {
        loop {
            let next = {
                let Some(link) = self
                    .sessions
                    .get_mut(&channel)
                    .and_then(|s| s.links.get_mut(&handle))
                else {
                    return Ok(false);
                };
                let Role::Sending {
                    credit,
                    delivery_count,
                    held,
                    ..
                } = &mut link.role
                else {
                    return Ok(false);
                };
                if *credit == 0 {
                    return Ok(false);
                }
                match held.pop_front() {
                    Some(m) => {
                        *credit -= 1;
                        *delivery_count = delivery_count.wrapping_add(1);
                        m
                    }
                    None => return Ok(true),
                }
            };
            self.transfer(channel, handle, &next).await?;
        }
    }

    async fn deliver(&mut self, channel: u16, handle: u32, msg: Arc<Vec<u8>>) -> Result<()> {
        if let Some(Link {
            role: Role::Sending { held, .. },
            ..
        }) = self
            .sessions
            .get_mut(&channel)
            .and_then(|s| s.links.get_mut(&handle))
        {
            if held.len() < HELD_PER_LINK {
                held.push_back(msg);
            }
        }
        self.pump(channel, handle).await.map(|_| ())
    }

    /// Carry out amqp1_send answers: to the asking link when no address is named.
    async fn sends(&mut self, answers: &[Json], asking: Option<(u16, u32)>) -> Result<usize> {
        let mut n = 0;
        for a in answers.iter().filter(|a| a["type"] == "amqp1_send") {
            let Ok(encoded) = message::encode(&a["message"]) else {
                continue;
            };
            let encoded = Arc::new(encoded);
            match (a["address"].as_str(), asking) {
                (Some(address), _) => n += self.shared.relay(address, encoded),
                (None, Some((channel, handle))) => {
                    self.deliver(channel, handle, encoded).await?;
                    n += 1;
                }
                (None, None) => {}
            }
        }
        Ok(n)
    }
}

async fn connection(shared: &Shared, id: ConnectionId, stream: TcpStream) -> Result<()> {
    let ctx = &shared.ctx;
    let (mut r, w) = tokio::io::split(stream);
    let (inbox_tx, mut inbox) = mpsc::channel::<Delivery>(INBOX);
    let mut c = Conn {
        shared,
        id,
        w,
        peer_max_frame: MIN_MAX_FRAME,
        sessions: HashMap::new(),
        inbox: inbox_tx,
    };
    // Protocol headers and SASL.
    let mut head = tokio::time::timeout(HANDSHAKE_TIMEOUT, frame::header(&mut r))
        .await
        .context("no protocol header in time")??;
    let mut admitted = false;
    if head == SASL_HEADER {
        c.write(&SASL_HEADER).await?;
        let mechanisms = Value::described(
            SASL_MECHANISMS,
            vec![Value::Array(vec![
                Value::sym("PLAIN"),
                Value::sym("ANONYMOUS"),
            ])],
        );
        c.write(&frame::encode(TYPE_SASL, 0, Some(&mechanisms), &[]))
            .await?;
        let init = tokio::time::timeout(HANDSHAKE_TIMEOUT, frame::read(&mut r, 64 * 1024))
            .await
            .context("no sasl-init in time")??
            .context("closed during SASL")?;
        let init = frame::expect(&init, TYPE_SASL, SASL_INIT)?.clone();
        let mechanism = field(&init, 0).as_str().unwrap_or_default().to_owned();
        let (user, password) = match (mechanism.as_str(), field(&init, 1)) {
            ("PLAIN", Value::Binary(b)) => {
                let parts: Vec<&[u8]> = b.split(|x| *x == 0).collect();
                ensure!(parts.len() == 3, "malformed PLAIN response");
                (
                    Some(String::from_utf8_lossy(parts[1]).into_owned()),
                    Some(String::from_utf8_lossy(parts[2]).into_owned()),
                )
            }
            ("ANONYMOUS", _) => (None, None),
            (other, _) => {
                outcome(ctx, id, "connect", "protocol_refusal");
                c.write(&frame::encode(
                    TYPE_SASL,
                    0,
                    Some(&Value::described(SASL_OUTCOME, vec![Value::Ubyte(1)])),
                    &[],
                ))
                .await?;
                bail!("unsupported SASL mechanism {other:?}");
            }
        };
        let event = Event::new(
            &actions::CONNECT_EVENT,
            json!({"mechanism": mechanism, "user": user, "password": password}),
        );
        let code = match ask(shared, id, event, "connect").await {
            Ok(a) if a.first().is_some_and(|a| a["type"] == "amqp1_accept") => 0u8,
            Ok(_) => 1,
            Err(_) => 2,
        };
        c.write(&frame::encode(
            TYPE_SASL,
            0,
            Some(&Value::described(SASL_OUTCOME, vec![Value::Ubyte(code)])),
            &[],
        ))
        .await?;
        if code != 0 {
            return Ok(());
        }
        admitted = true;
        head = tokio::time::timeout(HANDSHAKE_TIMEOUT, frame::header(&mut r))
            .await
            .context("no AMQP header after SASL")??;
    } else if shared.require_sasl {
        outcome(ctx, id, "connect", "protocol_refusal");
        c.write(&SASL_HEADER).await?;
        return Ok(());
    }
    if head != AMQP_HEADER {
        c.write(&AMQP_HEADER).await?;
        bail!("unsupported protocol header {head:?}");
    }
    c.write(&AMQP_HEADER).await?;
    let open = tokio::time::timeout(HANDSHAKE_TIMEOUT, frame::read(&mut r, MAX_FRAME))
        .await
        .context("no open in time")??
        .context("closed before open")?;
    let open = frame::expect(&open, TYPE_AMQP, OPEN)?.clone();
    c.peer_max_frame = field(&open, 2)
        .as_u64()
        .map(|m| m.clamp(MIN_MAX_FRAME as u64, MAX_FRAME as u64) as u32)
        .unwrap_or(MAX_FRAME);
    let peer_idle = field(&open, 4)
        .as_u64()
        .filter(|t| *t > 0)
        .map(Duration::from_millis);
    let reply = Value::described(
        OPEN,
        vec![
            Value::str(&shared.container),
            Value::Null,
            Value::Uint(MAX_FRAME),
            Value::Ushort(CHANNEL_MAX),
            Value::Uint(shared.idle.as_millis() as u32),
        ],
    );
    c.send(0, reply, &[]).await?;
    if !admitted {
        let event = Event::new(
            &actions::CONNECT_EVENT,
            json!({"mechanism": "none", "container_id": field(&open, 0).as_str(), "hostname": field(&open, 1).as_str()}),
        );
        match ask(shared, id, event, "connect").await {
            Ok(a) if a.first().is_some_and(|a| a["type"] == "amqp1_accept") => {}
            Ok(a) => {
                let first = a.first().cloned().unwrap_or_default();
                c.close(
                    first["condition"]
                        .as_str()
                        .unwrap_or("amqp:unauthorized-access"),
                    first["description"].as_str().unwrap_or("refused"),
                )
                .await?;
                return Ok(());
            }
            Err(text) => {
                c.close("amqp:internal-error", &text).await?;
                return Ok(());
            }
        }
    }
    // A dedicated reader: frame reads are not cancel-safe.
    let (frame_tx, mut frames) = mpsc::channel::<Result<Frame>>(64);
    let _reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            let f = frame::read(&mut r, MAX_FRAME).await;
            let stop = !matches!(f, Ok(Some(_)));
            let item = match f {
                Ok(Some(f)) => Ok(f),
                Ok(None) => Err(anyhow::anyhow!("the client closed the connection")),
                Err(e) => Err(e),
            };
            if frame_tx.send(item).await.is_err() || stop {
                return;
            }
        }
    }));
    let mut commands =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    let keepalive = peer_idle
        .map(|t| t / 2)
        .unwrap_or(Duration::from_secs(3600));
    let mut tick = tokio::time::interval(keepalive);
    tick.tick().await;
    let mut last_in = tokio::time::Instant::now();
    loop {
        enum Wake {
            Frame(Option<Result<Frame>>),
            Delivery(Option<Delivery>),
            Command(Option<ClientCommand>),
            Tick,
            Idle,
        }
        let wake = tokio::select! {
            f = frames.recv() => Wake::Frame(f),
            d = inbox.recv() => Wake::Delivery(d),
            cmd = commands.recv() => Wake::Command(cmd),
            _ = tick.tick() => Wake::Tick,
            _ = tokio::time::sleep_until(last_in + shared.idle) => Wake::Idle,
        };
        match wake {
            Wake::Tick => c.write(&frame::encode(TYPE_AMQP, 0, None, &[])).await?,
            Wake::Idle => {
                c.close("amqp:resource-limit-exceeded", "idle timeout")
                    .await?;
                return Ok(());
            }
            Wake::Delivery(Some((channel, handle, msg))) => c.deliver(channel, handle, msg).await?,
            Wake::Delivery(None) | Wake::Command(None) => {}
            Wake::Command(Some(cmd)) => {
                let a = cmd.action.clone();
                let outcome = match a["type"].as_str() {
                    Some("amqp1_send") => match actions::check_answer(&a) {
                        Err(e) => ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                        Ok(()) if a["address"].is_null() => ClientSendOutcome::Rejected {
                            error: "amqp1_send from the operator names an address".into(),
                        },
                        Ok(()) => ClientSendOutcome::Sent {
                            bytes_sent: c.sends(std::slice::from_ref(&a), None).await?,
                        },
                    },
                    Some("disconnect") => {
                        c.close("amqp:connection:forced", "closed by the operator")
                            .await?;
                        ClientSendOutcome::Disconnected
                    }
                    _ => ClientSendOutcome::Rejected {
                        error: "an AMQP 1.0 connection accepts amqp1_send or disconnect".into(),
                    },
                };
                ctx.state
                    .record_access_log(
                        crate::state::AccessLogOwner::Server(ctx.server_id.as_u32()),
                        "AMQP1",
                        Some(id.as_u32()),
                        "injected_action",
                        json!({"type": a["type"], "address": a["address"]}),
                        vec![serde_json::to_value(&outcome).unwrap_or(Json::Null)],
                    )
                    .await;
                let done = matches!(outcome, ClientSendOutcome::Disconnected);
                let _ = cmd.reply_tx.send(Ok(outcome));
                if done {
                    return Ok(());
                }
            }
            Wake::Frame(None) => return Ok(()),
            Wake::Frame(Some(Err(e))) => {
                let _ = c.close("amqp:decode-error", "malformed frame").await;
                return Err(e);
            }
            Wake::Frame(Some(Ok(f))) => {
                last_in = tokio::time::Instant::now();
                ctx.state
                    .update_connection_stats(
                        ctx.server_id,
                        id,
                        Some(f.payload.len() as u64 + 8),
                        None,
                        Some(1),
                        None,
                    )
                    .await;
                match on_frame(&mut c, f).await {
                    Ok(true) => {}
                    Ok(false) => return Ok(()),
                    Err(e) => {
                        let _ = c
                            .close(
                                "amqp:not-allowed",
                                &crate::utils::truncate::truncate_for_log(&format!("{e:#}"), 200),
                            )
                            .await;
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// One frame after open; false to close.
async fn on_frame(c: &mut Conn<'_>, f: Frame) -> Result<bool> {
    let Some(perf) = f.body else { return Ok(true) };
    ensure!(f.kind == TYPE_AMQP, "SASL frame after open");
    let channel = f.channel;
    match perf.descriptor() {
        Some(BEGIN) => {
            ensure!(
                channel <= CHANNEL_MAX && !c.sessions.contains_key(&channel),
                "channel {channel} is in use or over channel-max"
            );
            let next_incoming = field(&perf, 1).as_u64().unwrap_or(0) as u32;
            c.sessions.insert(
                channel,
                Session {
                    next_delivery: 0,
                    next_incoming,
                    links: HashMap::new(),
                },
            );
            let reply = Value::described(
                BEGIN,
                vec![
                    Value::Ushort(channel),
                    Value::Uint(0),
                    Value::Uint(2048),
                    Value::Uint(2048),
                    Value::Uint(HANDLE_MAX),
                ],
            );
            c.send(channel, reply, &[]).await?;
        }
        Some(ATTACH) => on_attach(c, channel, &perf).await?,
        Some(FLOW) => on_flow(c, channel, &perf).await?,
        Some(TRANSFER) => on_transfer(c, channel, &perf, f.payload).await?,
        Some(DISPOSITION) => {}
        Some(DETACH) => {
            let handle = field(&perf, 0).as_u64().unwrap_or(0) as u32;
            // A link we refused was already detached by us; answering the client's detach of it
            // with a second one is a protocol error (rhea: "Detach already received").
            let Some(link) = c
                .sessions
                .get_mut(&channel)
                .and_then(|s| s.links.remove(&handle))
            else {
                return Ok(true);
            };
            {
                if matches!(link.role, Role::Sending { .. }) {
                    let mut topics = c.shared.topics.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(subs) = topics.get_mut(&link.address) {
                        subs.retain(|s| {
                            !(s.conn == c.id && s.channel == channel && s.handle == handle)
                        });
                    }
                }
            }
            c.send(
                channel,
                Value::described(DETACH, vec![Value::Uint(handle), Value::Bool(true)]),
                &[],
            )
            .await?;
        }
        Some(END) => {
            if let Some(session) = c.sessions.remove(&channel) {
                let mut topics = c.shared.topics.lock().unwrap_or_else(|e| e.into_inner());
                for subs in topics.values_mut() {
                    subs.retain(|s| {
                        !(s.conn == c.id
                            && s.channel == channel
                            && session.links.contains_key(&s.handle))
                    });
                }
            }
            c.send(channel, Value::described(END, vec![]), &[]).await?;
        }
        Some(CLOSE) => {
            c.send(0, Value::described(CLOSE, vec![]), &[]).await?;
            return Ok(false);
        }
        Some(OPEN) => bail!("a second open"),
        other => bail!("unexpected performative {other:?}"),
    }
    Ok(true)
}

async fn on_attach(c: &mut Conn<'_>, channel: u16, perf: &Value) -> Result<()> {
    let name = field(perf, 0).as_str().unwrap_or_default().to_owned();
    let handle = field(perf, 1).as_u64().context("attach without a handle")? as u32;
    ensure!(handle <= HANDLE_MAX, "handle {handle} is over handle-max");
    let session = c
        .sessions
        .get(&channel)
        .context("attach on a channel with no session")?;
    ensure!(
        !session.links.contains_key(&handle),
        "handle {handle} is in use"
    );
    // The client's role: false = sender (it publishes), true = receiver (it consumes).
    let client_receives = field(perf, 2).as_bool().unwrap_or(false);
    let snd_settle_mode = field(perf, 3).as_u64().unwrap_or(2);
    let (source, target) = (field(perf, 5).clone(), field(perf, 6).clone());
    let address = if client_receives {
        frame::address(&source)
    } else {
        frame::address(&target)
    }
    .unwrap_or_default();
    let direction = if client_receives {
        "consume"
    } else {
        "publish"
    };
    let decision = if address.is_empty()
        || address.len() > 256
        || crate::utils::sanitize::has_controls(&address)
    {
        Err((
            "amqp:invalid-field".to_owned(),
            "the link names no address".to_owned(),
        ))
    } else {
        let event = Event::new(
            &actions::ATTACH_EVENT,
            json!({"direction": direction, "address": address, "link_name": name}),
        );
        match ask(c.shared, c.id, event, "attach").await {
            Ok(a) if a.first().is_some_and(|a| a["type"] == "amqp1_accept") => Ok(()),
            Ok(a) => {
                let first = a.first().cloned().unwrap_or_default();
                Err((
                    first["condition"]
                        .as_str()
                        .unwrap_or("amqp:unauthorized-access")
                        .to_owned(),
                    first["description"]
                        .as_str()
                        .unwrap_or("refused")
                        .to_owned(),
                ))
            }
            Err(text) => Err(("amqp:internal-error".to_owned(), text)),
        }
    };
    // Our attach mirrors the client's, with the opposite role.
    let our_role = Value::Bool(!client_receives);
    match decision {
        Err((condition, description)) => {
            // Refusal (part 2, 2.6.3): a null terminus on our side, then detach with the error.
            let (src, tgt) = if client_receives {
                (Value::Null, target)
            } else {
                (source, Value::Null)
            };
            c.send(
                channel,
                Value::described(
                    ATTACH,
                    vec![
                        Value::str(&name),
                        Value::Uint(handle),
                        our_role,
                        Value::Ubyte(snd_settle_mode as u8),
                        Value::Ubyte(0),
                        src,
                        tgt,
                        Value::Null,
                        Value::Null,
                        Value::Uint(0),
                    ],
                ),
                &[],
            )
            .await?;
            c.send(
                channel,
                Value::described(
                    DETACH,
                    vec![
                        Value::Uint(handle),
                        Value::Bool(true),
                        frame::error(&condition, &description),
                    ],
                ),
                &[],
            )
            .await?;
        }
        Ok(()) => {
            let initial = field(perf, 9).as_u64().unwrap_or(0) as u32;
            c.send(
                channel,
                Value::described(
                    ATTACH,
                    vec![
                        Value::str(&name),
                        Value::Uint(handle),
                        our_role,
                        Value::Ubyte(snd_settle_mode as u8),
                        Value::Ubyte(0),
                        source,
                        target,
                        Value::Null,
                        Value::Null,
                        Value::Uint(0),
                    ],
                ),
                &[],
            )
            .await?;
            let role = if client_receives {
                c.shared
                    .topics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(address.clone())
                    .or_default()
                    .push(Subscriber {
                        conn: c.id,
                        channel,
                        handle,
                        tx: c.inbox.clone(),
                    });
                Role::Sending {
                    credit: 0,
                    delivery_count: 0,
                    held: VecDeque::new(),
                    asked: false,
                    settled_mode: snd_settle_mode == 1,
                }
            } else {
                Role::Receiving {
                    credit: RECEIVE_CREDIT,
                    delivery_count: initial,
                    buffer: Vec::new(),
                    delivery_id: None,
                    settled: false,
                }
            };
            if let Some(s) = c.sessions.get_mut(&channel) {
                s.links.insert(handle, Link { address, role });
            }
            if !client_receives {
                c.flow(channel, handle, initial, RECEIVE_CREDIT).await?;
            }
        }
    }
    Ok(())
}

async fn on_flow(c: &mut Conn<'_>, channel: u16, perf: &Value) -> Result<()> {
    let Some(handle) = field(perf, 4).as_u64().map(|h| h as u32) else {
        return Ok(());
    };
    let mut ask_for = None;
    if let Some(link) = c
        .sessions
        .get_mut(&channel)
        .and_then(|s| s.links.get_mut(&handle))
    {
        if let Role::Sending {
            credit,
            delivery_count,
            asked,
            held,
            ..
        } = &mut link.role
        {
            let peer_count = field(perf, 5).as_u64().map(|n| n as u32).unwrap_or(0);
            let link_credit = field(perf, 6).as_u64().unwrap_or(0) as u32;
            // Part 2, 2.6.7: credit = delivery-count(receiver) + link-credit(receiver) - delivery-count(sender).
            *credit = peer_count
                .wrapping_add(link_credit)
                .wrapping_sub(*delivery_count);
            if *credit > 0 && held.is_empty() && !*asked {
                *asked = true;
                ask_for = Some((link.address.clone(), *credit));
            }
        }
    }
    let idle_with_credit = c.pump(channel, handle).await?;
    if let (Some((address, credit)), true) = (ask_for, idle_with_credit) {
        let event = Event::new(
            &actions::CREDIT_EVENT,
            json!({"address": address, "credit": credit}),
        );
        if let Ok(answers) = ask(c.shared, c.id, event, "credit").await {
            c.sends(&answers, Some((channel, handle))).await?;
        }
    }
    // echo is field 9 of flow.
    if field(perf, 9).as_bool() == Some(true) {
        let state = match c
            .sessions
            .get(&channel)
            .and_then(|s| s.links.get(&handle))
            .map(|l| &l.role)
        {
            Some(Role::Sending {
                credit,
                delivery_count,
                ..
            }) => Some((*delivery_count, *credit)),
            Some(Role::Receiving {
                credit,
                delivery_count,
                ..
            }) => Some((*delivery_count, *credit)),
            None => None,
        };
        if let Some((count, credit)) = state {
            c.flow(channel, handle, count, credit).await?;
        }
    }
    Ok(())
}

async fn on_transfer(c: &mut Conn<'_>, channel: u16, perf: &Value, payload: Vec<u8>) -> Result<()> {
    let handle = field(perf, 0)
        .as_u64()
        .context("transfer without a handle")? as u32;
    if let Some(s) = c.sessions.get_mut(&channel) {
        s.next_incoming = s.next_incoming.wrapping_add(1);
    }
    let more = field(perf, 5).as_bool().unwrap_or(false);
    let aborted = field(perf, 9).as_bool().unwrap_or(false);
    let (address, complete) = {
        let link = c
            .sessions
            .get_mut(&channel)
            .and_then(|s| s.links.get_mut(&handle))
            .context("transfer on an unknown link")?;
        let Role::Receiving {
            credit,
            buffer,
            delivery_id,
            settled,
            ..
        } = &mut link.role
        else {
            bail!("transfer on a link the client receives on")
        };
        if buffer.is_empty() && delivery_id.is_none() {
            ensure!(*credit > 0, "transfer without link credit");
            *delivery_id = field(perf, 1).as_u64().map(|n| n as u32);
            *settled = field(perf, 4).as_bool().unwrap_or(false);
        }
        ensure!(
            buffer.len() + payload.len() <= message::MAX_MESSAGE,
            "message over {} KiB",
            message::MAX_MESSAGE / 1024
        );
        buffer.extend(payload);
        if aborted {
            buffer.clear();
            *delivery_id = None;
            return Ok(());
        }
        if more {
            return Ok(());
        }
        (
            link.address.clone(),
            (std::mem::take(buffer), delivery_id.take(), *settled),
        )
    };
    let (bytes, delivery_id, settled) = complete;
    let refill = {
        let link = c
            .sessions
            .get_mut(&channel)
            .and_then(|s| s.links.get_mut(&handle))
            .context("link vanished")?;
        let Role::Receiving {
            credit,
            delivery_count,
            ..
        } = &mut link.role
        else {
            unreachable!()
        };
        *credit -= 1;
        *delivery_count = delivery_count.wrapping_add(1);
        (*credit < RECEIVE_CREDIT / 2).then(|| {
            *credit = RECEIVE_CREDIT;
            *delivery_count
        })
    };
    let decided = match message::decode(&bytes) {
        Err(e) => Err((
            "amqp:decode-error".to_owned(),
            crate::utils::truncate::truncate_for_log(&format!("{e:#}"), 200),
        )),
        Ok(msg) => {
            let event = Event::new(
                &actions::MESSAGE_EVENT,
                json!({"address": address, "message": msg, "settled": settled}),
            );
            match ask(c.shared, c.id, event, "message").await {
                Err(text) => Err(("amqp:internal-error".to_owned(), text)),
                Ok(answers) => {
                    c.sends(&answers, None).await?;
                    match answers.iter().find(|a| {
                        matches!(
                            a["type"].as_str(),
                            Some("amqp1_accept" | "amqp1_reject" | "amqp1_release")
                        )
                    }) {
                        Some(a) if a["type"] == "amqp1_accept" => {
                            c.shared.relay(&address, Arc::new(bytes));
                            Ok(Value::described(ACCEPTED, vec![]))
                        }
                        Some(a) if a["type"] == "amqp1_release" => {
                            Ok(Value::described(RELEASED, vec![]))
                        }
                        Some(a) => Err((
                            a["condition"]
                                .as_str()
                                .unwrap_or("amqp:not-allowed")
                                .to_owned(),
                            a["description"].as_str().unwrap_or("rejected").to_owned(),
                        )),
                        None => Err((
                            "amqp:internal-error".to_owned(),
                            "the server gave no outcome for this message".to_owned(),
                        )),
                    }
                }
            }
        }
    };
    let state = decided.unwrap_or_else(|(condition, description)| {
        Value::described(REJECTED, vec![frame::error(&condition, &description)])
    });
    if let (false, Some(id)) = (settled, delivery_id) {
        c.send(
            channel,
            Value::described(
                DISPOSITION,
                vec![
                    Value::Bool(true),
                    Value::Uint(id),
                    Value::Uint(id),
                    Value::Bool(true),
                    state,
                ],
            ),
            &[],
        )
        .await?;
    }
    if let Some(count) = refill {
        c.flow(channel, handle, count, RECEIVE_CREDIT).await?;
    }
    Ok(())
}
