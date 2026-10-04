//! AMQP 1.0 client over the server's codec: SASL, one session, links per address on demand.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::amqp1::frame::{self, *};
use crate::server::amqp1::message;
use crate::server::amqp1::types::{field, Value};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use crate::utils::clock::Instant;
use crate::utils::task_guard::AbortOnDrop;
pub use actions::Amqp1ClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_SASL: &str = "anonymous";
const TIMEOUT: Duration = Duration::from_secs(15);
const CHANNEL: u16 = 0;

struct Sender {
    handle: u32,
    credit: u32,
    delivery_count: u32,
}

struct Conn {
    w: WriteHalf<TcpStream>,
    frames: mpsc::Receiver<Result<Frame>>,
    peer_max_frame: u32,
    next_handle: u32,
    next_delivery: u32,
    senders: HashMap<String, Sender>,
    _reader: AbortOnDrop,
}

enum Settled {
    Outcome(Value),
    Refused(String, String),
}

impl Conn {
    async fn send(&mut self, perf: Value, payload: &[u8]) -> Result<()> {
        self.w
            .write_all(&frame::encode(TYPE_AMQP, CHANNEL, Some(&perf), payload))
            .await?;
        Ok(())
    }

    /// The next performative, skipping keep-alive frames, within `deadline`.
    async fn next(&mut self, deadline: Instant) -> Result<Value> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(!left.is_zero(), "no answer from the server in time");
            let f = tokio::time::timeout(left, self.frames.recv())
                .await
                .context("no answer from the server in time")?
                .context("the server closed the connection")??;
            if let Some(body) = f.body {
                if body.descriptor() == Some(CLOSE) {
                    let (c, d) = frame::describe_error(field(&body, 0));
                    bail!(
                        "the server closed the connection: {} {}",
                        c.unwrap_or_default(),
                        d.unwrap_or_default()
                    );
                }
                return Ok(body);
            }
        }
    }

    async fn attach(
        &mut self,
        address: &str,
        receiving: bool,
    ) -> Result<Result<u32, (String, String)>> {
        let handle = self.next_handle;
        self.next_handle += 1;
        let name = format!(
            "netget-{}-{handle}",
            if receiving { "receiver" } else { "sender" }
        );
        let terminus = |code| Value::described(code, vec![Value::str(address)]);
        let (source, target) = if receiving {
            (terminus(SOURCE), Value::described(TARGET, vec![]))
        } else {
            (Value::described(SOURCE, vec![]), terminus(TARGET))
        };
        // role false = sender, true = receiver; snd-settle-mode mixed (2), rcv-settle-mode first (0).
        let mut fields = vec![
            Value::str(&name),
            Value::Uint(handle),
            Value::Bool(receiving),
            Value::Ubyte(2),
            Value::Ubyte(0),
            source,
            target,
        ];
        if !receiving {
            fields.extend([Value::Null, Value::Null, Value::Uint(0)]);
        }
        self.send(Value::described(ATTACH, fields), &[]).await?;
        let deadline = Instant::now() + TIMEOUT;
        let mut attached = false;
        loop {
            let p = self.next(deadline).await?;
            match p.descriptor() {
                Some(ATTACH)
                    if field(&p, 1).as_u64() == Some(handle as u64)
                        || field(&p, 0).as_str() == Some(name.as_str()) =>
                {
                    attached = true;
                    let ours = if receiving {
                        field(&p, 5)
                    } else {
                        field(&p, 6)
                    };
                    // A null terminus is how a refusal starts, and its detach must follow at
                    // once (part 2, 2.6.3). Some peers (rhea's broker) also answer an accepted
                    // attach without echoing the terminus, so only a prompt detach means refusal.
                    if ours.is_null() {
                        if let Ok(Ok(next)) =
                            tokio::time::timeout(Duration::from_secs(1), self.next(deadline)).await
                        {
                            if next.descriptor() == Some(DETACH) {
                                let (c, d) = frame::describe_error(field(&next, 2));
                                return Ok(Err((
                                    c.unwrap_or_else(|| "amqp:internal-error".into()),
                                    d.unwrap_or_default(),
                                )));
                            }
                            if !receiving
                                && next.descriptor() == Some(FLOW)
                                && field(&next, 4).as_u64() == Some(handle as u64)
                            {
                                let credit = field(&next, 6).as_u64().unwrap_or(0) as u32;
                                let count = field(&next, 5).as_u64().unwrap_or(0) as u32;
                                self.senders.insert(
                                    address.to_owned(),
                                    Sender {
                                        handle,
                                        credit: count.wrapping_add(credit),
                                        delivery_count: 0,
                                    },
                                );
                                return Ok(Ok(handle));
                            }
                        }
                    }
                    if receiving {
                        return Ok(Ok(handle));
                    }
                }
                Some(DETACH) if attached => {
                    let (c, d) = frame::describe_error(field(&p, 2));
                    return Ok(Err((
                        c.unwrap_or_else(|| "amqp:internal-error".into()),
                        d.unwrap_or_default(),
                    )));
                }
                Some(FLOW)
                    if attached && !receiving && field(&p, 4).as_u64() == Some(handle as u64) =>
                {
                    let credit = field(&p, 6).as_u64().unwrap_or(0) as u32;
                    let count = field(&p, 5).as_u64().unwrap_or(0) as u32;
                    self.senders.insert(
                        address.to_owned(),
                        Sender {
                            handle,
                            credit: count.wrapping_add(credit),
                            delivery_count: 0,
                        },
                    );
                    return Ok(Ok(handle));
                }
                _ => {}
            }
        }
    }

    async fn transfer(&mut self, handle: u32, msg: &[u8]) -> Result<u32> {
        let id = self.next_delivery;
        self.next_delivery = self.next_delivery.wrapping_add(1);
        let room = (self.peer_max_frame as usize).saturating_sub(64).max(256);
        let pieces: Vec<&[u8]> = msg.chunks(room).collect();
        let n = pieces.len();
        for (i, piece) in pieces.into_iter().enumerate() {
            let more = i + 1 < n;
            let perf = if i == 0 {
                Value::described(
                    TRANSFER,
                    vec![
                        Value::Uint(handle),
                        Value::Uint(id),
                        Value::Binary(id.to_be_bytes().to_vec()),
                        Value::Uint(0),
                        Value::Bool(false),
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
                        Value::Bool(false),
                        Value::Bool(more),
                    ],
                )
            };
            self.send(perf, piece).await?;
        }
        Ok(id)
    }

    async fn send_message(&mut self, address: &str, m: &Json) -> Result<Settled> {
        let encoded = message::encode(m)?;
        if !self.senders.contains_key(address) {
            if let Err((c, d)) = self.attach(address, false).await? {
                return Ok(Settled::Refused(c, d));
            }
        }
        // Wait for credit if the link has none.
        let deadline = Instant::now() + TIMEOUT;
        while self.senders.get(address).is_some_and(|s| s.credit == 0) {
            let p = self.next(deadline).await?;
            if p.descriptor() == Some(FLOW) {
                self.on_flow(&p);
            }
        }
        let handle = {
            let s = self.senders.get_mut(address).context("link vanished")?;
            s.credit -= 1;
            s.delivery_count = s.delivery_count.wrapping_add(1);
            s.handle
        };
        let id = self.transfer(handle, &encoded).await?;
        loop {
            let p = self.next(deadline).await?;
            match p.descriptor() {
                Some(DISPOSITION) => {
                    let (first, last) = (
                        field(&p, 1).as_u64(),
                        field(&p, 2).as_u64().or(field(&p, 1).as_u64()),
                    );
                    if first.is_some_and(|f| f <= id as u64) && last.is_some_and(|l| l >= id as u64)
                    {
                        return Ok(Settled::Outcome(field(&p, 4).clone()));
                    }
                }
                Some(FLOW) => self.on_flow(&p),
                Some(DETACH) if field(&p, 0).as_u64() == Some(handle as u64) => {
                    self.senders.remove(address);
                    let (c, d) = frame::describe_error(field(&p, 2));
                    return Ok(Settled::Refused(
                        c.unwrap_or_else(|| "amqp:link:detach-forced".into()),
                        d.unwrap_or_default(),
                    ));
                }
                _ => {}
            }
        }
    }

    fn on_flow(&mut self, p: &Value) {
        let Some(handle) = field(p, 4).as_u64() else {
            return;
        };
        if let Some(s) = self
            .senders
            .values_mut()
            .find(|s| s.handle as u64 == handle)
        {
            let count = field(p, 5).as_u64().unwrap_or(0) as u32;
            let credit = field(p, 6).as_u64().unwrap_or(0) as u32;
            s.credit = count.wrapping_add(credit).wrapping_sub(s.delivery_count);
        }
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str| -> Result<Option<String>> {
        Ok(p.map(|p| p.get_optional_string(k)).transpose()?.flatten())
    };
    let sasl = s("sasl")?.unwrap_or_else(|| DEFAULT_SASL.to_owned());
    ensure!(
        matches!(sasl.as_str(), "anonymous" | "plain" | "none"),
        "sasl is anonymous, plain or none"
    );
    let (user, password) = (s("username")?, s("password")?);
    if sasl == "plain" {
        ensure!(
            user.as_deref()
                .is_some_and(|u| !u.is_empty() && u.len() <= 256)
                && password.as_deref().is_some_and(|p| p.len() <= 256),
            "sasl plain needs username and password"
        );
    }
    let hostname = s("hostname")?.unwrap_or_else(|| {
        ctx.remote_addr
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(&ctx.remote_addr)
            .to_owned()
    });
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("AMQP connect timed out")??;
    let local = stream.local_addr()?;
    if sasl != "none" {
        stream.write_all(&SASL_HEADER).await?;
        let head = frame::header(&mut stream).await?;
        ensure!(
            head == SASL_HEADER,
            "the server did not answer the SASL header"
        );
        let mechs = frame::read(&mut stream, 64 * 1024)
            .await?
            .context("closed during SASL")?;
        frame::expect(&mechs, TYPE_SASL, SASL_MECHANISMS)?;
        let init = if sasl == "plain" {
            let response = [
                &[0u8][..],
                user.as_deref().unwrap_or_default().as_bytes(),
                &[0],
                password.as_deref().unwrap_or_default().as_bytes(),
            ]
            .concat();
            Value::described(
                SASL_INIT,
                vec![Value::sym("PLAIN"), Value::Binary(response)],
            )
        } else {
            Value::described(
                SASL_INIT,
                vec![Value::sym("ANONYMOUS"), Value::Binary(b"netget".to_vec())],
            )
        };
        stream
            .write_all(&frame::encode(TYPE_SASL, 0, Some(&init), &[]))
            .await?;
        let out = frame::read(&mut stream, 64 * 1024)
            .await?
            .context("closed during SASL")?;
        let out = frame::expect(&out, TYPE_SASL, SASL_OUTCOME)?;
        let code = field(out, 0).as_u64().unwrap_or(4);
        ensure!(code == 0, "SASL authentication failed (code {code})");
    }
    stream.write_all(&AMQP_HEADER).await?;
    let head = frame::header(&mut stream).await?;
    ensure!(
        head == AMQP_HEADER,
        "the server answered with protocol header {head:?}"
    );
    let open = Value::described(
        OPEN,
        vec![
            Value::str(&format!("netget-{}", ctx.client_id)),
            Value::str(&hostname),
            Value::Uint(MAX_FRAME),
            Value::Ushort(0),
            Value::Uint(60_000),
        ],
    );
    stream
        .write_all(&frame::encode(TYPE_AMQP, 0, Some(&open), &[]))
        .await?;
    stream
        .write_all(&frame::encode(
            TYPE_AMQP,
            CHANNEL,
            Some(&Value::described(
                BEGIN,
                vec![
                    Value::Null,
                    Value::Uint(0),
                    Value::Uint(2048),
                    Value::Uint(2048),
                ],
            )),
            &[],
        ))
        .await?;
    let (mut r, w) = tokio::io::split(stream);
    let their_open = tokio::time::timeout(TIMEOUT, frame::read(&mut r, MAX_FRAME))
        .await
        .context("no open from the server")??
        .context("the server closed the connection")?;
    let their_open = frame::expect(&their_open, TYPE_AMQP, OPEN)?.clone();
    let container = field(&their_open, 0)
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let peer_max_frame = field(&their_open, 2)
        .as_u64()
        .map(|m| m.clamp(MIN_MAX_FRAME as u64, MAX_FRAME as u64) as u32)
        .unwrap_or(MAX_FRAME);
    let peer_idle = field(&their_open, 4)
        .as_u64()
        .filter(|t| *t > 0)
        .map(Duration::from_millis);
    let (frame_tx, frames) = mpsc::channel::<Result<Frame>>(256);
    let reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            let f = frame::read(&mut r, MAX_FRAME).await;
            let stop = !matches!(f, Ok(Some(_)));
            let item = match f {
                Ok(Some(f)) => Ok(f),
                Ok(None) => Err(anyhow::anyhow!("the server closed the connection")),
                Err(e) => Err(e),
            };
            if frame_tx.send(item).await.is_err() || stop {
                return;
            }
        }
    }));
    let mut conn = Conn {
        w,
        frames,
        peer_max_frame,
        next_handle: 0,
        next_delivery: 0,
        senders: HashMap::new(),
        _reader: reader,
    };
    let begin = conn.next(Instant::now() + TIMEOUT).await?;
    ensure!(
        begin.descriptor() == Some(BEGIN),
        "the server did not begin the session"
    );
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Json>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"container_id": container, "max_frame_size": peer_max_frame}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = Amqp1ClientProtocol;
        while let Some(event) = event_rx.recv().await {
            let instruction = events_ctx
                .state
                .get_instruction_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = events_ctx
                .state
                .get_memory_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &protocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("AMQP 1.0 client handler: {e}")),
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(
            &session_ctx,
            &mut conn,
            external,
            internal_rx,
            &event_tx,
            peer_idle,
        )
        .await
        {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("AMQP 1.0 client ended: {e:#}"));
        }
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
            .await;
        session_ctx
            .state
            .remove_client_handle(session_ctx.client_id)
            .await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

