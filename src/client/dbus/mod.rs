//! D-Bus client: one connection to a bus (or a peer). Authenticates, says Hello, and then runs
//! three tasks like the other stream clients: a reader, a dispatcher that asks the model about
//! each event in turn, and the session that owns the write half, the serial counter and the
//! calls awaiting replies. A call made *to* NetGet is answered by the model; one that expects
//! a reply and gets none is answered with an error, as the server does.
pub mod actions;

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event, EventType};
use crate::server::dbus::wire::{self, Message};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::DbusClientProtocol;
use anyhow::{anyhow, bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Calls awaiting their reply at once; the next is refused.
pub const MAX_PENDING: usize = 64;
/// How many model turns may follow from one another before the chain stops.
pub const MAX_FOLLOWUP_DEPTH: u32 = 8;
const TURN_QUEUE: usize = 128;

type Reader = BufReader<Box<dyn AsyncRead + Send + Unpin>>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// An action for the session, with how deep in a chain it is and, for an answer, the call it
/// answers.
struct Work {
    action: Value,
    depth: u32,
    call: Option<Message>,
    command: Option<ClientCommand>,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let socket_path = params
        .map(|p| p.get_optional_string("socket_path"))
        .transpose()?
        .flatten();
    let use_bus = params
        .map(|p| p.get_optional_bool("bus"))
        .transpose()?
        .flatten()
        .unwrap_or(actions::DEFAULT_BUS);
    let timeout_ms = params
        .map(|p| p.get_optional_u64("timeout_ms"))
        .transpose()?
        .flatten()
        .unwrap_or(actions::DEFAULT_TIMEOUT_MS);
    ensure!(
        (100..=120_000).contains(&timeout_ms),
        "timeout_ms {timeout_ms} is outside 100-120000"
    );
    let (mut reader, mut writer, local) =
        tokio::time::timeout(CONNECT_TIMEOUT, open(&ctx.remote_addr, socket_path))
            .await
            .context("D-Bus connect deadline")??;
    let (guid, mechanism) =
        tokio::time::timeout(CONNECT_TIMEOUT, authenticate(&mut reader, &mut writer))
            .await
            .context("D-Bus authentication did not finish within 10 s")??;
    let mut serial = 1u32;
    let unique = if use_bus {
        let mut hello = Message::call(wire::BUS_PATH, Some("org.freedesktop.DBus"), "Hello");
        hello.destination = Some(wire::BUS_NAME.into());
        hello.serial = serial;
        serial += 1;
        writer.write_all(&hello.encode()?).await?;
        let name = tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                let m = wire::read_message(&mut reader)
                    .await?
                    .context("the bus closed the connection after authentication")?;
                if m.reply_serial == Some(1) {
                    if m.kind == wire::ERROR {
                        bail!(
                            "the bus refused Hello: {}",
                            m.error_name.unwrap_or_default()
                        );
                    }
                    break m
                        .first_string()
                        .map(str::to_string)
                        .context("Hello's reply carried no name");
                }
            }
        })
        .await
        .context("the bus did not answer Hello within 10 s")??;
        Some(name)
    } else {
        None
    };
    Log::new(Some(&ctx.status_tx)).info(format!(
        "D-Bus client {} connected ({mechanism}){}",
        ctx.client_id,
        unique
            .as_deref()
            .map(|u| format!(" as {u}"))
            .unwrap_or_default()
    ));
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;

    let (message_tx, messages) = mpsc::channel::<Result<Message>>(256);
    let reader_task = tokio::spawn(async move {
        loop {
            let item = wire::read_message(&mut reader).await;
            let end = !matches!(item, Ok(Some(_)));
            let sent = match item {
                Ok(Some(m)) => message_tx.send(Ok(m)).await,
                Ok(None) => {
                    message_tx
                        .send(Err(anyhow!("the bus closed the connection")))
                        .await
                }
                Err(e) => message_tx.send(Err(e)).await,
            };
            if end || sent.is_err() {
                return;
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, reader_task)
        .await;

    let (event_tx, event_rx) = mpsc::channel::<(Event, u32, Option<Message>)>(TURN_QUEUE);
    let (internal_tx, internal) = mpsc::channel::<Work>(64);
    let _ = event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({"unique_name": unique, "server_guid": guid, "mechanism": mechanism}),
        ),
        0,
        None,
    ));
    let dispatcher = tokio::spawn(run_turns(ctx.clone(), event_rx, internal_tx));
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let mut session = Session {
        writer,
        serial,
        unique,
        pending: HashMap::new(),
        events: event_tx,
        timeout: Duration::from_millis(timeout_ms),
        client_id: ctx.client_id,
    };
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session
            .run(&session_ctx, messages, external, internal)
            .await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("D-Bus client ended: {e:#}"));
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
                .with_context(|| format!("cannot connect to the D-Bus socket {path}"))?;
            let (r, w) = tokio::io::split(stream);
            let r: Box<dyn AsyncRead + Send + Unpin> = Box::new(r);
            return Ok((
                BufReader::new(r),
                Box::new(w),
                SocketAddr::from(([0, 0, 0, 0], 0)),
            ));
        }
        #[cfg(not(unix))]
        bail!("socket_path {path} needs a Unix platform");
    }
    ensure!(
        !remote.trim().is_empty(),
        "the D-Bus client needs remote_addr (host:port) or socket_path"
    );
    let stream = tokio::net::TcpStream::connect(remote)
        .await
        .with_context(|| format!("cannot connect to D-Bus at {remote}"))?;
    let local = stream.local_addr()?;
    let (r, w) = tokio::io::split(stream);
    let r: Box<dyn AsyncRead + Send + Unpin> = Box::new(r);
    Ok((BufReader::new(r), Box::new(w), local))
}

