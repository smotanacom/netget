//! RTMP live server. Rust owns the handshake, chunking, AMF0 commands, the NetConnection and
//! NetStream flow and the live relay from each publisher to its players (sequence headers and
//! metadata cached, players joining at a keyframe, timestamps rebased per player); the handler
//! admits connections, publishers and players and can push AMF0 data messages into a stream.
pub mod actions;
pub mod amf0;
pub mod chunk;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{bail, Context, Result};
use chunk::Message;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
pub const MAX_PLAYERS: usize = 64;
const OUT_CHUNK_SIZE: usize = 4096;
const WINDOW: u32 = 2_500_000;
const PLAYER_QUEUE: usize = 1024;

pub use crate::utils::task_guard::AbortOnDrop;

/// Media and data relayed to one player: (type, publisher timestamp, payload).
type Relay = (u8, u32, Arc<Vec<u8>>);

#[derive(Default)]
struct Stats {
    started: Option<Instant>,
    video: u64,
    audio: u64,
    keyframes: u64,
    bytes: u64,
}

struct Live {
    publisher: Option<ConnectionId>,
    metadata: Option<Arc<Vec<u8>>>,
    video_header: Option<Arc<Vec<u8>>>,
    audio_header: Option<Arc<Vec<u8>>>,
    players: HashMap<ConnectionId, mpsc::Sender<Relay>>,
    stats: Stats,
}

impl Live {
    fn new() -> Self {
        Self {
            publisher: None,
            metadata: None,
            video_header: None,
            audio_header: None,
            players: HashMap::new(),
            stats: Stats::default(),
        }
    }
    /// Fan out to every player; a player whose queue is full is too slow to keep and is dropped.
    fn relay(&mut self, item: Relay) {
        self.players
            .retain(|_, tx| tx.try_send(item.clone()).is_ok());
    }
}

struct Shared {
    ctx: SpawnContext,
    /// "app/stream" → live stream
    streams: Mutex<HashMap<String, Live>>,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=3600).contains(&idle),
        "idle_timeout_secs must be 1..=3600"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("RTMP server at rtmp://{local}/"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        streams: Mutex::default(),
        idle: Duration::from_secs(idle),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                b"",
                "RTMP",
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
                    let mut conn = Conn::new(&child, id);
                    if let Err(e) = conn.run(stream).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("RTMP connection {id}: {e:#}"));
                    }
                    conn.cleanup().await;
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
    let summary = format!("RTMP connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// Ask the handler; Ok(None) is an rtmp_accept, Ok(Some(reason)) a refusal, Err a failure.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Option<String>, String> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RtmpProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(crate::utils::WireFailure::classify(&e).text().to_owned());
        }
    };
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match (result.failures.is_empty(), answers.as_slice()) {
        (true, [a]) if a["type"] == "rtmp_accept" || a["type"] == "rtmp_ignore" => {
            outcome(ctx, id, operation, "model_answer");
            Ok(None)
        }
        (true, [a]) if a["type"] == "rtmp_reject" => {
            outcome(ctx, id, operation, "model_reject");
            Ok(Some(
                a["description"].as_str().unwrap_or("refused").to_owned(),
            ))
        }
        (true, []) => {
            outcome(ctx, id, operation, "model_silent");
            Err("the server could not decide this request".into())
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Err("the server could not decide this request".into())
        }
    }
}

fn status(code: &str, level: &str, description: &str) -> Value {
    json!({"level": level, "code": code, "description": description})
}

/// What one connection is doing with one of its message streams.
enum Role {
    Publishing(String),
    Playing(String),
}

struct Conn<'a> {
    shared: &'a Shared,
    id: ConnectionId,
    w: Option<tokio::io::WriteHalf<TcpStream>>,
    writer: chunk::Writer,
    app: Option<String>,
    next_stream: u32,
    roles: HashMap<u32, Role>,
    /// Player state per message stream: (relay receiver, timestamp base, waiting for a keyframe)
    players: HashMap<u32, (mpsc::Receiver<Relay>, Option<u32>, bool)>,
    peer_window: u32,
    acked: u64,
}

