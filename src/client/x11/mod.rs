//! X11 client: one connection to an X server (TCP or a Unix socket). The model creates and
//! manages windows and reads and writes properties; each action is executed whole — interning
//! the atoms it needs, sending its requests, then a GetInputFocus round trip that proves the
//! server has processed them — and answered with one `x11_result` or `x11_error`.
pub mod actions;
pub mod wire;

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event, EventType};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::X11ClientProtocol;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use wire::Packet;

/// The screen whose root window "root" means.
pub const DEFAULT_SCREEN: u64 = 0;
/// How long the connection and setup may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How many model turns may follow from one another before the chain stops.
pub const MAX_FOLLOWUP_DEPTH: u32 = 8;
/// Children whose titles query_tree reads, and properties list_properties names.
pub const MAX_LISTED: usize = 256;
const TURN_QUEUE: usize = 128;

type Reader = Box<dyn AsyncRead + Send + Unpin>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

pub fn hex_id(id: u32) -> String {
    format!("0x{id:x}")
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let get = |k: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten())
    };
    let num = |k: &str| -> Result<Option<u64>> {
        Ok(params.map(|p| p.get_optional_u64(k)).transpose()?.flatten())
    };
    let screen_index = num("screen")?.unwrap_or(DEFAULT_SCREEN) as usize;
    let timeout_ms = num("timeout_ms")?.unwrap_or(actions::DEFAULT_TIMEOUT_MS);
    ensure!(
        (100..=60_000).contains(&timeout_ms),
        "timeout_ms {timeout_ms} is outside 100-60000"
    );
    let cookie = match get("auth_cookie")? {
        Some(h) => {
            let bytes = hex::decode(h.trim())
                .context("auth_cookie must be hex, as `xauth list` prints it")?;
            ensure!(
                !bytes.is_empty() && bytes.len() <= 255,
                "auth_cookie is 1-255 bytes"
            );
            Some(bytes)
        }
        None => None,
    };
    let (reader, mut writer, local): (Reader, Writer, SocketAddr) =
        tokio::time::timeout(CONNECT_TIMEOUT, open(&ctx.remote_addr, get("socket_path")?))
            .await
            .context("X11 connect deadline")??;
    let mut reader = reader;
    writer
        .write_all(&wire::setup_request(cookie.as_deref()))
        .await?;
    let setup = tokio::time::timeout(CONNECT_TIMEOUT, wire::read_setup(&mut reader))
        .await
        .context("the X server did not answer the setup within 10 s")??;
    let screen = setup.screens.get(screen_index).cloned().with_context(|| {
        format!(
            "screen {screen_index} does not exist; the server has {}",
            setup.screens.len()
        )
    })?;
    ensure!(
        setup.resource_id_mask != 0,
        "the X server granted no resource ids"
    );
    Log::new(Some(&ctx.status_tx)).info(format!(
        "X11 client {} connected: {} release {}, screen {screen_index} {}x{}",
        ctx.client_id, setup.vendor, setup.release, screen.width, screen.height
    ));
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;

    let (packet_tx, packets) = mpsc::channel::<Result<Packet>>(256);
    let reader_task = tokio::spawn(async move {
        loop {
            let item = wire::read_packet(&mut reader).await;
            let end = !matches!(item, Ok(Some(_)));
            let sent = match item {
                Ok(Some(p)) => packet_tx.send(Ok(p)).await,
                Ok(None) => {
                    packet_tx
                        .send(Err(anyhow!("the X server closed the connection")))
                        .await
                }
                Err(e) => packet_tx.send(Err(e)).await,
            };
            if end || sent.is_err() {
                return;
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;

    let (event_tx, event_rx) = mpsc::channel::<(Event, u32)>(TURN_QUEUE);
    let (internal_tx, internal) = mpsc::channel::<(Value, u32)>(64);
    let _ = event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({
                "vendor": setup.vendor,
                "release": setup.release,
                "protocol_version": format!("{}.{}", setup.protocol_major, setup.protocol_minor),
                "screen": {
                    "index": screen_index,
                    "root": hex_id(screen.root),
                    "width": screen.width,
                    "height": screen.height,
                    "width_mm": screen.width_mm,
                    "height_mm": screen.height_mm,
                    "root_depth": screen.root_depth,
                },
                "screens": setup.screens.len(),
            }),
        ),
        0,
    ));
    let dispatcher = tokio::spawn(run_turns(ctx.clone(), event_rx, internal_tx));
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    let mut session = Session {
        writer,
        packets,
        seq: 0,
        max_request_bytes: setup.max_request_bytes,
        id_base: setup.resource_id_base,
        id_mask: setup.resource_id_mask,
        next_id: 1,
        screen,
        atoms: HashMap::new(),
        names: HashMap::new(),
        expects_reply: HashSet::new(),
        stashed: HashMap::new(),
        stray_errors: Vec::new(),
        events: event_tx,
        depth: 0,
        timeout: Duration::from_millis(timeout_ms),
        client_id: ctx.client_id,
    };
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session.run(&session_ctx, external, internal).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("X11 client ended: {e:#}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, status)
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

async fn open(remote: &str, socket_path: Option<String>) -> Result<(Reader, Writer, SocketAddr)> {
    if let Some(path) = socket_path {
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(&path)
                .await
                .with_context(|| format!("cannot connect to the X server socket {path}"))?;
            let (r, w) = tokio::io::split(stream);
            return Ok((
                Box::new(r),
                Box::new(w),
                SocketAddr::from(([0, 0, 0, 0], 0)),
            ));
        }
        #[cfg(not(unix))]
        bail!("socket_path {path} needs a Unix platform");
    }
    ensure!(
        !remote.trim().is_empty(),
        "the X11 client needs remote_addr (host:port, port 6000+display) or socket_path"
    );
    let stream = tokio::net::TcpStream::connect(remote)
        .await
        .with_context(|| format!("cannot connect to the X server at {remote}"))?;
    let local = stream.local_addr()?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w), local))
}