/// The client's half of SASL: EXTERNAL as this process's uid, then ANONYMOUS if the server
/// offers it. The server's GUID and the mechanism that worked.
async fn authenticate(r: &mut Reader, w: &mut Writer) -> Result<(String, &'static str)> {
    #[cfg(unix)]
    let uid = unsafe { libc::geteuid() }.to_string();
    #[cfg(not(unix))]
    let uid = String::new();
    w.write_all(format!("\0AUTH EXTERNAL {}\r\n", wire::hex_ascii(&uid)).as_bytes())
        .await?;
    let mut mechanism = "EXTERNAL";
    for _ in 0..4 {
        let line = wire::read_auth_line(r).await?;
        let mut words = line.split_whitespace();
        match words.next() {
            Some("OK") => {
                let guid = words.next().unwrap_or_default().to_string();
                w.write_all(b"BEGIN\r\n").await?;
                return Ok((guid, mechanism));
            }
            Some("REJECTED") if mechanism == "EXTERNAL" => {
                let offered: Vec<&str> = words.collect();
                ensure!(
                    offered.contains(&"ANONYMOUS"),
                    "the server refused EXTERNAL and offers {offered:?}; NetGet speaks EXTERNAL and ANONYMOUS"
                );
                mechanism = "ANONYMOUS";
                w.write_all(format!("AUTH ANONYMOUS {}\r\n", wire::hex_ascii("netget")).as_bytes())
                    .await?;
            }
            Some("DATA") => w.write_all(b"DATA\r\n").await?,
            _ => bail!(
                "the server refused authentication: {}",
                crate::utils::sanitize::line_field(&line)
            ),
        }
    }
    bail!("authentication did not converge")
}

