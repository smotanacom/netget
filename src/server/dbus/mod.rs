//! D-Bus server: NetGet as a small message bus that every call terminates at. A client
//! authenticates (SASL EXTERNAL on the Unix socket, checked against the peer's uid; ANONYMOUS
//! where allowed), may say Hello and request names, and every method call that is not the
//! bus's own goes to the model, which answers with a return value or an error.
//!
//! **Fails closed:** a call expecting a reply that gets none from the model — no answer, an
//! answer that does not encode, a backend failure — is answered with a D-Bus error, never
//! left to the caller's timeout.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use actions::DbusProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use wire::Message;

pub const DEFAULT_ALLOW_ANONYMOUS: bool = true;
/// How long an authenticated connection may stay silent, by default.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86_400;
/// The SASL handshake, from connect to BEGIN.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

const BUS_INTERFACE: &str = "org.freedesktop.DBus";
const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";

/// What every connection of one server shares: the bus's id and who owns which name.
struct Bus {
    guid: String,
    /// Well-known name -> owning unique name; unique names map to themselves.
    owners: std::sync::Mutex<BTreeMap<String, String>>,
}

#[derive(Clone)]
struct Policy {
    allow_anonymous: bool,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let socket_path = params
        .map(|p| p.get_optional_string("socket_path"))
        .transpose()?
        .flatten();
    let allow_anonymous = params
        .map(|p| p.get_optional_bool("allow_anonymous"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_ALLOW_ANONYMOUS);
    let idle_secs = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&idle_secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    let policy = Policy {
        allow_anonymous,
        idle: Duration::from_secs(idle_secs),
    };
    let bus = Arc::new(Bus {
        guid: wire::random_guid(),
        owners: Default::default(),
    });
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr())
        .await
        .context("D-Bus failed to bind")?;
    let local = listener.local_addr()?;
    let log = Log::new(Some(&ctx.status_tx));
    log.info(format!(
        "D-Bus server listening on tcp:host={},port={}{}",
        local.ip(),
        local.port(),
        socket_path
            .as_deref()
            .map(|p| format!(" and unix:path={p}"))
            .unwrap_or_default()
    ));
    let limiter = Arc::new(ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS));

    #[cfg(unix)]
    if let Some(path) = socket_path {
        let p = std::path::Path::new(&path);
        if p.exists() {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                std::fs::symlink_metadata(p)?.file_type().is_socket(),
                "{path} exists and is not a socket; refusing to replace it"
            );
            std::fs::remove_file(p)?;
        }
        let unix = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("D-Bus failed to bind {path}"))?;
        let (uctx, bus, policy, limiter) =
            (ctx.clone(), bus.clone(), policy.clone(), limiter.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, permit)) = crate::server::accept_bounded::accept_bounded_unix(
                    &unix,
                    &limiter,
                    b"",
                    "D-Bus",
                    Some(&uctx.status_tx),
                )
                .await
                else {
                    break;
                };
                let uid = stream.peer_cred().ok().map(|c| c.uid());
                let (r, w) = tokio::io::split(stream);
                spawn_connection(&uctx, &bus, &policy, r, w, local, uid, permit).await;
            }
        });
        ctx.state.register_server_task(ctx.server_id, task).await;
    }
    #[cfg(not(unix))]
    if socket_path.is_some() {
        bail!("socket_path needs a Unix platform");
    }

    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, peer, permit)) =
                accept_bounded(&listener, &limiter, b"", "D-Bus", Some(&ctx.status_tx)).await
            else {
                break;
            };
            let (r, w) = tokio::io::split(stream);
            spawn_connection(&ctx, &bus, &policy, r, w, peer, None, permit).await;
        }
    });
    state.register_server_task(server_id, task).await;
    Ok(local)
}

