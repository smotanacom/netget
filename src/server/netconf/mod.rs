//! NETCONF over SSH (RFC 6241, RFC 6242): russh carries the transport, NetGet owns the
//! `netconf` subsystem — hello exchange, framing, envelopes, message identity and the
//! capability rules — and a handler decides every RPC's data and outcome.
pub mod actions;
pub(crate) mod owned_stream;
pub mod rpc;
pub mod wire;
pub mod xml;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, ensure, Context, Result};
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodSet};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use wire::{Decoder, Framing};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a closed session waits for the client to hang up before the server does.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
const READ_CHUNK: usize = 16 * 1024;

/// Bound a seconds parameter, falling back to `default`.
pub fn seconds(value: Option<u64>, default: Duration, max: u64, name: &str) -> Result<Duration> {
    let v = value.unwrap_or(default.as_secs());
    ensure!((1..=max).contains(&v), "{name} must be between 1 and {max}");
    Ok(Duration::from_secs(v))
}

/// Everything one server instance shares across its connections.
struct Shared {
    ctx: SpawnContext,
    capabilities: Vec<String>,
    handshake: Duration,
    idle: Duration,
    next_session: AtomicU32,
    /// Live NETCONF sessions by session-id, for kill-session.
    sessions: Mutex<HashMap<u32, CancellationToken>>,
}

fn parse_capabilities(value: Option<Value>) -> Result<Vec<String>> {
    let mut out = vec![rpc::BASE_10.to_owned(), rpc::BASE_11.to_owned()];
    let extra = match value {
        None => vec![json!(rpc::WRITABLE_RUNNING)],
        Some(Value::Array(items)) => items,
        Some(_) => bail!("capabilities must be an array of capability URIs"),
    };
    for item in extra {
        let uri = item.as_str().context("capabilities entries must be strings")?;
        ensure!(rpc::capability_ok(uri), "capability '{uri}' is not a URI");
        ensure!(out.len() < rpc::MAX_CAPABILITIES, "too many capabilities");
        if !out.iter().any(|c| c == uri) {
            out.push(uri.to_owned());
        }
    }
    Ok(out)
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let capabilities = parse_capabilities(
        params.map(|p| p.get_optional_array("capabilities")).transpose()?.flatten().map(|v| Value::Array(v.clone())),
    )?;
    let handshake = seconds(
        params.map(|p| p.get_optional_u64("handshake_timeout_secs")).transpose()?.flatten(),
        HANDSHAKE_TIMEOUT,
        600,
        "handshake_timeout_secs",
    )?;
    let idle = seconds(
        params.map(|p| p.get_optional_u64("idle_timeout_secs")).transpose()?.flatten(),
        IDLE_TIMEOUT,
        86400,
        "idle_timeout_secs",
    )?;
    let key = match params.map(|p| p.get_optional_string("host_key_path")).transpose()?.flatten() {
        Some(path) => russh_keys::load_secret_key(&path, None)
            .with_context(|| format!("Failed to load NETCONF host key from {path}"))?,
        None => russh_keys::key::KeyPair::generate_ed25519().context("Failed to generate an Ed25519 host key")?,
    };
    let public = key.clone_public_key()?;
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    {
        use russh_keys::PublicKeyBase64;
        Log::new(Some(&ctx.status_tx)).info(format!(
            "NETCONF listening on {addr}; host key SHA256:{} ({} {})",
            public.fingerprint(),
            public.name(),
            public.public_key_base64()
        ));
    }
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        methods: MethodSet::PASSWORD,
        auth_rejection_time: Duration::from_millis(500),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(idle),
        ..Default::default()
    });
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        capabilities,
        handshake,
        idle,
        next_session: AtomicU32::new(1),
        sessions: Mutex::new(HashMap::new()),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"Exceeded MaxStartups\r\n",
                "NETCONF",
                Some(&shared.ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: addr,
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
            let config = config.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    connection(&child, config, id, socket).await;
                    child.ctx.state.update_connection_status(server_id, id, ConnectionStatus::Closed).await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(addr)
}