/// Ask the model about each event in turn. An event `depth` turns removed from anything the
/// operator or the server started is not asked about past [`MAX_FOLLOWUP_DEPTH`].
async fn run_turns(
    ctx: ConnectContext,
    mut events: mpsc::Receiver<(Event, u32)>,
    internal: mpsc::Sender<(Value, u32)>,
) {
    let protocol = X11ClientProtocol;
    while let Some((event, depth)) = events.recv().await {
        if depth >= MAX_FOLLOWUP_DEPTH {
            tracing::warn!(
                "X11 client {} not asking the model about {}: {depth} turns deep decision=followup_depth",
                ctx.client_id,
                event.id()
            );
            continue;
        }
        let instruction = ctx
            .state
            .get_instruction_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        let memory = ctx
            .state
            .get_memory_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        match call_llm_for_client(
            &ctx.llm_client,
            &ctx.state,
            ctx.client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &protocol,
            &ctx.status_tx,
        )
        .await
        {
            Ok(result) => {
                if let Some(memory) = result.memory_updates {
                    ctx.state.set_memory_for_client(ctx.client_id, memory).await;
                }
                for action in result.actions {
                    if internal.send((action, depth + 1)).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => Log::new(Some(&ctx.status_tx)).warn(format!("X11 client handler: {e}")),
        }
    }
}

#[derive(Debug, Clone)]
struct XError {
    code: u8,
    bad_value: u32,
    major: u8,
}

/// What an action came to: a result, or a refusal (X's or NetGet's) for the model.
type Outcome = std::result::Result<Value, Value>;

struct Session {
    writer: Writer,
    packets: mpsc::Receiver<Result<Packet>>,
    /// The sequence number of the last request sent.
    seq: u16,
    max_request_bytes: usize,
    id_base: u32,
    id_mask: u32,
    next_id: u32,
    screen: wire::Screen,
    atoms: HashMap<String, u32>,
    names: HashMap<u32, String>,
    /// Requests that will be answered by a reply (or an error in its place).
    expects_reply: HashSet<u16>,
    /// Replies that arrived while another was being waited for.
    stashed: HashMap<u16, std::result::Result<Vec<u8>, XError>>,
    /// Errors for requests that have no reply, collected until the action's sync.
    stray_errors: Vec<(u16, XError)>,
    events: mpsc::Sender<(Event, u32)>,
    /// The follow-up depth of the action being executed, which its events inherit.
    depth: u32,
    timeout: Duration,
    client_id: crate::state::ClientId,
}

impl Session {
    async fn run(
        &mut self,
        ctx: &ConnectContext,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<(Value, u32)>,
    ) -> Result<()> {
        loop {
            let (action, depth, mut injected) = tokio::select! {
                packet = self.packets.recv() => {
                    let packet = packet.context("the reader stopped")??;
                    self.idle_packet(packet);
                    continue;
                }
                command = external.recv() => match command {
                    Some(c) => (c.action.clone(), 0, Some(c)),
                    None => return Ok(()),
                },
                action = internal.recv() => match action {
                    Some((a, d)) => (a, d, None),
                    None => return Ok(()),
                },
            };
            let reply = |command: Option<ClientCommand>, outcome: ClientSendOutcome| {
                if let Some(command) = command {
                    crate::client::command_support::reply(command, Ok(outcome));
                }
            };
            match X11ClientProtocol.execute_action(action.clone()) {
                Ok(ClientActionResult::Disconnect) => {
                    reply(injected.take(), ClientSendOutcome::Disconnected);
                    let _ = self.writer.shutdown().await;
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    let name = action["type"].as_str().unwrap_or("unknown").to_string();
                    self.emit(
                        &actions::ERROR_EVENT,
                        json!({"action": name, "error": e.to_string()}),
                        depth,
                    );
                    reply(
                        injected.take(),
                        ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                    );
                    continue;
                }
            }
            if injected.is_some() {
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "X11",
                        None,
                        "injected_action",
                        action.clone(),
                        vec![],
                    )
                    .await;
            }
            self.depth = depth;
            let outcome = tokio::time::timeout(self.timeout, self.perform(&action))
                .await
                .map_err(|_| {
                    anyhow!(
                        "the X server did not answer within {} ms",
                        self.timeout.as_millis()
                    )
                })??;
            let (event_type, data) = match outcome {
                Ok(v) => (&*actions::RESULT_EVENT, v),
                Err(v) => (&*actions::ERROR_EVENT, v),
            };
            reply(
                injected.take(),
                ClientSendOutcome::Executed {
                    detail: data.to_string(),
                },
            );
            self.emit(event_type, data, depth);
        }
    }

    fn emit(&self, event_type: &'static EventType, data: Value, depth: u32) {
        if self
            .events
            .try_send((Event::new(event_type, data), depth))
            .is_err()
        {
            tracing::warn!(
                "X11 client {} dropped an event: the model is {TURN_QUEUE} events behind decision=turn_queue_full",
                self.client_id
            );
        }
    }

    /// A packet that arrived while no action was waiting.
    fn idle_packet(&mut self, packet: Packet) {
        match packet {
            Packet::Event {
                code,
                synthetic,
                body,
            } => self.x_event(code, synthetic, &body),
            Packet::Error {
                code,
                bad_value,
                major,
                ..
            } => {
                let data = error_payload(
                    "unknown",
                    &XError {
                        code,
                        bad_value,
                        major,
                    },
                );
                self.emit(&actions::ERROR_EVENT, data, self.depth);
            }
            Packet::Reply { seq, .. } => {
                tracing::debug!(
                    "X11 client {} ignored an unexpected reply to request {seq}",
                    self.client_id
                )
            }
        }
    }

    fn x_event(&mut self, code: u8, synthetic: bool, b: &[u8]) {
        let mut data = json!({"event": wire::event_name(code), "synthetic": synthetic});
        let w = |at: usize| json!(hex_id(wire::u32_at(b, at)));
        match code {
            2..=6 => {
                data["window"] = w(12);
                data["detail"] = json!(b[1]);
                data["x"] = json!(wire::i16_at(b, 24));
                data["y"] = json!(wire::i16_at(b, 26));
                data["state"] = json!(wire::u16_at(b, 28));
            }
            9 | 10 => data["window"] = w(4),
            12 => {
                data["window"] = w(4);
                data["x"] = json!(wire::u16_at(b, 8));
                data["y"] = json!(wire::u16_at(b, 10));
                data["width"] = json!(wire::u16_at(b, 12));
                data["height"] = json!(wire::u16_at(b, 14));
                data["count"] = json!(wire::u16_at(b, 16));
            }
            17..=19 => data["window"] = w(8),
            22 => {
                data["window"] = w(8);
                data["x"] = json!(wire::i16_at(b, 16));
                data["y"] = json!(wire::i16_at(b, 18));
                data["width"] = json!(wire::u16_at(b, 20));
                data["height"] = json!(wire::u16_at(b, 22));
                data["border_width"] = json!(wire::u16_at(b, 24));
            }
            28 => {
                data["window"] = w(4);
                let atom = wire::u32_at(b, 8);
                data["property"] = match self.known_name(atom) {
                    Some(n) => json!(n),
                    None => json!(atom),
                };
                data["state"] = json!(if b[16] == 0 { "new_value" } else { "deleted" });
            }
            _ => {}
        }
        self.emit(&actions::X_EVENT, data, self.depth.saturating_add(1));
    }

    fn known_name(&self, atom: u32) -> Option<String> {
        wire::predefined_name(atom)
            .map(str::to_string)
            .or_else(|| self.names.get(&atom).cloned())
    }

    fn alloc_id(&mut self) -> Result<u32> {
        let step = self.id_mask & self.id_mask.wrapping_neg();
        let id = self
            .next_id
            .checked_mul(step)
            .filter(|v| v & !self.id_mask == 0);
        let id = id.context("this connection has used every resource id the server granted")?;
        self.next_id += 1;
        Ok(self.id_base | id)
    }

    async fn send(&mut self, opcode: u8, data: u8, body: &[u8], reply: bool) -> Result<u16> {
        let bytes = wire::request(opcode, data, body, self.max_request_bytes)?;
        self.writer.write_all(&bytes).await?;
        self.seq = self.seq.wrapping_add(1);
        if reply {
            self.expects_reply.insert(self.seq);
        }
        Ok(self.seq)
    }

    async fn next_packet(&mut self) -> Result<Packet> {
        self.packets.recv().await.context("the reader stopped")?
    }

    /// The reply to request `seq`, or the error the server sent in its place.
    async fn wait(&mut self, seq: u16) -> Result<std::result::Result<Vec<u8>, XError>> {
        loop {
            if let Some(answer) = self.stashed.remove(&seq) {
                self.expects_reply.remove(&seq);
                return Ok(answer);
            }
            match self.next_packet().await? {
                Packet::Reply { seq: s, body } => {
                    self.stashed.insert(s, Ok(body));
                }
                Packet::Error {
                    seq: s,
                    code,
                    bad_value,
                    major,
                    ..
                } => {
                    let e = XError {
                        code,
                        bad_value,
                        major,
                    };
                    if self.expects_reply.contains(&s) {
                        self.stashed.insert(s, Err(e));
                    } else {
                        self.stray_errors.push((s, e));
                    }
                }
                Packet::Event {
                    code,
                    synthetic,
                    body,
                } => self.x_event(code, synthetic, &body),
            }
        }
    }

    /// Confirm every request since `first` was processed: the first error any of them drew.
    async fn sync(&mut self, first: u16) -> Result<Option<XError>> {
        let sync = self.send(wire::GET_INPUT_FOCUS, 0, &[], true).await?;
        let _ = self.wait(sync).await?;
        let span = sync.wrapping_sub(first);
        let errors = std::mem::take(&mut self.stray_errors);
        Ok(errors
            .into_iter()
            .find(|(s, _)| s.wrapping_sub(first) < span)
            .map(|(_, e)| e))
    }

    async fn atom(
        &mut self,
        name: &str,
        only_if_exists: bool,
    ) -> Result<std::result::Result<Option<u32>, XError>> {
        if let Some(a) = wire::predefined_atom(name).or_else(|| self.atoms.get(name).copied()) {
            return Ok(Ok(Some(a)));
        }
        let seq = self
            .send(
                wire::INTERN_ATOM,
                u8::from(only_if_exists),
                &wire::intern_atom(name),
                true,
            )
            .await?;
        Ok(match self.wait(seq).await? {
            Ok(body) => {
                let atom = wire::u32_at(&body, 8);
                if atom != 0 {
                    self.atoms.insert(name.to_string(), atom);
                    self.names.insert(atom, name.to_string());
                }
                Ok((atom != 0).then_some(atom))
            }
            Err(e) => Err(e),
        })
    }

    async fn atom_name(&mut self, atom: u32) -> Result<String> {
        if let Some(n) = self.known_name(atom) {
            return Ok(n);
        }
        let seq = self
            .send(wire::GET_ATOM_NAME, 0, &wire::u32s(&[atom]), true)
            .await?;
        Ok(match self.wait(seq).await? {
            Ok(body) => {
                let len = wire::u16_at(&body, 8) as usize;
                let name = crate::utils::sanitize::line_field(&String::from_utf8_lossy(
                    &wire::tail(&body)[..len.min(wire::tail(&body).len())],
                ));
                self.names.insert(atom, name.clone());
                self.atoms.insert(name.clone(), atom);
                name
            }
            Err(_) => atom.to_string(),
        })
    }

    fn window(&self, v: &Value) -> Result<u32> {
        actions::window_id(v, self.screen.root)
    }

    async fn perform(&mut self, a: &Value) -> Result<Outcome> {
        let name = a["type"].as_str().unwrap_or_default().to_string();
        let refuse = |message: String| Ok(Err(json!({"action": name, "error": message})));
        macro_rules! xtry {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(error_payload(&name, &e))),
                }
            };
        }
        let window = match a.get("window") {
            Some(w) => self.window(w)?,
            None => self.screen.root,
        };
        match name.as_str() {
            "x11_create_window" => {
                let parent = match a.get("parent") {
                    Some(p) => self.window(p)?,
                    None => self.screen.root,
                };
                let mut mask = 0;
                for item in a["watch"].as_array().into_iter().flatten() {
                    let n = item.as_str().unwrap_or_default();
                    mask |= wire::EVENT_MASKS
                        .iter()
                        .find(|(k, _)| *k == n)
                        .map_or(0, |(_, m)| *m);
                }
                let background = if a["background"] == "black" {
                    self.screen.black_pixel
                } else {
                    self.screen.white_pixel
                };
                let (w, h) = (
                    a["width"].as_u64().unwrap_or(1) as u16,
                    a["height"].as_u64().unwrap_or(1) as u16,
                );
                let (x, y) = (
                    a["x"].as_i64().unwrap_or(0) as i16,
                    a["y"].as_i64().unwrap_or(0) as i16,
                );
                let wid = self.alloc_id()?;
                let first = self
                    .send(
                        wire::CREATE_WINDOW,
                        0,
                        &wire::create_window(wid, parent, x, y, w, h, 0, background, mask),
                        false,
                    )
                    .await?;
                if let Some(title) = a["title"].as_str() {
                    let latin1: Vec<u8> = title
                        .chars()
                        .map(|c| u8::try_from(c as u32).unwrap_or(b'?'))
                        .collect();
                    self.send(
                        wire::CHANGE_PROPERTY,
                        0,
                        &wire::change_property(wid, 39, 31, 8, &latin1),
                        false,
                    )
                    .await?;
                    let net = xtry!(self.atom("_NET_WM_NAME", false).await?).unwrap_or(0);
                    let utf8 = xtry!(self.atom("UTF8_STRING", false).await?).unwrap_or(0);
                    self.send(
                        wire::CHANGE_PROPERTY,
                        0,
                        &wire::change_property(wid, net, utf8, 8, title.as_bytes()),
                        false,
                    )
                    .await?;
                }
                let map = a["map"].as_bool().unwrap_or(true);
                if map {
                    self.send(wire::MAP_WINDOW, 0, &wire::u32s(&[wid]), false)
                        .await?;
                }
                if let Some(e) = self.sync(first).await? {
                    return Ok(Err(error_payload(&name, &e)));
                }
                Ok(Ok(json!({
                    "action": name, "window": hex_id(wid), "parent": hex_id(parent),
                    "x": x, "y": y, "width": w, "height": h, "title": a["title"], "mapped": map,
                })))
            }
            "x11_set_property" => {
                let property = a["property"].as_str().unwrap_or_default();
                let kind = a
                    .get("property_type")
                    .and_then(Value::as_str)
                    .unwrap_or("UTF8_STRING");
                let (format, data) = match self.encode_value(kind, &a["value"]).await? {
                    Ok(Ok(v)) => v,
                    Ok(Err(message)) => return refuse(message),
                    Err(e) => return Ok(Err(error_payload(&name, &e))),
                };
                if data.len() > wire::MAX_PROPERTY_BYTES {
                    return refuse(format!(
                        "the value is {} bytes; the bound is {}",
                        data.len(),
                        wire::MAX_PROPERTY_BYTES
                    ));
                }
                let prop = xtry!(self.atom(property, false).await?).unwrap_or(0);
                let type_atom = xtry!(self.atom(kind, false).await?).unwrap_or(0);
                let mode = match a["mode"].as_str() {
                    Some("prepend") => 1,
                    Some("append") => 2,
                    _ => 0,
                };
                let first = self
                    .send(
                        wire::CHANGE_PROPERTY,
                        mode,
                        &wire::change_property(window, prop, type_atom, format, &data),
                        false,
                    )
                    .await?;
                if let Some(e) = self.sync(first).await? {
                    return Ok(Err(error_payload(&name, &e)));
                }
                Ok(Ok(
                    json!({"action": name, "window": hex_id(window), "property": property, "property_type": kind, "bytes": data.len()}),
                ))
            }
            "x11_get_property" => {
                let property = a["property"].as_str().unwrap_or_default();
                let base = json!({"action": name, "window": hex_id(window), "property": property});
                let Some(prop) = xtry!(self.atom(property, true).await?) else {
                    return Ok(Ok(merge(base, json!({"exists": false}))));
                };
                let seq = self
                    .send(
                        wire::GET_PROPERTY,
                        0,
                        &wire::get_property(window, prop),
                        true,
                    )
                    .await?;
                let body = xtry!(self.wait(seq).await?);
                let type_atom = wire::u32_at(&body, 8);
                if type_atom == 0 {
                    return Ok(Ok(merge(base, json!({"exists": false}))));
                }
                let format = body[1];
                let units = wire::u32_at(&body, 16) as usize;
                let bytes = units
                    .saturating_mul(format as usize / 8)
                    .min(wire::tail(&body).len());
                let raw = wire::tail(&body)[..bytes].to_vec();
                let kind = self.atom_name(type_atom).await?;
                let value = self.decode_value(&kind, format, &raw).await?;
                Ok(Ok(merge(
                    base,
                    json!({"exists": true, "property_type": kind, "format": format, "value": value, "truncated": wire::u32_at(&body, 12) > 0}),
                )))
            }
            "x11_delete_property" => {
                let property = a["property"].as_str().unwrap_or_default();
                let Some(prop) = xtry!(self.atom(property, true).await?) else {
                    return Ok(Ok(
                        json!({"action": name, "window": hex_id(window), "property": property, "existed": false}),
                    ));
                };
                let first = self
                    .send(
                        wire::DELETE_PROPERTY,
                        0,
                        &wire::u32s(&[window, prop]),
                        false,
                    )
                    .await?;
                if let Some(e) = self.sync(first).await? {
                    return Ok(Err(error_payload(&name, &e)));
                }
                Ok(Ok(
                    json!({"action": name, "window": hex_id(window), "property": property}),
                ))
            }
            "x11_list_properties" => {
                let seq = self
                    .send(wire::LIST_PROPERTIES, 0, &wire::u32s(&[window]), true)
                    .await?;
                let body = xtry!(self.wait(seq).await?);
                let n = wire::u16_at(&body, 8) as usize;
                let tail = wire::tail(&body);
                let mut names = Vec::new();
                for i in 0..n.min(MAX_LISTED).min(tail.len() / 4) {
                    let atom = wire::u32_at(tail, i * 4);
                    names.push(self.atom_name(atom).await?);
                }
                Ok(Ok(
                    json!({"action": name, "window": hex_id(window), "properties": names, "count": n}),
                ))
            }
            "x11_query_tree" => {
                let seq = self
                    .send(wire::QUERY_TREE, 0, &wire::u32s(&[window]), true)
                    .await?;
                let body = xtry!(self.wait(seq).await?);
                let n = wire::u16_at(&body, 16) as usize;
                let tail = wire::tail(&body);
                let ids: Vec<u32> = (0..n.min(MAX_LISTED).min(tail.len() / 4))
                    .map(|i| wire::u32_at(tail, i * 4))
                    .collect();
                // One GetProperty(WM_NAME) per child, pipelined, then the replies in order.
                let mut seqs = Vec::with_capacity(ids.len());
                for id in &ids {
                    seqs.push(
                        self.send(wire::GET_PROPERTY, 0, &wire::get_property(*id, 39), true)
                            .await?,
                    );
                }
                let mut children = Vec::with_capacity(ids.len());
                for (id, seq) in ids.iter().zip(seqs) {
                    let title = match self.wait(seq).await? {
                        Ok(b) if wire::u32_at(&b, 8) != 0 && b[1] == 8 => {
                            let len = (wire::u32_at(&b, 16) as usize).min(wire::tail(&b).len());
                            json!(text(&wire::tail(&b)[..len], true))
                        }
                        _ => Value::Null,
                    };
                    children.push(json!({"window": hex_id(*id), "title": title}));
                }
                let parent = wire::u32_at(&body, 12);
                Ok(Ok(json!({
                    "action": name, "window": hex_id(window),
                    "parent": if parent == 0 { Value::Null } else { json!(hex_id(parent)) },
                    "children": children, "count": n,
                })))
            }
            "x11_get_geometry" => {
                let seq = self
                    .send(wire::GET_GEOMETRY, 0, &wire::u32s(&[window]), true)
                    .await?;
                let b = xtry!(self.wait(seq).await?);
                Ok(Ok(json!({
                    "action": name, "window": hex_id(window), "depth": b[1],
                    "x": wire::i16_at(&b, 12), "y": wire::i16_at(&b, 14),
                    "width": wire::u16_at(&b, 16), "height": wire::u16_at(&b, 18),
                    "border_width": wire::u16_at(&b, 20),
                })))
            }
            "x11_configure_window" => {
                let mut values = Vec::new();
                for (key, bit) in [("x", 0x1u16), ("y", 0x2), ("width", 0x4), ("height", 0x8)] {
                    if let Some(n) = a[key].as_i64() {
                        values.push((bit, n as i32 as u32));
                    }
                }
                if a["raise"] == true {
                    values.push((0x40, 0)); // stack-mode Above
                }
                if values.is_empty() {
                    return refuse("give at least one of x, y, width, height or raise".into());
                }
                let first = self
                    .send(
                        wire::CONFIGURE_WINDOW,
                        0,
                        &wire::configure_window(window, &values),
                        false,
                    )
                    .await?;
                if let Some(e) = self.sync(first).await? {
                    return Ok(Err(error_payload(&name, &e)));
                }
                Ok(Ok(json!({"action": name, "window": hex_id(window)})))
            }
            "x11_map_window" | "x11_unmap_window" | "x11_destroy_window" => {
                let opcode = match name.as_str() {
                    "x11_map_window" => wire::MAP_WINDOW,
                    "x11_unmap_window" => wire::UNMAP_WINDOW,
                    _ => wire::DESTROY_WINDOW,
                };
                let first = self.send(opcode, 0, &wire::u32s(&[window]), false).await?;
                if let Some(e) = self.sync(first).await? {
                    return Ok(Err(error_payload(&name, &e)));
                }
                Ok(Ok(json!({"action": name, "window": hex_id(window)})))
            }
            "x11_intern_atom" => {
                let atom_name = a["name"].as_str().unwrap_or_default();
                let only = a["only_if_exists"] == true;
                let atom = xtry!(self.atom(atom_name, only).await?);
                Ok(Ok(
                    json!({"action": name, "name": atom_name, "atom": atom, "exists": atom.is_some()}),
                ))
            }
            "x11_list_extensions" => {
                let seq = self.send(wire::LIST_EXTENSIONS, 0, &[], true).await?;
                let b = xtry!(self.wait(seq).await?);
                let names: Vec<String> = wire::strs(b[1] as usize, wire::tail(&b))
                    .iter()
                    .map(|s| crate::utils::sanitize::line_field(s))
                    .collect();
                Ok(Ok(json!({"action": name, "extensions": names})))
            }
            "x11_bell" => {
                let percent = a["percent"].as_i64().unwrap_or(0) as i8;
                let first = self.send(wire::BELL, percent as u8, &[], false).await?;
                if let Some(e) = self.sync(first).await? {
                    return Ok(Err(error_payload(&name, &e)));
                }
                Ok(Ok(json!({"action": name, "percent": percent})))
            }
            other => bail!("no executor for {other}"),
        }
    }

    /// A property value as the model wrote it, in X's encoding: `(format, bytes)`.
    async fn encode_value(
        &mut self,
        kind: &str,
        value: &Value,
    ) -> Result<std::result::Result<std::result::Result<(u8, Vec<u8>), String>, XError>> {
        let items: Vec<&Value> = match value {
            Value::Array(list) => list.iter().collect(),
            v => vec![v],
        };
        let multiple = value.is_array();
        Ok(Ok(Ok(match kind {
            "UTF8_STRING" | "STRING" => {
                let mut out = Vec::new();
                for item in &items {
                    let Some(s) = item.as_str() else {
                        return Ok(Ok(Err(format!(
                            "a {kind} value is a string or an array of strings"
                        ))));
                    };
                    if kind == "STRING" {
                        for c in s.chars() {
                            match u8::try_from(c as u32) {
                                Ok(b) => out.push(b),
                                Err(_) => {
                                    return Ok(Ok(Err(format!(
                                        "{c:?} is not Latin-1; use UTF8_STRING"
                                    ))))
                                }
                            }
                        }
                    } else {
                        out.extend(s.as_bytes());
                    }
                    if multiple {
                        out.push(0);
                    }
                }
                (8, out)
            }
            "CARDINAL" | "INTEGER" => {
                let mut out = Vec::new();
                for item in &items {
                    let n = if kind == "CARDINAL" {
                        item.as_u64().and_then(|n| u32::try_from(n).ok())
                    } else {
                        item.as_i64()
                            .and_then(|n| i32::try_from(n).ok())
                            .map(|n| n as u32)
                    };
                    let Some(n) = n else {
                        return Ok(Ok(Err(format!("{item} is not a 32-bit {kind}"))));
                    };
                    out.extend(n.to_le_bytes());
                }
                (32, out)
            }
            "ATOM" => {
                let mut out = Vec::new();
                for item in &items {
                    let Some(s) = item.as_str() else {
                        return Ok(Ok(Err(
                            "an ATOM value is an atom name or an array of them".into()
                        )));
                    };
                    match self.atom(s, false).await? {
                        Ok(a) => out.extend(a.unwrap_or(0).to_le_bytes()),
                        Err(e) => return Ok(Err(e)),
                    }
                }
                (32, out)
            }
            "WINDOW" => {
                let mut out = Vec::new();
                for item in &items {
                    match self.window(item) {
                        Ok(w) => out.extend(w.to_le_bytes()),
                        Err(e) => return Ok(Ok(Err(e.to_string()))),
                    }
                }
                (32, out)
            }
            other => {
                return Ok(Ok(Err(format!(
                    "property_type {other} is not one NetGet writes"
                ))))
            }
        })))
    }

    /// A property's bytes as the model reads them.
    async fn decode_value(&mut self, kind: &str, format: u8, raw: &[u8]) -> Result<Value> {
        let words =
            |raw: &[u8]| -> Vec<u32> { raw.chunks_exact(4).map(|c| wire::u32_at(c, 0)).collect() };
        Ok(match (format, kind) {
            (8, "STRING") => strings(raw, false),
            (8, _) => match std::str::from_utf8(raw) {
                Ok(_) => strings(raw, true),
                Err(_) => json!({"hex": hex::encode(raw)}),
            },
            (32, "INTEGER") => json!(words(raw).iter().map(|w| *w as i32).collect::<Vec<_>>()),
            (32, "ATOM") => {
                let mut names = Vec::new();
                for atom in words(raw).into_iter().take(64) {
                    names.push(self.atom_name(atom).await?);
                }
                json!(names)
            }
            (32, "WINDOW") | (32, "DRAWABLE") | (32, "PIXMAP") => {
                json!(words(raw).into_iter().map(hex_id).collect::<Vec<_>>())
            }
            (32, _) => json!(words(raw)),
            (16, _) => json!(raw
                .chunks_exact(2)
                .map(|c| wire::u16_at(c, 0))
                .collect::<Vec<_>>()),
            _ => json!({"hex": hex::encode(raw)}),
        })
    }
}

/// Text from a property: Latin-1 or UTF-8, one string, or an array where NULs separate several.
fn strings(raw: &[u8], utf8: bool) -> Value {
    let trimmed = raw.strip_suffix(&[0]).unwrap_or(raw);
    let parts: Vec<String> = trimmed.split(|b| *b == 0).map(|p| text(p, utf8)).collect();
    if parts.len() == 1 {
        json!(parts[0])
    } else {
        json!(parts)
    }
}

fn text(raw: &[u8], utf8: bool) -> String {
    let s = if utf8 {
        String::from_utf8_lossy(raw).into_owned()
    } else {
        raw.iter().map(|b| *b as char).collect()
    };
    crate::utils::sanitize::multiline(&s)
}

fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Value::Object(e)) = (base.as_object_mut(), extra) {
        b.extend(e);
    }
    base
}

fn error_payload(action: &str, e: &XError) -> Value {
    json!({
        "action": action,
        "error": wire::error_name(e.code),
        "request": wire::request_name(e.major),
        "bad_value": hex_id(e.bad_value),
    })
}