impl<'a> Conn<'a> {
    fn new(shared: &'a Shared, id: ConnectionId) -> Self {
        Self {
            shared,
            id,
            w: None,
            writer: chunk::Writer::default(),
            app: None,
            next_stream: 1,
            roles: HashMap::new(),
            players: HashMap::new(),
            peer_window: WINDOW,
            acked: 0,
        }
    }

    async fn send(&mut self, csid: u32, m: &Message) -> Result<()> {
        let bytes = self.writer.encode(csid, m)?;
        let w = self.w.as_mut().context("not connected")?;
        w.write_all(&bytes).await?;
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

    async fn command(&mut self, stream_id: u32, values: &[Value]) -> Result<()> {
        let payload = amf0::encode(values)?;
        self.send(
            3,
            &Message {
                type_id: chunk::COMMAND_AMF0,
                stream_id,
                timestamp: 0,
                payload,
            },
        )
        .await
    }

    async fn on_status(&mut self, stream_id: u32, info: Value) -> Result<()> {
        self.command(stream_id, &[json!("onStatus"), json!(0), Value::Null, info])
            .await
    }

    /// The stream a play or publish names, as (stream name, "app/stream" key). A client that
    /// carries the whole path as its app and sends an empty name (MediaMTX's gortmplib, for
    /// rtmp://host/live/cam) names the same stream as app "live" with stream "cam".
    fn target(&self, name: &str) -> Option<(String, String)> {
        let app = self.app.as_deref().unwrap_or_default();
        let name = name.split('?').next().unwrap_or_default();
        let (stream, key) = if name.is_empty() {
            let (_, last) = app.rsplit_once('/')?;
            (last.to_owned(), app.to_owned())
        } else {
            (name.to_owned(), format!("{app}/{name}"))
        };
        (!stream.is_empty()
            && stream.len() <= 256
            && !crate::utils::sanitize::has_controls(&stream))
        .then_some((stream, key))
    }

    async fn run(&mut self, mut stream: TcpStream) -> Result<()> {
        let ctx = &self.shared.ctx;
        tokio::time::timeout(HANDSHAKE_TIMEOUT, chunk::accept(&mut stream))
            .await
            .context("handshake timed out")??;
        let (mut r, w) = tokio::io::split(stream);
        self.w = Some(w);
        // A dedicated reader: chunk reassembly is not cancel-safe, so it never sits in a select.
        let (msg_tx, mut msgs) = mpsc::channel::<Result<(Message, u64)>>(64);
        let reader = AbortOnDrop(tokio::spawn(async move {
            let mut reader = chunk::Reader::default();
            loop {
                let m = reader.read(&mut r).await;
                let failed = m.is_err();
                if let Ok(m) = &m {
                    if m.type_id == chunk::SET_CHUNK_SIZE {
                        if let Err(e) = reader.set_chunk_size(&m.payload) {
                            let _ = msg_tx.send(Err(e)).await;
                            return;
                        }
                    }
                }
                if msg_tx.send(m.map(|m| (m, reader.bytes))).await.is_err() || failed {
                    return;
                }
            }
        }));
        let _reader = reader;
        let mut commands: Option<mpsc::Receiver<ClientCommand>> = None;
        loop {
            enum Wake {
                Message(Option<Result<(Message, u64)>>),
                Relay(u32, Option<Relay>),
                Command(Option<ClientCommand>),
                Idle,
            }
            let idle = self.shared.idle;
            let wake = {
                let players = &mut self.players;
                let relay = async move {
                    if players.is_empty() {
                        std::future::pending::<(u32, Option<Relay>)>().await
                    } else {
                        let futs = players.iter_mut().map(|(sid, (rx, _, _))| {
                            Box::pin(async move { (*sid, rx.recv().await) })
                        });
                        futures::future::select_all(futs).await.0
                    }
                };
                let command = async {
                    match commands.as_mut() {
                        Some(c) => c.recv().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    m = tokio::time::timeout(idle, msgs.recv()) => match m { Ok(m) => Wake::Message(m), Err(_) => Wake::Idle },
                    (sid, item) = relay => Wake::Relay(sid, item),
                    c = command => Wake::Command(c),
                }
            };
            match wake {
                Wake::Idle => {
                    if self.players.is_empty() {
                        bail!("idle for {:?}", self.shared.idle);
                    }
                }
                Wake::Message(None) => return Ok(()),
                Wake::Message(Some(Err(e))) => return Err(e),
                Wake::Message(Some(Ok((m, total)))) => {
                    ctx.state
                        .update_connection_stats(
                            ctx.server_id,
                            self.id,
                            Some(m.payload.len() as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;
                    if total - self.acked >= self.peer_window as u64 {
                        self.acked = total;
                        self.send(
                            2,
                            &chunk::control(chunk::ACK, (total as u32).to_be_bytes().to_vec()),
                        )
                        .await?;
                    }
                    let joined = self.app.is_some();
                    if !self.on_message(m).await? {
                        return Ok(());
                    }
                    if !joined && self.app.is_some() && commands.is_none() {
                        commands = Some(
                            crate::server::peer_support::register_peer_channel(
                                &ctx.state,
                                ctx.server_id,
                                self.id.as_u32(),
                            )
                            .await,
                        );
                    }
                }
                Wake::Relay(sid, None) => {
                    self.players.remove(&sid);
                }
                Wake::Relay(sid, Some((type_id, ts, payload))) => {
                    self.deliver(sid, type_id, ts, payload).await?
                }
                Wake::Command(None) => commands = None,
                Wake::Command(Some(cmd)) => {
                    let outcome = self.injected(&cmd.action).await;
                    ctx.state
                        .record_access_log(
                            crate::state::AccessLogOwner::Server(ctx.server_id.as_u32()),
                            "RTMP",
                            Some(self.id.as_u32()),
                            "injected_action",
                            json!({"type": cmd.action["type"], "handler": cmd.action["handler"]}),
                            vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                        )
                        .await;
                    let done = matches!(outcome, ClientSendOutcome::Disconnected);
                    let _ = cmd.reply_tx.send(Ok(outcome));
                    if done {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Forward relayed media to one player, holding video until the first keyframe and
    /// rebasing timestamps so the player starts at zero.
    async fn deliver(
        &mut self,
        sid: u32,
        type_id: u8,
        ts: u32,
        payload: Arc<Vec<u8>>,
    ) -> Result<()> {
        let Some((_, base, waiting)) = self.players.get_mut(&sid) else {
            return Ok(());
        };
        if type_id == chunk::VIDEO && *waiting {
            if payload.first().is_some_and(|b| (b >> 4) & 0x07 == 1) {
                *waiting = false;
            } else {
                return Ok(());
            }
        }
        let base = *base.get_or_insert(ts);
        let timestamp = ts.wrapping_sub(base);
        let csid = match type_id {
            chunk::AUDIO => 4,
            chunk::VIDEO => 6,
            _ => 5,
        };
        self.send(
            csid,
            &Message {
                type_id,
                stream_id: sid,
                timestamp,
                payload: payload.to_vec(),
            },
        )
        .await
    }

    async fn injected(&mut self, a: &Value) -> ClientSendOutcome {
        match a["type"].as_str() {
            Some("disconnect") => return ClientSendOutcome::Disconnected,
            Some("rtmp_send_data") => {}
            _ => {
                return ClientSendOutcome::Rejected {
                    error: "an RTMP connection accepts rtmp_send_data or disconnect".into(),
                }
            }
        }
        if let Err(e) = actions::check_answer(a) {
            return ClientSendOutcome::Rejected {
                error: e.to_string(),
            };
        }
        let payload = match amf0::encode(&[a["handler"].clone(), a["data"].clone()]) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                return ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }
            }
        };
        // A publisher's connection sends into every player of its stream; a player's into itself.
        let publishing: Vec<String> = self
            .roles
            .values()
            .filter_map(|r| {
                if let Role::Publishing(k) = r {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        if let Some(key) = publishing.first() {
            let mut streams = self
                .shared
                .streams
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let reached = streams.get_mut(key).map(|l| {
                let ts = l
                    .stats
                    .started
                    .map(|s| s.elapsed().as_millis() as u32)
                    .unwrap_or(0);
                l.relay((chunk::DATA_AMF0, ts, payload.clone()));
                l.players.len()
            });
            return ClientSendOutcome::Sent {
                bytes_sent: reached.unwrap_or(0),
            };
        }
        let sids: Vec<u32> = self.players.keys().copied().collect();
        for sid in &sids {
            if let Err(e) = self
                .send(
                    5,
                    &Message {
                        type_id: chunk::DATA_AMF0,
                        stream_id: *sid,
                        timestamp: 0,
                        payload: payload.to_vec(),
                    },
                )
                .await
            {
                return ClientSendOutcome::Rejected {
                    error: e.to_string(),
                };
            }
        }
        if sids.is_empty() {
            ClientSendOutcome::Rejected {
                error: "this connection neither publishes nor plays a stream".into(),
            }
        } else {
            ClientSendOutcome::Sent {
                bytes_sent: payload.len(),
            }
        }
    }

    /// Handle one message; false to close.
    async fn on_message(&mut self, m: Message) -> Result<bool> {
        match m.type_id {
            chunk::SET_CHUNK_SIZE | chunk::ACK | chunk::SET_PEER_BANDWIDTH => {}
            chunk::ABORT => {}
            chunk::WINDOW_ACK_SIZE => {
                let v = u32::from_be_bytes(
                    m.payload
                        .get(..4)
                        .context("short Window Acknowledgement Size")?
                        .try_into()?,
                );
                self.peer_window = v.max(4096);
            }
            chunk::USER_CONTROL => {
                // PingRequest → PingResponse with the same timestamp.
                if m.payload.get(..2) == Some(&[0, 6]) && m.payload.len() >= 6 {
                    let t = u32::from_be_bytes(m.payload[2..6].try_into()?);
                    self.send(2, &chunk::user_control(7, t)).await?;
                }
            }
            chunk::COMMAND_AMF3 | chunk::DATA_AMF3 => bail!("AMF3 messages are not supported"),
            chunk::COMMAND_AMF0 => return self.on_command(&m).await,
            chunk::DATA_AMF0 => self.on_data(&m)?,
            chunk::AUDIO | chunk::VIDEO => self.on_media(&m)?,
            other => Log::new(Some(&self.shared.ctx.status_tx)).debug(format!(
                "RTMP connection {}: ignored message type {other}",
                self.id
            )),
        }
        Ok(true)
    }

    async fn on_command(&mut self, m: &Message) -> Result<bool> {
        let values = amf0::decode_all(&m.payload)?;
        let name = values
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let tx = values.get(1).cloned().unwrap_or(json!(0));
        let arg = |i: usize| {
            values
                .get(i)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        if name != "connect" && self.app.is_none() {
            bail!("{name} before connect");
        }
        match name.as_str() {
            "connect" => {
                let obj = values
                    .get(2)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let app = obj
                    .get("app")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim_matches('/')
                    .to_owned();
                if self.app.is_some()
                    || app.is_empty()
                    || app.len() > 256
                    || crate::utils::sanitize::has_controls(&app)
                {
                    self.command(
                        0,
                        &[
                            json!("_error"),
                            tx,
                            Value::Null,
                            status(
                                "NetConnection.Connect.Rejected",
                                "error",
                                "connect needs an app name, once",
                            ),
                        ],
                    )
                    .await?;
                    return Ok(false);
                }
                let event = Event::new(
                    &actions::CONNECT_EVENT,
                    json!({"app": app, "tc_url": obj.get("tcUrl"), "flash_ver": obj.get("flashVer")}),
                );
                let refusal = match ask(self.shared, self.id, event, "connect").await {
                    Ok(None) => None,
                    Ok(Some(r)) => Some(r),
                    Err(r) => Some(r),
                };
                if let Some(description) = refusal {
                    self.command(
                        0,
                        &[
                            json!("_error"),
                            tx,
                            Value::Null,
                            status("NetConnection.Connect.Rejected", "error", &description),
                        ],
                    )
                    .await?;
                    return Ok(false);
                }
                self.app = Some(app);
                self.send(
                    2,
                    &chunk::control(chunk::WINDOW_ACK_SIZE, WINDOW.to_be_bytes().to_vec()),
                )
                .await?;
                let mut bw = WINDOW.to_be_bytes().to_vec();
                bw.push(2);
                self.send(2, &chunk::control(chunk::SET_PEER_BANDWIDTH, bw))
                    .await?;
                self.send(
                    2,
                    &chunk::control(
                        chunk::SET_CHUNK_SIZE,
                        (OUT_CHUNK_SIZE as u32).to_be_bytes().to_vec(),
                    ),
                )
                .await?;
                self.writer.chunk_size = OUT_CHUNK_SIZE;
                let mut info = status(
                    "NetConnection.Connect.Success",
                    "status",
                    "Connection succeeded.",
                );
                info["objectEncoding"] = json!(0);
                self.command(
                    0,
                    &[
                        json!("_result"),
                        tx,
                        json!({"fmsVer": "FMS/3,0,1,123", "capabilities": 31}),
                        info,
                    ],
                )
                .await?;
            }
            "createStream" => {
                let sid = self.next_stream;
                if sid > 16 {
                    bail!("more than 16 message streams on one connection");
                }
                self.next_stream += 1;
                self.command(0, &[json!("_result"), tx, Value::Null, json!(sid)])
                    .await?;
            }
            "releaseStream" | "FCPublish" | "FCUnpublish" | "getStreamLength" => {
                if tx.as_f64().is_some_and(|t| t != 0.0) {
                    // A live stream has no length; getStreamLength's answer is a number (0).
                    let answer = if name == "getStreamLength" {
                        json!(0)
                    } else {
                        Value::Null
                    };
                    self.command(0, &[json!("_result"), tx, Value::Null, answer])
                        .await?;
                }
                if name == "FCUnpublish" {
                    self.end_publishing(&arg(3)).await;
                }
            }
            "publish" => {
                let (stream, kind) = (
                    arg(3),
                    values
                        .get(4)
                        .and_then(Value::as_str)
                        .unwrap_or("live")
                        .to_owned(),
                );
                let Some((stream, key)) = self
                    .target(&stream)
                    .filter(|_| !self.roles.contains_key(&m.stream_id))
                else {
                    self.on_status(
                        m.stream_id,
                        status("NetStream.Publish.BadName", "error", "invalid stream name"),
                    )
                    .await?;
                    return Ok(true);
                };
                let busy = self
                    .shared
                    .streams
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&key)
                    .is_some_and(|l| l.publisher.is_some());
                if busy {
                    outcome(&self.shared.ctx, self.id, "publish", "protocol_refusal");
                    self.on_status(
                        m.stream_id,
                        status(
                            "NetStream.Publish.BadName",
                            "error",
                            "the stream is already being published",
                        ),
                    )
                    .await?;
                    return Ok(true);
                }
                let event = Event::new(
                    &actions::PUBLISH_EVENT,
                    json!({"app": self.app, "stream": stream, "type": kind}),
                );
                match ask(self.shared, self.id, event, "publish").await {
                    Ok(None) => {
                        let taken = {
                            let mut streams = self
                                .shared
                                .streams
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let live = streams.entry(key.clone()).or_insert_with(Live::new);
                            if live.publisher.is_some() {
                                true
                            } else {
                                live.publisher = Some(self.id);
                                live.stats = Stats {
                                    started: Some(Instant::now()),
                                    ..Default::default()
                                };
                                live.metadata = None;
                                live.video_header = None;
                                live.audio_header = None;
                                false
                            }
                        };
                        if taken {
                            self.on_status(
                                m.stream_id,
                                status(
                                    "NetStream.Publish.BadName",
                                    "error",
                                    "the stream is already being published",
                                ),
                            )
                            .await?;
                            return Ok(true);
                        }
                        self.roles.insert(m.stream_id, Role::Publishing(key));
                        self.send(2, &chunk::user_control(0, m.stream_id)).await?;
                        self.on_status(
                            m.stream_id,
                            status(
                                "NetStream.Publish.Start",
                                "status",
                                &format!("{stream} is now published."),
                            ),
                        )
                        .await?;
                    }
                    Ok(Some(description)) | Err(description) => {
                        self.on_status(
                            m.stream_id,
                            status("NetStream.Publish.Denied", "error", &description),
                        )
                        .await?;
                    }
                }
            }
            "play" => {
                let stream = arg(3);
                let Some((stream, key)) = self
                    .target(&stream)
                    .filter(|_| !self.roles.contains_key(&m.stream_id))
                else {
                    self.on_status(
                        m.stream_id,
                        status(
                            "NetStream.Play.StreamNotFound",
                            "error",
                            "invalid stream name",
                        ),
                    )
                    .await?;
                    return Ok(true);
                };
                let live_now = self
                    .shared
                    .streams
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&key)
                    .is_some_and(|l| l.publisher.is_some());
                let event = Event::new(
                    &actions::PLAY_EVENT,
                    json!({"app": self.app, "stream": stream, "live": live_now}),
                );
                match ask(self.shared, self.id, event, "play").await {
                    Ok(None) => {
                        let (tx_relay, rx) = mpsc::channel::<Relay>(PLAYER_QUEUE);
                        let cached = {
                            let mut streams = self
                                .shared
                                .streams
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            let live = streams.entry(key.clone()).or_insert_with(Live::new);
                            if live.players.len() >= MAX_PLAYERS {
                                None
                            } else {
                                live.players.insert(self.id, tx_relay);
                                Some((
                                    live.metadata.clone(),
                                    live.video_header.clone(),
                                    live.audio_header.clone(),
                                ))
                            }
                        };
                        let Some((metadata, video, audio)) = cached else {
                            self.on_status(
                                m.stream_id,
                                status(
                                    "NetStream.Play.Failed",
                                    "error",
                                    "the stream has its maximum number of players",
                                ),
                            )
                            .await?;
                            return Ok(true);
                        };
                        self.roles.insert(m.stream_id, Role::Playing(key));
                        self.send(2, &chunk::user_control(0, m.stream_id)).await?;
                        self.on_status(
                            m.stream_id,
                            status("NetStream.Play.Reset", "status", "Playing and resetting."),
                        )
                        .await?;
                        self.on_status(
                            m.stream_id,
                            status(
                                "NetStream.Play.Start",
                                "status",
                                &format!("Started playing {stream}."),
                            ),
                        )
                        .await?;
                        let access =
                            amf0::encode(&[json!("|RtmpSampleAccess"), json!(true), json!(true)])?;
                        self.send(
                            5,
                            &Message {
                                type_id: chunk::DATA_AMF0,
                                stream_id: m.stream_id,
                                timestamp: 0,
                                payload: access,
                            },
                        )
                        .await?;
                        for (type_id, item) in [
                            (chunk::DATA_AMF0, metadata),
                            (chunk::VIDEO, video),
                            (chunk::AUDIO, audio),
                        ] {
                            if let Some(p) = item {
                                let csid = if type_id == chunk::VIDEO {
                                    6
                                } else if type_id == chunk::AUDIO {
                                    4
                                } else {
                                    5
                                };
                                self.send(
                                    csid,
                                    &Message {
                                        type_id,
                                        stream_id: m.stream_id,
                                        timestamp: 0,
                                        payload: p.to_vec(),
                                    },
                                )
                                .await?;
                            }
                        }
                        self.players.insert(m.stream_id, (rx, None, true));
                    }
                    Ok(Some(description)) | Err(description) => {
                        self.on_status(
                            m.stream_id,
                            status("NetStream.Play.StreamNotFound", "error", &description),
                        )
                        .await?;
                    }
                }
            }
            "deleteStream" | "closeStream" => {
                let sid = if name == "deleteStream" {
                    values.get(3).and_then(Value::as_f64).unwrap_or(0.0) as u32
                } else {
                    m.stream_id
                };
                self.end_stream(sid).await;
            }
            _ => {
                if tx.as_f64().is_some_and(|t| t != 0.0) {
                    self.command(
                        0,
                        &[
                            json!("_error"),
                            tx,
                            Value::Null,
                            status(
                                "NetConnection.Call.Failed",
                                "error",
                                &format!("{name} is not supported"),
                            ),
                        ],
                    )
                    .await?;
                }
            }
        }
        Ok(true)
    }

    fn publishing_key(&self, stream_id: u32) -> Option<String> {
        match self.roles.get(&stream_id) {
            Some(Role::Publishing(k)) => Some(k.clone()),
            _ => None,
        }
    }

    fn on_data(&mut self, m: &Message) -> Result<()> {
        let Some(key) = self.publishing_key(m.stream_id) else {
            return Ok(());
        };
        let values = amf0::decode_all(&m.payload)?;
        let (handler, rest) = match values.first().and_then(Value::as_str) {
            Some("@setDataFrame") => (
                values
                    .get(1)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                &values[2.min(values.len())..],
            ),
            Some(h) => (h.to_owned(), &values[1..]),
            None => return Ok(()),
        };
        let mut out = vec![json!(handler)];
        out.extend(rest.iter().cloned());
        let payload = Arc::new(match rest.first().and_then(Value::as_object) {
            Some(meta) if handler == "onMetaData" => {
                let mut p = amf0::encode(&[json!("onMetaData")])?;
                p.extend(amf0::encode_ecma(meta)?);
                p
            }
            _ => amf0::encode(&out)?,
        });
        let mut streams = self
            .shared
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(live) = streams.get_mut(&key) {
            if handler == "onMetaData" {
                live.metadata = Some(payload.clone());
            }
            live.relay((chunk::DATA_AMF0, m.timestamp, payload));
        }
        Ok(())
    }

    fn on_media(&mut self, m: &Message) -> Result<()> {
        let Some(key) = self.publishing_key(m.stream_id) else {
            return Ok(());
        };
        let payload = Arc::new(m.payload.clone());
        let mut streams = self
            .shared
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(live) = streams.get_mut(&key) else {
            return Ok(());
        };
        live.stats.bytes += m.payload.len() as u64;
        let b0 = m.payload.first().copied().unwrap_or(0);
        if m.type_id == chunk::VIDEO {
            live.stats.video += 1;
            let enhanced = b0 & 0x80 != 0;
            let sequence_header = if enhanced {
                b0 & 0x0f == 0
            } else {
                matches!(b0 & 0x0f, 7 | 12) && m.payload.get(1) == Some(&0)
            };
            if (b0 >> 4) & 0x07 == 1 {
                live.stats.keyframes += 1;
            }
            if sequence_header {
                live.video_header = Some(payload.clone());
                return Ok(());
            }
        } else {
            live.stats.audio += 1;
            if b0 >> 4 == 10 && m.payload.get(1) == Some(&0) {
                live.audio_header = Some(payload.clone());
                return Ok(());
            }
        }
        live.relay((m.type_id, m.timestamp, payload));
        Ok(())
    }

    async fn end_stream(&mut self, sid: u32) {
        match self.roles.remove(&sid) {
            Some(Role::Publishing(key)) => self.finish_publish(&key).await,
            Some(Role::Playing(key)) => {
                self.players.remove(&sid);
                if let Some(l) = self
                    .shared
                    .streams
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&key)
                {
                    l.players.remove(&self.id);
                }
            }
            None => {}
        }
    }

    async fn end_publishing(&mut self, stream: &str) {
        let Some((_, key)) = self.target(stream) else {
            return;
        };
        let sid = self
            .roles
            .iter()
            .find(|(_, r)| matches!(r, Role::Publishing(k) if *k == key))
            .map(|(s, _)| *s);
        if let Some(sid) = sid {
            self.end_stream(sid).await;
        }
    }

    /// The publisher left: players get StreamEOF-like silence (their relay stays open for a
    /// republish), and the handler is told what was published.
    async fn finish_publish(&mut self, key: &str) {
        let summary = {
            let mut streams = self
                .shared
                .streams
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(live) = streams.get_mut(key) else {
                return;
            };
            if live.publisher != Some(self.id) {
                return;
            }
            live.publisher = None;
            let s = std::mem::take(&mut live.stats);
            json!({
                "app": key.split('/').next(), "stream": key.split_once('/').map(|x| x.1),
                "duration_ms": s.started.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0),
                "video_messages": s.video, "audio_messages": s.audio, "keyframes": s.keyframes, "media_bytes": s.bytes,
                "players": live.players.len(),
                "has_video_header": live.video_header.is_some(), "has_audio_header": live.audio_header.is_some(),
            })
        };
        let event = Event::new(&actions::PUBLISH_ENDED_EVENT, summary);
        let _ = ask(self.shared, self.id, event, "publish-ended").await;
    }

    async fn cleanup(&mut self) {
        let sids: Vec<u32> = self.roles.keys().copied().collect();
        for sid in sids {
            self.end_stream(sid).await;
        }
    }
}