/// One SSH connection, owned for its whole life. russh spawns its own session driver;
/// [`owned_stream::OwnedStream`] is how dropping this future reaches it.
async fn connection(shared: &Arc<Shared>, config: Arc<russh::server::Config>, id: ConnectionId, socket: tokio::net::TcpStream) {
    let (stream, owner) = owned_stream::OwnedStream::new(socket);
    let started = Arc::new(Notify::new());
    let handler = Handler {
        shared: shared.clone(),
        id,
        owner: owner.token(),
        started: started.clone(),
        username: None,
        channel: None,
        netconf_started: false,
    };
    let log = Log::new(Some(&shared.ctx.status_tx));
    let token = owner.token();
    let session = async {
        let running = russh::server::run_stream(config, stream, handler).await?;
        running.await
    };
    let deadline = async {
        tokio::select! {
            _ = tokio::time::sleep(shared.handshake) => {}
            _ = started.notified() => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        outcome = session => {
            if let Err(e) = outcome {
                log.debug(format!("NETCONF connection {id} ended: {e}"));
            }
        }
        _ = token.cancelled() => {}
        _ = deadline => log.warn(format!("NETCONF connection {id}: no completed <hello> within {}s; closing", shared.handshake.as_secs())),
    }
    drop(owner);
}

struct Handler {
    shared: Arc<Shared>,
    id: ConnectionId,
    owner: CancellationToken,
    started: Arc<Notify>,
    username: Option<String>,
    channel: Option<Channel<Msg>>,
    netconf_started: bool,
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("NETCONF connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// Ask the handler, racing the connection owner. Returns the single expected action's data.
async fn decide(shared: &Shared, owner: &CancellationToken, id: ConnectionId, event: Event, expected: &str) -> Result<Value> {
    let ctx = &shared.ctx;
    let call = call_llm(&ctx.llm_client, &ctx.state, ctx.server_id, Some(id), &event, &actions::NetconfProtocol);
    let result = tokio::select! {
        biased;
        _ = owner.cancelled() => bail!("NETCONF connection closed before the answer arrived"),
        r = call => r,
    };
    let result = match result {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        bail!("NETCONF handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == expected => {
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("NETCONF handler supplied more than one reply");
                }
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    if found.is_none() {
        outcome(ctx, id, event.id(), "model_silent");
    }
    found.context("NETCONF handler did not answer")
}

#[async_trait::async_trait]
impl russh::server::Handler for Handler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        let reject = Auth::Reject { proceed_with_methods: Some(MethodSet::PASSWORD) };
        if self.username.is_some() || user.is_empty() || user.len() > 256 || password.len() > 1024 {
            return Ok(reject);
        }
        let event = Event::new(&actions::AUTH_EVENT, json!({"username": user, "password": password}));
        let allowed = match decide(&self.shared, &self.owner, self.id, event, "netconf_auth_decision").await {
            Ok(v) => {
                let allowed = v["allowed"] == true;
                outcome(&self.shared.ctx, self.id, "netconf_auth", if allowed { "model_answer" } else { "model_reject" });
                allowed
            }
            Err(_) => false,
        };
        if allowed {
            self.username = Some(user.to_owned());
            Ok(Auth::Accept)
        } else {
            Ok(reject)
        }
    }

    async fn channel_open_session(&mut self, channel: Channel<Msg>, _session: &mut Session) -> Result<bool, Self::Error> {
        if self.username.is_none() || self.channel.is_some() || self.netconf_started {
            return Ok(false);
        }
        self.channel = Some(channel);
        Ok(true)
    }

    async fn subsystem_request(&mut self, channel: ChannelId, name: &str, session: &mut Session) -> Result<(), Self::Error> {
        let ours = self.channel.as_ref().is_some_and(|c| c.id() == channel);
        if name != "netconf" || !ours || self.netconf_started {
            session.channel_failure(channel);
            return Ok(());
        }
        let Some(stream) = self.channel.take() else {
            session.channel_failure(channel);
            return Ok(());
        };
        let Some(username) = self.username.clone() else {
            session.channel_failure(channel);
            return Ok(());
        };
        self.netconf_started = true;
        session.channel_success(channel);
        let handle = session.handle();
        let shared = self.shared.clone();
        let owner = self.owner.clone();
        let started = self.started.clone();
        let id = self.id;
        let server_id = shared.ctx.server_id;
        shared
            .ctx
            .state
            .clone()
            .spawn_server_task(server_id, async move {
                let session_id = shared.next_session.fetch_add(1, Ordering::Relaxed).max(1);
                let mut io = stream.into_stream();
                tokio::select! {
                    _ = owner.cancelled() => {}
                    result = netconf_session(&shared, &owner, &started, id, session_id, &username, &mut io) => {
                        if let Err(e) = result {
                            Log::new(Some(&shared.ctx.status_tx)).debug(format!("NETCONF session {session_id} on connection {id} ended: {e}"));
                        }
                    }
                }
                shared.sessions.lock().await.remove(&session_id);
                // EOF then CHANNEL_CLOSE, then give the client a moment to hang up itself.
                let _ = tokio::time::timeout(Duration::from_millis(250), io.shutdown()).await;
                let _ = tokio::time::timeout(Duration::from_millis(250), handle.close(channel)).await;
                tokio::select! {
                    _ = owner.cancelled() => {}
                    _ = tokio::time::sleep(CLOSE_GRACE) => owner.cancel(),
                }
            })
            .await;
        Ok(())
    }
}

/// Read one framed message. The idle bound applies only while waiting for bytes.
async fn read_message<R: AsyncRead + Unpin>(io: &mut R, decoder: &mut Decoder, wait: Duration, stats: &mut u64) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        if let Some(m) = decoder.next_message()? {
            return Ok(Some(m));
        }
        let n = tokio::time::timeout(wait, io.read(&mut buf)).await.context("NETCONF read deadline")??;
        if n == 0 {
            ensure!(!decoder.is_partial(), "NETCONF peer closed mid-message");
            return Ok(None);
        }
        *stats += n as u64;
        decoder.feed(&buf[..n])?;
    }
}