#[allow(clippy::too_many_arguments)]
async fn spawn_connection<R, W, P>(
    ctx: &SpawnContext,
    bus: &Arc<Bus>,
    policy: &Policy,
    reader: R,
    writer: W,
    peer: SocketAddr,
    peer_uid: Option<u32>,
    permit: P,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    P: Send + 'static,
{
    let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
    let now = crate::utils::clock::Instant::now();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id,
                remote_addr: peer,
                local_addr: peer,
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
    let writer = Arc::new(Mutex::new(writer));
    let peer_rx =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    crate::server::peer_support::spawn_peer_command_task(
        peer_rx,
        Arc::new(DbusProtocol::new()),
        ctx.state.clone(),
        ctx.server_id,
        id.as_u32(),
        writer.clone(),
        ctx.status_tx.clone(),
    );
    let (child, bus, policy) = (ctx.clone(), bus.clone(), policy.clone());
    ctx.state
        .spawn_server_task(ctx.server_id, async move {
            let _permit = permit;
            let mut reader = BufReader::new(reader);
            let mut conn = Connection {
                ctx: &child,
                bus: &bus,
                writer: &writer,
                id,
                serials: Arc::new(AtomicU32::new(1)),
                unique: None,
            };
            let result = async {
                tokio::time::timeout(
                    AUTH_TIMEOUT,
                    authenticate(
                        &mut reader,
                        &writer,
                        peer_uid,
                        policy.allow_anonymous,
                        &bus.guid,
                    ),
                )
                .await
                .context("authentication did not finish within 10 s")??;
                conn.serve(&mut reader, policy.idle).await
            }
            .await;
            if let Err(e) = result {
                Log::new(Some(&child.status_tx))
                    .warn(format!("D-Bus connection {id} from {peer} ended: {e:#}"));
            }
            if let Some(unique) = &conn.unique {
                bus.owners
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|_, owner| owner != unique);
            }
            child
                .state
                .remove_peer_handle(child.server_id, id.as_u32())
                .await;
            let _ = writer.lock().await.shutdown().await;
            child
                .state
                .update_connection_status(child.server_id, id, ConnectionStatus::Closed)
                .await;
            let _ = child.status_tx.send("__UPDATE_UI__".into());
        })
        .await;
}

/// The server's half of SASL: a NUL byte, then lines until BEGIN. EXTERNAL is offered only
/// where the peer's uid is known (the Unix socket) and succeeds only for that uid.
pub async fn authenticate<B, W>(
    r: &mut B,
    w: &Arc<Mutex<W>>,
    peer_uid: Option<u32>,
    allow_anonymous: bool,
    guid: &str,
) -> Result<()>
where
    B: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut nul = [0u8; 1];
    r.read_exact(&mut nul).await?;
    ensure!(nul[0] == 0, "the peer did not start with the SASL NUL byte");
    let mut mechanisms = Vec::new();
    if peer_uid.is_some() {
        mechanisms.push("EXTERNAL");
    }
    if allow_anonymous {
        mechanisms.push("ANONYMOUS");
    }
    let rejected = format!("REJECTED {}\r\n", mechanisms.join(" "));
    let ok = format!("OK {guid}\r\n");
    let send = |line: String| async move { w.lock().await.write_all(line.as_bytes()).await };
    let external_ok = |hex_identity: &str| -> bool {
        let claimed = hex::decode(hex_identity)
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        match (peer_uid, claimed) {
            (Some(uid), Some(c)) => c.is_empty() || c == uid.to_string(),
            _ => false,
        }
    };
    let mut authed = false;
    let mut waiting_external_data = false;
    for _ in 0..wire::MAX_AUTH_LINES {
        let line = wire::read_auth_line(r).await?;
        let mut words = line.split_whitespace();
        let command = words.next().unwrap_or_default();
        let arg = words.next();
        let reply = match command {
            "AUTH" => match arg {
                Some("EXTERNAL") if mechanisms.contains(&"EXTERNAL") => match words.next() {
                    Some(identity) if external_ok(identity) => {
                        authed = true;
                        ok.clone()
                    }
                    Some(_) => rejected.clone(),
                    None => {
                        waiting_external_data = true;
                        "DATA\r\n".into()
                    }
                },
                Some("ANONYMOUS") if allow_anonymous => {
                    authed = true;
                    ok.clone()
                }
                _ => rejected.clone(),
            },
            "DATA" if waiting_external_data => {
                waiting_external_data = false;
                if external_ok(arg.unwrap_or_default()) {
                    authed = true;
                    ok.clone()
                } else {
                    rejected.clone()
                }
            }
            "BEGIN" if authed => return Ok(()),
            "NEGOTIATE_UNIX_FD" if authed => {
                "ERROR Unix file descriptors are not supported\r\n".into()
            }
            "CANCEL" | "ERROR" => {
                authed = false;
                rejected.clone()
            }
            _ => "ERROR unexpected command\r\n".into(),
        };
        send(reply).await?;
    }
    bail!(
        "authentication took more than {} lines",
        wire::MAX_AUTH_LINES
    )
}

struct Connection<'a, W> {
    ctx: &'a SpawnContext,
    bus: &'a Arc<Bus>,
    writer: &'a Arc<Mutex<W>>,
    id: ConnectionId,
    serials: Arc<AtomicU32>,
    unique: Option<String>,
}