fn outcome_json(address: &str, s: Settled) -> Json {
    match s {
        Settled::Refused(c, d) => {
            json!({"address": address, "outcome": "refused", "condition": c, "description": d})
        }
        Settled::Outcome(v) => {
            let name = match v.descriptor() {
                Some(ACCEPTED) => "accepted",
                Some(REJECTED) => "rejected",
                Some(RELEASED) => "released",
                Some(MODIFIED) => "modified",
                _ => "settled",
            };
            let (c, d) = frame::describe_error(field(&v, 0));
            json!({"address": address, "outcome": name, "condition": c, "description": d})
        }
    }
}

async fn run(
    ctx: &ConnectContext,
    conn: &mut Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Json>,
    events: &mpsc::Sender<Event>,
    peer_idle: Option<Duration>,
) -> Result<()> {
    let mut tick = tokio::time::interval(
        peer_idle
            .map(|t| t / 2)
            .unwrap_or(Duration::from_secs(3600)),
    );
    tick.tick().await;
    loop {
        enum Wake {
            Action(Json, Option<ClientCommand>),
            Frame(Option<Result<Frame>>),
            Tick,
            Closed,
        }
        let wake = tokio::select! {
            c = external.recv() => match c { Some(c) => Wake::Action(c.action.clone(), Some(c)), None => Wake::Closed },
            a = internal.recv() => match a { Some(a) => Wake::Action(a, None), None => Wake::Closed },
            f = conn.frames.recv() => Wake::Frame(f),
            _ = tick.tick() => Wake::Tick,
        };
        let (action, command) = match wake {
            Wake::Closed | Wake::Frame(None) | Wake::Frame(Some(Err(_))) => return Ok(()),
            Wake::Tick => {
                conn.w
                    .write_all(&frame::encode(TYPE_AMQP, 0, None, &[]))
                    .await?;
                continue;
            }
            Wake::Frame(Some(Ok(f))) => {
                if let Some(p) = &f.body {
                    match p.descriptor() {
                        Some(FLOW) => conn.on_flow(p),
                        Some(CLOSE) => return Ok(()),
                        _ => {}
                    }
                }
                continue;
            }
            Wake::Action(a, c) => (a, c),
        };
        let outcome = match Amqp1ClientProtocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(ClientActionResult::Disconnect) => {
                let _ = conn.send(Value::described(CLOSE, vec![]), &[]).await;
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => {
                let address = action["address"].as_str().unwrap_or_default().to_owned();
                let result = if action["type"] == "amqp1_send" {
                    conn.send_message(&address, &action["message"])
                        .await
                        .map(|s| Event::new(&actions::OUTCOME_EVENT, outcome_json(&address, s)))
                } else {
                    let count = action["count"].as_u64().unwrap_or(1) as u32;
                    let wait =
                        Duration::from_secs_f64(action["timeout_secs"].as_f64().unwrap_or(5.0));
                    receive_with_payloads(conn, &address, count, wait)
                        .await
                        .map(|j| Event::new(&actions::MESSAGES_EVENT, j))
                };
                match result {
                    Ok(event) => {
                        events.send(event).await.ok();
                        Ok(ClientSendOutcome::Sent { bytes_sent: 0 })
                    }
                    Err(e) => Err(e),
                }
            }
        };
        if let Some(c) = command {
            let logged = outcome
                .as_ref()
                .map(|o| serde_json::to_value(o).unwrap_or(Json::Null))
                .unwrap_or_else(|e| json!({"error": e.to_string()}));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "AMQP1",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "address": action["address"]}),
                    vec![logged],
                )
                .await;
            crate::client::command_support::reply(c, outcome);
        } else if let Err(e) = outcome {
            Log::new(Some(&ctx.status_tx)).warn(format!("AMQP 1.0 action failed: {e:#}"));
        }
    }
}