async fn send<W: AsyncWrite + Unpin>(shared: &Shared, id: ConnectionId, io: &mut W, message: &[u8], framing: Framing) -> Result<()> {
    let framed = wire::frame(message, framing)?;
    tokio::time::timeout(WRITE_TIMEOUT, async {
        io.write_all(&framed).await?;
        io.flush().await
    })
    .await
    .context("NETCONF write deadline")??;
    shared
        .ctx
        .state
        .update_connection_stats(shared.ctx.server_id, id, None, Some(framed.len() as u64), None, Some(1))
        .await;
    Ok(())
}

async fn netconf_session<S: AsyncRead + AsyncWrite + Unpin>(
    shared: &Arc<Shared>,
    owner: &CancellationToken,
    started: &Notify,
    id: ConnectionId,
    session_id: u32,
    username: &str,
    io: &mut S,
) -> Result<()> {
    let ctx = &shared.ctx;
    send(shared, id, io, &rpc::hello(&shared.capabilities, Some(session_id))?, Framing::Delimiter).await?;
    let mut decoder = Decoder::new(Framing::Delimiter);
    let mut received = 0u64;
    let hello = read_message(io, &mut decoder, shared.handshake, &mut received)
        .await?
        .context("NETCONF peer closed before <hello>")?;
    let hello = rpc::parse_hello(&hello)?;
    ensure!(hello.session_id.is_none(), "NETCONF client <hello> must not carry a session-id");
    let framing = rpc::negotiate(&shared.capabilities, &hello.capabilities).context("NETCONF peer shares no base version")?;
    decoder.set_framing(framing)?;
    started.notify_one();
    shared.sessions.lock().await.insert(session_id, owner.clone());
    Log::new(Some(&ctx.status_tx)).info(format!(
        "NETCONF session {session_id} on connection {id}: user {username}, {}",
        if framing == Framing::Chunked { "base:1.1 chunked framing" } else { "base:1.0 end-of-message framing" }
    ));
    loop {
        let before = received;
        let Some(message) = read_message(io, &mut decoder, shared.idle, &mut received).await? else {
            return Ok(());
        };
        ctx.state
            .update_connection_stats(ctx.server_id, id, Some(received - before), None, Some(1), None)
            .await;
        let incoming = match rpc::parse_rpc(&message, &shared.capabilities) {
            Ok(r) => r,
            Err(rpc::RpcRefusal::Reply { attributes, bindings, error }) => {
                outcome(ctx, id, "rpc", "protocol_refusal");
                send(shared, id, io, &rpc::reply(&attributes, &bindings, &rpc::ReplyBody::Errors(vec![error]))?, framing).await?;
                continue;
            }
            Err(rpc::RpcRefusal::Fatal(e)) => {
                outcome(ctx, id, "rpc", "protocol_refusal");
                let error = rpc::RpcError::new("rpc", "malformed-message", "the message is not a NETCONF <rpc>");
                let _ = send(shared, id, io, &rpc::reply(&[], &[], &rpc::ReplyBody::Errors(vec![error]))?, framing).await;
                return Err(e);
            }
        };
        let reply_with = |body: rpc::ReplyBody| rpc::reply(&incoming.attributes, &incoming.bindings, &body);
        let base = incoming.namespace == rpc::NC;
        match (base, incoming.operation.as_str()) {
            (true, "close-session") => {
                send(shared, id, io, &reply_with(rpc::ReplyBody::Ok)?, framing).await?;
                return Ok(());
            }
            (true, "kill-session") => {
                let target = incoming.fields["session_id"].as_u64().unwrap_or(0) as u32;
                let body = if target == session_id {
                    rpc::ReplyBody::Errors(vec![rpc::RpcError::new("protocol", "invalid-value", "a session cannot kill itself; use close-session")])
                } else if let Some(token) = shared.sessions.lock().await.remove(&target) {
                    token.cancel();
                    rpc::ReplyBody::Ok
                } else {
                    rpc::ReplyBody::Errors(vec![rpc::RpcError::new("protocol", "invalid-value", "no such session")])
                };
                send(shared, id, io, &reply_with(body)?, framing).await?;
                continue;
            }
            _ => {}
        }
        let mut data = Value::Object(incoming.fields.clone());
        data["session_id"] = json!(session_id);
        data["username"] = json!(username);
        let decision = decide(shared, owner, id, Event::new(&actions::RPC_EVENT, data), "netconf_rpc_reply").await;
        let wants_data = base && matches!(incoming.operation.as_str(), "get" | "get-config");
        let failed = || rpc::ReplyBody::Errors(vec![rpc::RpcError::new("application", "operation-failed", "the server cannot answer this request right now")]);
        let body = match decision {
            // `decide` has already logged which way it failed.
            Err(e) if owner.is_cancelled() => return Err(e),
            Err(_) => failed(),
            Ok(v) => match actions::reply_body(&v) {
                Ok(body) => {
                    let fits = match &body {
                        rpc::ReplyBody::Errors(_) => true,
                        rpc::ReplyBody::Data(_) => wants_data,
                        rpc::ReplyBody::Ok => !wants_data,
                        rpc::ReplyBody::Output(_) => !base,
                    };
                    if fits {
                        let tag = if matches!(body, rpc::ReplyBody::Errors(_)) { "model_reject" } else { "model_answer" };
                        outcome(ctx, id, &incoming.operation, tag);
                        body
                    } else {
                        outcome(ctx, id, &incoming.operation, "fail_closed_invalid_reply");
                        failed()
                    }
                }
                Err(_) => {
                    outcome(ctx, id, &incoming.operation, "fail_closed_invalid_reply");
                    failed()
                }
            },
        };
        let rendered = match reply_with(body) {
            Ok(r) => r,
            Err(_) => {
                outcome(ctx, id, &incoming.operation, "fail_closed_invalid_reply");
                reply_with(rpc::ReplyBody::Errors(vec![rpc::RpcError::new("application", "too-big", "the reply exceeds the message bound")]))?
            }
        };
        send(shared, id, io, &rendered, framing).await?;
    }
}