impl<W: AsyncWrite + Unpin> Connection<'_, W> {
    async fn send(&self, mut m: Message) -> Result<()> {
        m.serial = self.serials.fetch_add(1, Ordering::Relaxed);
        if m.destination.is_none() {
            m.destination = self.unique.clone();
        }
        self.write(&m.encode()?).await
    }

    async fn write(&self, bytes: &[u8]) -> Result<()> {
        self.writer.lock().await.write_all(bytes).await?;
        self.ctx
            .state
            .update_connection_stats(
                self.ctx.server_id,
                self.id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    async fn serve<B: AsyncBufRead + Unpin>(&mut self, r: &mut B, idle: Duration) -> Result<()> {
        loop {
            let message = match tokio::time::timeout(idle, wire::read_message(r)).await {
                Err(_) => return Ok(()),
                Ok(m) => match m? {
                    Some(m) => m,
                    None => return Ok(()),
                },
            };
            self.ctx
                .state
                .update_connection_stats(self.ctx.server_id, self.id, None, None, Some(1), None)
                .await;
            match message.kind {
                wire::METHOD_CALL if is_bus_call(&message) => self.bus_method(&message).await?,
                wire::METHOD_CALL if message.interface.as_deref() == Some(PEER_INTERFACE) => {
                    self.peer_method(&message).await?
                }
                wire::METHOD_CALL | wire::SIGNAL => self.ask_model(message).await?,
                other => tracing::debug!(
                    "D-Bus ignored a {} from connection {}",
                    wire::type_name(other),
                    self.id
                ),
            }
        }
    }

    async fn reply(&self, call: &Message, signature: &str, body: Vec<Value>) -> Result<()> {
        if call.expects_reply() {
            self.send(Message::reply_to(call, signature, body)).await?;
        }
        Ok(())
    }

    async fn error(&self, call: &Message, name: &str, text: &str) -> Result<()> {
        if call.expects_reply() {
            self.send(Message::error_to(call, name, text)).await?;
        }
        Ok(())
    }

    fn owners(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.bus.owners.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bus_signal(&self, member: &str, signature: &str, body: Vec<Value>) -> Message {
        Message {
            kind: wire::SIGNAL,
            path: Some(wire::BUS_PATH.into()),
            interface: Some(BUS_INTERFACE.into()),
            member: Some(member.into()),
            sender: Some(wire::BUS_NAME.into()),
            signature: signature.into(),
            body,
            ..Default::default()
        }
    }

    /// The methods a message bus answers itself.
    async fn bus_method(&mut self, call: &Message) -> Result<()> {
        let member = call.member.as_deref().unwrap_or_default();
        let arg = call.first_string().unwrap_or_default().to_string();
        if member != "Hello" && self.unique.is_none() && call.destination.is_some() {
            return self
                .error(
                    call,
                    "org.freedesktop.DBus.Error.AccessDenied",
                    "Client tried to send a message other than Hello without being registered",
                )
                .await;
        }
        match member {
            "Hello" => {
                if self.unique.is_some() {
                    return self
                        .error(
                            call,
                            "org.freedesktop.DBus.Error.Failed",
                            "Already handled an Hello message",
                        )
                        .await;
                }
                let unique = format!(":1.{}", self.id.as_u32());
                self.owners().insert(unique.clone(), unique.clone());
                self.unique = Some(unique.clone());
                self.reply(call, "s", vec![json!(unique)]).await?;
                self.send(self.bus_signal("NameAcquired", "s", vec![json!(unique)]))
                    .await
            }
            "RequestName" => {
                if !wire::valid_bus_name(&arg) || arg.starts_with(':') {
                    return self
                        .error(
                            call,
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            &format!("{arg:?} is not a well-known bus name"),
                        )
                        .await;
                }
                let me = self
                    .unique
                    .clone()
                    .unwrap_or_else(|| format!(":1.{}", self.id.as_u32()));
                let code = {
                    let mut owners = self.owners();
                    match owners.get(&arg) {
                        Some(owner) if *owner == me => 4, // ALREADY_OWNER
                        Some(_) => 3,                     // EXISTS
                        None => {
                            owners.insert(arg.clone(), me);
                            1 // PRIMARY_OWNER
                        }
                    }
                };
                self.reply(call, "u", vec![json!(code)]).await?;
                if code == 1 {
                    self.send(self.bus_signal("NameAcquired", "s", vec![json!(arg)]))
                        .await?;
                }
                Ok(())
            }
            "ReleaseName" => {
                let me = self.unique.clone().unwrap_or_default();
                let code = {
                    let mut owners = self.owners();
                    match owners.get(&arg) {
                        Some(owner) if *owner == me && !arg.starts_with(':') => {
                            owners.remove(&arg);
                            1
                        }
                        Some(_) => 3,
                        None => 2,
                    }
                };
                self.reply(call, "u", vec![json!(code)]).await
            }
            "GetId" => self.reply(call, "s", vec![json!(self.bus.guid)]).await,
            "ListNames" => {
                let mut names = vec![json!(wire::BUS_NAME)];
                names.extend(self.owners().keys().map(|n| json!(n)));
                self.reply(call, "as", vec![Value::Array(names)]).await
            }
            "ListActivatableNames" => self.reply(call, "as", vec![json!([wire::BUS_NAME])]).await,
            "NameHasOwner" => {
                let has = arg == wire::BUS_NAME || self.owners().contains_key(&arg);
                self.reply(call, "b", vec![json!(has)]).await
            }
            "GetNameOwner" => {
                if arg == wire::BUS_NAME {
                    return self.reply(call, "s", vec![json!(wire::BUS_NAME)]).await;
                }
                let owner = self.owners().get(&arg).cloned();
                match owner {
                    Some(o) => self.reply(call, "s", vec![json!(o)]).await,
                    None => {
                        self.error(
                            call,
                            "org.freedesktop.DBus.Error.NameHasNoOwner",
                            &format!("Could not get owner of name '{arg}': no such name"),
                        )
                        .await
                    }
                }
            }
            "AddMatch" | "RemoveMatch" => self.reply(call, "", vec![]).await,
            _ => {
                self.error(
                    call,
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    &format!("NetGet's bus does not implement {member}"),
                )
                .await
            }
        }
    }

    async fn peer_method(&self, call: &Message) -> Result<()> {
        match call.member.as_deref() {
            Some("Ping") => self.reply(call, "", vec![]).await,
            Some("GetMachineId") => self.reply(call, "s", vec![json!(self.bus.guid)]).await,
            _ => {
                self.error(
                    call,
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "org.freedesktop.DBus.Peer has Ping and GetMachineId",
                )
                .await
            }
        }
    }

    /// A call or signal for the model, with the fail-closed rule for calls.
    async fn ask_model(&self, mut m: Message) -> Result<()> {
        if m.sender.is_none() {
            m.sender = self.unique.clone();
        }
        let args = Value::Array(m.body.clone());
        let event = if m.kind == wire::METHOD_CALL {
            Event::new(
                &actions::METHOD_CALL_EVENT,
                json!({
                    "path": m.path, "interface": m.interface, "member": m.member,
                    "destination": m.destination, "sender": m.sender,
                    "signature": m.signature, "args": args, "no_reply_expected": !m.expects_reply(),
                }),
            )
        } else {
            Event::new(
                &actions::SIGNAL_EVENT,
                json!({"path": m.path, "interface": m.interface, "member": m.member, "sender": m.sender, "signature": m.signature, "args": args}),
            )
        };
        let protocol = DbusProtocol::for_call(m.clone(), self.serials.clone());
        let outcome = call_llm(
            &self.ctx.llm_client,
            &self.ctx.state,
            self.ctx.server_id,
            Some(self.id),
            &event,
            &protocol,
        )
        .await;
        let log = Log::new(Some(&self.ctx.status_tx));
        let summary = format!(
            "D-Bus {} {}.{} from connection {}",
            wire::type_name(m.kind),
            m.interface.as_deref().unwrap_or("-"),
            m.member.as_deref().unwrap_or("-"),
            self.id
        );
        let execution = match outcome {
            Ok(e) => e,
            Err(e) => {
                let (name, category) = match crate::utils::wire_failure::WireFailure::classify(&e) {
                    crate::utils::wire_failure::WireFailure::Overloaded => {
                        ("org.freedesktop.DBus.Error.LimitsExceeded", "overloaded")
                    }
                    crate::utils::wire_failure::WireFailure::Unavailable => {
                        ("org.freedesktop.DBus.Error.Failed", "unavailable")
                    }
                };
                log.error(format!(
                    "{summary} decision=fail_closed_llm_error category={category}: {e}"
                ));
                return self
                    .error(
                        &m,
                        name,
                        crate::utils::wire_failure::WireFailure::classify(&e).text(),
                    )
                    .await;
            }
        };
        let mut replied = false;
        for result in &execution.protocol_results {
            for output in result.get_all_output() {
                replied |= matches!(
                    output.get(1),
                    Some(&wire::METHOD_RETURN) | Some(&wire::ERROR)
                );
                self.write(&output).await?;
            }
        }
        if !m.expects_reply() {
            log.info(format!("{summary} decision=model_answered"));
            return Ok(());
        }
        if replied {
            log.info(format!("{summary} decision=model_reply"));
            return Ok(());
        }
        log.error(format!(
            "{summary} decision=fail_closed_no_answer (no usable dbus_return or dbus_error)"
        ));
        self.error(
            &m,
            "org.freedesktop.DBus.Error.Failed",
            "No answer was produced for this call",
        )
        .await
    }
}

fn is_bus_call(m: &Message) -> bool {
    match m.destination.as_deref() {
        Some(d) => d == wire::BUS_NAME,
        None => {
            m.path.as_deref() == Some(wire::BUS_PATH)
                && m.interface.as_deref() == Some(BUS_INTERFACE)
        }
    }
}