async fn run_turns(
    ctx: ConnectContext,
    mut events: mpsc::Receiver<(Event, u32, Option<Message>)>,
    internal: mpsc::Sender<Work>,
) {
    let protocol = DbusClientProtocol;
    while let Some((event, depth, call)) = events.recv().await {
        let expects_reply = call.as_ref().is_some_and(Message::expects_reply);
        if depth >= MAX_FOLLOWUP_DEPTH && !expects_reply {
            tracing::warn!("D-Bus client {} not asking the model about {}: {depth} turns deep decision=followup_depth", ctx.client_id, event.id());
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
        let outcome = if depth >= MAX_FOLLOWUP_DEPTH {
            Err(anyhow!("follow-up depth {depth} reached"))
        } else {
            call_llm_for_client(
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
        };
        let mut answered = false;
        let fail = |name: &str, message: &str| json!({"type": "dbus_error", "name": name, "message": message});
        let mut work: Vec<Value> = match outcome {
            Ok(result) => {
                if let Some(memory) = result.memory_updates {
                    ctx.state.set_memory_for_client(ctx.client_id, memory).await;
                }
                result.actions
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("D-Bus client handler: {e}"));
                if expects_reply {
                    let name =
                        if crate::utils::wire_failure::WireFailure::classify(&e).is_overloaded() {
                            "org.freedesktop.DBus.Error.LimitsExceeded"
                        } else {
                            "org.freedesktop.DBus.Error.Failed"
                        };
                    tracing::error!(
                        "D-Bus client {} decision=fail_closed_llm_error",
                        ctx.client_id
                    );
                    vec![fail(
                        name,
                        crate::utils::wire_failure::WireFailure::classify(&e).text(),
                    )]
                } else {
                    vec![]
                }
            }
        };
        for a in &work {
            answered |= matches!(a["type"].as_str(), Some("dbus_return") | Some("dbus_error"));
        }
        if expects_reply && !answered {
            tracing::error!(
                "D-Bus client {} decision=fail_closed_no_answer",
                ctx.client_id
            );
            work.push(fail(
                "org.freedesktop.DBus.Error.Failed",
                "No answer was produced for this call",
            ));
        }
        for action in work {
            let item = Work {
                action,
                depth: depth + 1,
                call: call.clone(),
                command: None,
            };
            if internal.send(item).await.is_err() {
                return;
            }
        }
    }
}

struct Pending {
    destination: Option<String>,
    member: String,
    deadline: Instant,
    depth: u32,
    command: Option<ClientCommand>,
}

struct Session {
    writer: Writer,
    serial: u32,
    unique: Option<String>,
    pending: HashMap<u32, Pending>,
    events: mpsc::Sender<(Event, u32, Option<Message>)>,
    timeout: Duration,
    client_id: crate::state::ClientId,
}

fn reply_command(command: Option<ClientCommand>, outcome: ClientSendOutcome) {
    if let Some(command) = command {
        crate::client::command_support::reply(command, Ok(outcome));
    }
}

impl Session {
    fn emit(&self, t: &'static EventType, data: Value, depth: u32, call: Option<Message>) {
        if self
            .events
            .try_send((Event::new(t, data), depth, call))
            .is_err()
        {
            tracing::warn!("D-Bus client {} dropped an event: the model is {TURN_QUEUE} events behind decision=turn_queue_full", self.client_id);
        }
    }

    async fn send(&mut self, mut m: Message) -> Result<u32> {
        m.serial = self.serial;
        self.serial = self.serial.checked_add(1).unwrap_or(1);
        if m.sender.is_none() {
            m.sender = self.unique.clone();
        }
        self.writer.write_all(&m.encode()?).await?;
        Ok(m.serial)
    }

    async fn run(
        &mut self,
        ctx: &ConnectContext,
        mut messages: mpsc::Receiver<Result<Message>>,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<Work>,
    ) -> Result<()> {
        loop {
            let deadline = self.pending.values().map(|p| p.deadline).min();
            let work = tokio::select! {
                m = messages.recv() => {
                    let m = m.context("the reader stopped")??;
                    self.incoming(m).await?;
                    continue;
                }
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.expire();
                    continue;
                }
                c = external.recv() => match c {
                    Some(c) => Work { action: c.action.clone(), depth: 0, call: None, command: Some(c) },
                    None => return Ok(()),
                },
                w = internal.recv() => match w {
                    Some(w) => w,
                    None => return Ok(()),
                },
            };
            if work.command.is_some() {
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "D-Bus",
                        None,
                        "injected_action",
                        work.action.clone(),
                        vec![],
                    )
                    .await;
            }
            if self.perform(work).await? {
                return Ok(());
            }
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let due: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(s, _)| *s)
            .collect();
        for serial in due {
            let Some(p) = self.pending.remove(&serial) else {
                continue;
            };
            let data = json!({"destination": p.destination, "member": p.member, "name": "org.freedesktop.DBus.Error.NoReply",
                "message": format!("no reply within {} ms", self.timeout.as_millis())});
            reply_command(
                p.command,
                ClientSendOutcome::Executed {
                    detail: data.to_string(),
                },
            );
            self.emit(&actions::ERROR_REPLY_EVENT, data, p.depth, None);
        }
    }

    async fn incoming(&mut self, m: Message) -> Result<()> {
        match m.kind {
            wire::METHOD_RETURN | wire::ERROR => {
                let Some(p) = m.reply_serial.and_then(|s| self.pending.remove(&s)) else {
                    tracing::debug!(
                        "D-Bus client {} ignored a reply to a call it is not waiting on",
                        self.client_id
                    );
                    return Ok(());
                };
                let (t, data) = if m.kind == wire::METHOD_RETURN {
                    (
                        &*actions::REPLY_EVENT,
                        json!({"destination": p.destination, "member": p.member, "signature": m.signature, "values": m.body}),
                    )
                } else {
                    (
                        &*actions::ERROR_REPLY_EVENT,
                        json!({"destination": p.destination, "member": p.member, "name": m.error_name, "message": m.first_string()}),
                    )
                };
                reply_command(
                    p.command,
                    ClientSendOutcome::Executed {
                        detail: data.to_string(),
                    },
                );
                self.emit(t, data, p.depth, None);
            }
            wire::SIGNAL => {
                let from_bus = m.sender.as_deref() == Some(wire::BUS_NAME);
                if from_bus
                    && matches!(m.member.as_deref(), Some("NameAcquired") | Some("NameLost"))
                {
                    return Ok(()); // the request_name reply already said so
                }
                let data = json!({"sender": m.sender, "path": m.path, "interface": m.interface, "member": m.member, "signature": m.signature, "args": m.body});
                self.emit(&actions::SIGNAL_EVENT, data, 0, None);
            }
            wire::METHOD_CALL if m.interface.as_deref() == Some("org.freedesktop.DBus.Peer") => {
                let answer = match m.member.as_deref() {
                    Some("Ping") => Message::reply_to(&m, "", vec![]),
                    Some("GetMachineId") => {
                        Message::reply_to(&m, "s", vec![json!(wire::random_guid())])
                    }
                    _ => Message::error_to(
                        &m,
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "org.freedesktop.DBus.Peer has Ping and GetMachineId",
                    ),
                };
                if m.expects_reply() {
                    self.send(answer).await?;
                }
            }
            wire::METHOD_CALL => {
                let data = json!({"sender": m.sender, "destination": m.destination, "path": m.path, "interface": m.interface,
                    "member": m.member, "signature": m.signature, "args": m.body, "no_reply_expected": !m.expects_reply()});
                self.emit(&actions::INCOMING_CALL_EVENT, data, 0, Some(m));
            }
            _ => {}
        }
        Ok(())
    }

    /// Execute one action. `Ok(true)` when it ends the session.
    async fn perform(&mut self, w: Work) -> Result<bool> {
        let Work {
            action,
            depth,
            call,
            command,
        } = w;
        let kind = action["type"].as_str().unwrap_or_default().to_string();
        let refused = |command: Option<ClientCommand>, error: String| {
            reply_command(command, ClientSendOutcome::Rejected { error })
        };
        match DbusClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply_command(command, ClientSendOutcome::Disconnected);
                let _ = self.writer.shutdown().await;
                return Ok(true);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("D-Bus client {} refused {kind}: {e}", self.client_id);
                refused(command, e.to_string());
                return Ok(false);
            }
        }
        let s = |k: &str| action[k].as_str().map(str::to_string);
        let call_message = match kind.as_str() {
            "dbus_call" => {
                let mut m = Message::call(
                    &s("path").unwrap_or_default(),
                    s("interface").as_deref(),
                    &s("member").unwrap_or_default(),
                );
                m.destination = s("destination");
                m.signature = s("signature").unwrap_or_default();
                m.body = action["args"].as_array().cloned().unwrap_or_default();
                Some(m)
            }
            "dbus_request_name" => {
                let mut m =
                    Message::call(wire::BUS_PATH, Some("org.freedesktop.DBus"), "RequestName");
                m.destination = Some(wire::BUS_NAME.into());
                m.signature = "su".into();
                m.body = vec![json!(s("name")), json!(4)]; // DO_NOT_QUEUE
                Some(m)
            }
            "dbus_add_match" => {
                let mut m = Message::call(wire::BUS_PATH, Some("org.freedesktop.DBus"), "AddMatch");
                m.destination = Some(wire::BUS_NAME.into());
                m.signature = "s".into();
                m.body = vec![json!(s("rule"))];
                Some(m)
            }
            _ => None,
        };
        if let Some(m) = call_message {
            if self.pending.len() >= MAX_PENDING {
                refused(
                    command,
                    format!("{MAX_PENDING} calls already await replies"),
                );
                return Ok(false);
            }
            let (destination, member) =
                (m.destination.clone(), m.member.clone().unwrap_or_default());
            let serial = self.send(m).await?;
            self.pending.insert(
                serial,
                Pending {
                    destination,
                    member,
                    deadline: Instant::now() + self.timeout,
                    depth,
                    command,
                },
            );
            return Ok(false);
        }
        // An answer or a signal.
        if matches!(kind.as_str(), "dbus_return" | "dbus_error") && call.is_none() {
            refused(command, format!("{kind} answers a call made to this client; use it in reply to dbus_method_call"));
            return Ok(false);
        }
        if let Some(c) = &call {
            if kind != "dbus_emit_signal" && !c.expects_reply() {
                reply_command(
                    command,
                    ClientSendOutcome::Executed {
                        detail: "the caller asked for no reply".into(),
                    },
                );
                return Ok(false);
            }
        }
        match crate::server::dbus::actions::answer_message(call.as_ref(), &action, None) {
            Ok(mut m) => {
                m.sender = None;
                self.send(m).await?;
                reply_command(
                    command,
                    ClientSendOutcome::Executed {
                        detail: format!("{kind} sent"),
                    },
                );
            }
            Err(e) => refused(command, e.to_string()),
        }
        Ok(false)
    }
}