/// amqp1_receive, reading transfers with their payloads straight off the frame channel.
async fn receive_with_payloads(
    conn: &mut Conn,
    address: &str,
    count: u32,
    wait: Duration,
) -> Result<Json> {
    let handle = match conn.attach(address, true).await? {
        Ok(h) => h,
        Err((c, d)) => {
            return Ok(json!({"address": address, "messages": [], "error": format!("{c}: {d}")}))
        }
    };
    conn.send(
        Value::described(
            FLOW,
            vec![
                Value::Uint(0),
                Value::Uint(2048),
                Value::Uint(conn.next_delivery),
                Value::Uint(2048),
                Value::Uint(handle),
                Value::Uint(0),
                Value::Uint(count),
            ],
        ),
        &[],
    )
    .await?;
    let mut messages = Vec::new();
    let mut buffer: Vec<u8> = Vec::new();
    let mut current: Option<(u32, bool)> = None;
    let mut error = None;
    let deadline = Instant::now() + wait;
    while (messages.len() as u32) < count {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let f = match tokio::time::timeout(left, conn.frames.recv()).await {
            Err(_) => break,
            Ok(None) => bail!("the server closed the connection"),
            Ok(Some(f)) => f?,
        };
        let Some(p) = f.body else { continue };
        match p.descriptor() {
            Some(TRANSFER) if field(&p, 0).as_u64() == Some(handle as u64) => {
                if current.is_none() {
                    current = Some((
                        field(&p, 1).as_u64().unwrap_or(0) as u32,
                        field(&p, 4).as_bool().unwrap_or(false),
                    ));
                }
                ensure!(
                    buffer.len() + f.payload.len() <= message::MAX_MESSAGE,
                    "message over the bound"
                );
                buffer.extend(&f.payload);
                if field(&p, 9).as_bool() == Some(true) {
                    buffer.clear();
                    current = None;
                    continue;
                }
                if field(&p, 5).as_bool() == Some(true) {
                    continue;
                }
                let (id, settled) = current.take().unwrap_or((0, true));
                messages.push(
                    message::decode(&std::mem::take(&mut buffer))
                        .unwrap_or_else(|e| json!({"undecodable": e.to_string()})),
                );
                if !settled {
                    conn.send(
                        Value::described(
                            DISPOSITION,
                            vec![
                                Value::Bool(true),
                                Value::Uint(id),
                                Value::Uint(id),
                                Value::Bool(true),
                                Value::described(ACCEPTED, vec![]),
                            ],
                        ),
                        &[],
                    )
                    .await?;
                }
            }
            Some(DETACH) if field(&p, 0).as_u64() == Some(handle as u64) => {
                let (c, d) = frame::describe_error(field(&p, 2));
                error = Some(format!(
                    "{}: {}",
                    c.unwrap_or_default(),
                    d.unwrap_or_default()
                ));
                break;
            }
            Some(FLOW) => conn.on_flow(&p),
            Some(CLOSE) => bail!("the server closed the connection"),
            _ => {}
        }
    }
    let _ = conn
        .send(
            Value::described(DETACH, vec![Value::Uint(handle), Value::Bool(true)]),
            &[],
        )
        .await;
    let mut out = json!({"address": address, "messages": messages});
    if let Some(e) = error {
        out["error"] = json!(e);
    }
    Ok(out)
}
