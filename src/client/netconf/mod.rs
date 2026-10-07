//! NETCONF client over SSH: pinned host key, password auth, the `netconf` subsystem, and one
//! correlated RPC at a time.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::netconf::{
    owned_stream::{OwnedStream, Owner},
    rpc,
    wire::{self, Decoder, Framing},
    HANDSHAKE_TIMEOUT, WRITE_TIMEOUT,
};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::NetconfClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use russh::client;
use russh::ChannelMsg;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

struct Pinned {
    expected: String,
}
#[async_trait::async_trait]
impl client::Handler for Pinned {
    type Error = anyhow::Error;
    async fn check_server_key(&mut self, key: &russh_keys::key::PublicKey) -> Result<bool, Self::Error> {
        Ok(key.fingerprint() == self.expected)
    }
}

fn pin(value: &str) -> Result<String> {
    use base64::Engine;
    let hash = value.strip_prefix("SHA256:").context("host_key_sha256 must start with SHA256:")?;
    ensure!(
        base64::engine::general_purpose::STANDARD_NO_PAD.decode(hash).map(|v| v.len()) .ok() == Some(32),
        "host_key_sha256 must hold a base64 SHA-256 digest"
    );
    Ok(hash.to_owned())
}

/// The live session: everything that must go away together when the client stops.
struct Live<S> {
    io: S,
    decoder: Decoder,
    framing: Framing,
    server_capabilities: Vec<String>,
    next_id: u64,
    reply_timeout: Duration,
}

async fn read_message<R: AsyncRead + Unpin>(io: &mut R, decoder: &mut Decoder) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        if let Some(m) = decoder.next_message()? {
            return Ok(Some(m));
        }
        let n = io.read(&mut buf).await?;
        if n == 0 {
            ensure!(!decoder.is_partial(), "NETCONF server closed mid-message");
            return Ok(None);
        }
        decoder.feed(&buf[..n])?;
    }
}

async fn write_message<W: AsyncWrite + Unpin>(io: &mut W, message: &[u8], framing: Framing) -> Result<usize> {
    let framed = wire::frame(message, framing)?;
    tokio::time::timeout(WRITE_TIMEOUT, async {
        io.write_all(&framed).await?;
        io.flush().await
    })
    .await
    .context("NETCONF write deadline")??;
    Ok(framed.len())
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref().context("NETCONF client needs username, password and host_key_sha256")?;
    let username = p.get_string("username")?;
    let password = p.get_string("password")?;
    let expected = pin(&p.get_string("host_key_sha256")?)?;
    let versions = match p.get_optional_array("base_versions")? {
        None => vec![rpc::BASE_10.to_owned(), rpc::BASE_11.to_owned()],
        Some(list) => {
            let mut out = Vec::new();
            for v in list {
                match v.as_str() {
                    Some("1.0") => out.push(rpc::BASE_10.to_owned()),
                    Some("1.1") => out.push(rpc::BASE_11.to_owned()),
                    _ => bail!("base_versions entries must be \"1.0\" or \"1.1\""),
                }
            }
            ensure!(!out.is_empty(), "base_versions must not be empty");
            out
        }
    };
    let handshake = crate::server::netconf::seconds(p.get_optional_u64("handshake_timeout_secs")?, HANDSHAKE_TIMEOUT, 600, "handshake_timeout_secs")?;
    let reply_timeout = crate::server::netconf::seconds(p.get_optional_u64("reply_timeout_secs")?, REPLY_TIMEOUT, 3600, "reply_timeout_secs")?;

    let (owner, handle, live, local, session_id) = tokio::time::timeout(handshake, async {
        let socket = tokio::net::TcpStream::connect(&ctx.remote_addr).await.context("NETCONF TCP connect")?;
        let local = socket.local_addr()?;
        let (stream, owner) = OwnedStream::new(socket);
        let config = Arc::new(client::Config { inactivity_timeout: None, ..Default::default() });
        let mut handle = client::connect_stream(config, stream, Pinned { expected })
            .await
            .context("NETCONF SSH handshake failed (a host key that does not match host_key_sha256 is refused)")?;
        ensure!(
            handle.authenticate_password(username.clone(), password).await.context("NETCONF SSH authentication")?,
            "NETCONF SSH authentication refused for '{username}'"
        );
        let mut channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "netconf").await?;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::WindowAdjusted { .. }) => {}
                _ => bail!("NETCONF server refused the netconf subsystem"),
            }
        }
        let mut io = channel.into_stream();
        write_message(&mut io, &rpc::hello(&versions, None)?, Framing::Delimiter).await?;
        let mut decoder = Decoder::new(Framing::Delimiter);
        let hello = read_message(&mut io, &mut decoder).await?.context("NETCONF server closed before <hello>")?;
        let hello = rpc::parse_hello(&hello)?;
        let session_id = hello.session_id.context("NETCONF server <hello> lacks a session-id")?;
        let framing = rpc::negotiate(&versions, &hello.capabilities).context("NETCONF server shares no base version")?;
        decoder.set_framing(framing)?;
        Ok::<_, anyhow::Error>((
            owner,
            handle,
            Live { io, decoder, framing, server_capabilities: hello.capabilities, next_id: 1, reply_timeout },
            local,
            session_id,
        ))
    })
    .await
    .context("NETCONF handshake deadline exceeded")??;

    ctx.state.update_client_status(ctx.client_id, ClientStatus::Connected).await;
    let external = crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({
            "session_id": session_id,
            "server_capabilities": live.server_capabilities,
            "base_version": if live.framing == Framing::Chunked { "1.1" } else { "1.0" },
        }),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = NetconfClientProtocol;
        while let Some(event) = event_rx.recv().await {
            let instruction = events_ctx.state.get_instruction_for_client(events_ctx.client_id).await.unwrap_or_default();
            let memory = events_ctx.state.get_memory_for_client(events_ctx.client_id).await.unwrap_or_default();
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
                        events_ctx.state.set_memory_for_client(events_ctx.client_id, memory).await;
                    }
                    for action in result.actions {
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx)).warn(format!("NETCONF client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state.register_client_task(ctx.client_id, dispatcher).await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, live, external, internal_rx, event_tx).await;
        // Hang up the SSH connection before the owner drops it out from under the driver.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            handle.disconnect(russh::Disconnect::ByApplication, "", "en"),
        )
        .await;
        drop::<Owner>(owner);
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("NETCONF client ended: {e}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx.state.update_client_status(session_ctx.client_id, status).await;
        session_ctx.state.remove_client_handle(session_ctx.client_id).await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

fn reject(command: Option<ClientCommand>, error: String) {
    if let Some(command) = command {
        crate::client::command_support::reply(command, Ok(ClientSendOutcome::Rejected { error }));
    }
}

async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    ctx: &ConnectContext,
    mut live: Live<S>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            command = external.recv() => match command { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            action = internal.recv() => match action { Some(a) => (a, None), None => return Ok(()) },
            unsolicited = read_message(&mut live.io, &mut live.decoder) => {
                // Notifications are not negotiated, so anything here is a protocol error.
                return match unsolicited? {
                    None => Ok(()),
                    Some(_) => bail!("NETCONF server sent a message with no request pending"),
                };
            }
        };
        let data = match NetconfClientProtocol.execute_action(action) {
            Ok(ClientActionResult::Custom { data, .. }) => data,
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = command {
                    crate::client::command_support::reply(command, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => {
                reject(command, "unsupported action result".into());
                continue;
            }
            Err(e) => {
                reject(command, e.to_string());
                continue;
            }
        };
        let message_id = live.next_id;
        let (operation, message) = match rpc::build_rpc(message_id, &data, &live.server_capabilities) {
            Ok(v) => v,
            Err(e) => {
                reject(command, e.to_string());
                continue;
            }
        };
        live.next_id += 1;
        let written = write_message(&mut live.io, &message, live.framing).await;
        if command.is_some() {
            // Operation only: configuration content may carry secrets.
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "NETCONF",
                    None,
                    "injected_action",
                    json!({"operation": operation, "message_id": message_id}),
                    vec![json!({"sent": written.is_ok()})],
                )
                .await;
        }
        if let Some(command) = command {
            crate::client::command_support::reply(
                command,
                match &written {
                    Ok(n) => Ok(ClientSendOutcome::Sent { bytes_sent: *n }),
                    Err(e) => Err(anyhow::anyhow!(e.to_string())),
                },
            );
        }
        written?;
        let reply = {
            let pending = tokio::time::timeout(live.reply_timeout, read_message(&mut live.io, &mut live.decoder));
            tokio::pin!(pending);
            loop {
                tokio::select! {
                    biased;
                    r = &mut pending => break r.context("NETCONF reply deadline")??.context("NETCONF server closed before replying")?,
                    command = external.recv() => {
                        let Some(command) = command else { return Ok(()) };
                        if matches!(NetconfClientProtocol.execute_action(command.action.clone()), Ok(ClientActionResult::Disconnect)) {
                            crate::client::command_support::reply(command, Ok(ClientSendOutcome::Disconnected));
                            return Ok(());
                        }
                        reject(Some(command), "a NETCONF request is already pending; retry after its reply".into());
                    }
                }
            }
        };
        let parsed = rpc::parse_reply(&reply)?;
        ensure!(
            parsed.message_id.as_deref() == Some(message_id.to_string().as_str()),
            "NETCONF reply message-id {:?} does not match request {message_id}",
            parsed.message_id
        );
        let mut data = parsed.body;
        data["operation"] = json!(operation);
        data["message_id"] = json!(message_id.to_string());
        let closed = operation == "close-session" && data["ok"] == true;
        events
            .try_send(Event::new(&actions::REPLY_EVENT, data))
            .context("NETCONF event queue full; consumer stalled")?;
        if closed {
            let _ = tokio::time::timeout(Duration::from_millis(250), live.io.shutdown()).await;
            return Ok(());
        }
    }
}
