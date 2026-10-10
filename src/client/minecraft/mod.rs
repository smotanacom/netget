//! Minecraft Java Edition client: server-list pings (modern and legacy) and login probes,
//! each over a fresh TCP connection, as a launcher's server list does.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::minecraft::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::MinecraftClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// Packets read during one login attempt before giving up on reaching a verdict.
pub const MAX_LOGIN_PACKETS: usize = 16;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let (host, port) = split_host_port(&ctx.remote_addr)?;
    tokio::net::lookup_host((host.as_str(), port))
        .await
        .context("resolve the server address")?
        .next()
        .context("the server address resolved to nothing")?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::READY_EVENT,
        json!({"remote_addr": ctx.remote_addr}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = MinecraftClientProtocol;
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
                    .warn(format!("Minecraft client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &host, port, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Minecraft client ended: {e}"));
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
    Ok("0.0.0.0:0".parse()?)
}

/// `host:port`, `[v6]:port`, or a bare host (port 25565).
pub fn split_host_port(addr: &str) -> Result<(String, u16)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').context("unclosed [ in address")?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().context("port")?,
            None => 25565,
        };
        return Ok((host.to_string(), port));
    }
    match addr.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            Ok((host.to_string(), port.parse().context("port")?))
        }
        _ => Ok((addr.to_string(), 25565)),
    }
}

/// Per-connection framing state: compression switches on during login.
struct Conn {
    stream: TcpStream,
    threshold: Option<i32>,
}

impl Conn {
    async fn open(host: &str, port: u16) -> Result<Self> {
        let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect((host, port)))
            .await
            .context("Minecraft connect deadline")??;
        Ok(Self {
            stream,
            threshold: None,
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        tokio::time::timeout(wire::IO_TIMEOUT, self.stream.write_all(bytes))
            .await
            .context("Minecraft write deadline")??;
        Ok(())
    }

    /// Send one packet, in the compressed layout once compression is on (always below any
    /// threshold here, so uncompressed with a zero data length).
    async fn send(&mut self, id: i32, body: &[u8]) -> Result<()> {
        let bytes = if self.threshold.is_some() {
            let mut inner = vec![0u8];
            wire::put_varint(&mut inner, id);
            inner.extend_from_slice(body);
            let mut out = Vec::new();
            wire::put_varint(&mut out, inner.len() as i32);
            out.extend_from_slice(&inner);
            out
        } else {
            wire::frame(id, body)
        };
        self.write(&bytes).await
    }

    async fn recv(&mut self) -> Result<(i32, Vec<u8>)> {
        let raw = wire::read_raw(
            &mut self.stream,
            wire::MAX_CLIENTBOUND_PACKET,
            wire::IO_TIMEOUT,
        )
        .await?
        .context("server closed the connection")?;
        if self.threshold.is_none() {
            return wire::split_id(&raw);
        }
        let (data_len, n) = wire::get_varint(&raw)?;
        if data_len == 0 {
            return wire::split_id(&raw[n..]);
        }
        anyhow::ensure!(
            data_len > 0 && data_len as usize <= wire::MAX_CLIENTBOUND_PACKET,
            "compressed packet declares {data_len} bytes"
        );
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(&raw[n..])
            .take(data_len as u64 + 1)
            .read_to_end(&mut inflated)
            .context("inflate a compressed packet")?;
        anyhow::ensure!(
            inflated.len() == data_len as usize,
            "compressed packet inflated to {} bytes, not {data_len}",
            inflated.len()
        );
        wire::split_id(&inflated)
    }
}

use std::io::Read as _;

async fn status(host: &str, port: u16, protocol: i32) -> Result<Value> {
    let mut conn = Conn::open(host, port).await?;
    let mut out = wire::encode_handshake(&wire::Handshake {
        protocol_version: protocol,
        server_address: host.to_string(),
        server_port: port,
        next_state: wire::STATE_STATUS,
    });
    out.extend_from_slice(&wire::frame(0x00, &[]));
    conn.write(&out).await?;
    let (id, body) = conn.recv().await?;
    anyhow::ensure!(id == 0x00, "status answered with packet 0x{id:02x}");
    let text = wire::Reader::new(&body).string(wire::MAX_JSON_CHARS)?;
    let status: Value = serde_json::from_str(&text).context("status is not JSON")?;
    let token = crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(1);
    let started = crate::utils::clock::Instant::now();
    conn.send(0x01, &token.to_be_bytes()).await?;
    // A server that answers status but not ping still gave its entry; latency stays null.
    let latency = match conn.recv().await {
        Ok((0x01, body)) if body == token.to_be_bytes() => {
            Some(started.elapsed().as_secs_f64() * 1000.0)
        }
        _ => None,
    };
    let sample: Vec<Value> = status["players"]["sample"]
        .as_array()
        .map(|s| {
            s.iter()
                .take(wire::MAX_SAMPLE)
                .map(|p| json!({"name": p["name"], "id": p["id"]}))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "legacy": false,
        "version_name": status["version"]["name"],
        "protocol": status["version"]["protocol"],
        "online_players": status["players"]["online"],
        "max_players": status["players"]["max"],
        "motd": wire::plain_text(&status["description"]),
        "sample": sample,
        "has_favicon": status.get("favicon").is_some(),
        "enforces_secure_chat": status.get("enforcesSecureChat"),
        "latency_ms": latency,
    }))
}

async fn legacy_status(host: &str, port: u16) -> Result<Value> {
    let mut conn = Conn::open(host, port).await?;
    conn.write(&wire::encode_legacy_ping(host, port)).await?;
    let reply = tokio::time::timeout(wire::IO_TIMEOUT, async {
        let kind = conn.stream.read_u8().await?;
        anyhow::ensure!(kind == 0xFF, "legacy ping answered with 0x{kind:02x}");
        let units = conn.stream.read_u16().await? as usize;
        let mut text = vec![0u8; units * 2];
        conn.stream.read_exact(&mut text).await?;
        Ok(text)
    })
    .await
    .context("legacy ping deadline")??;
    let mut event = wire::decode_legacy_reply(&reply)?;
    event["legacy"] = Value::Bool(true);
    event["motd"] = json!(event["motd"].as_str().unwrap_or_default());
    Ok(event)
}

async fn login(
    host: &str,
    port: u16,
    protocol: i32,
    username: &str,
    uuid: [u8; 16],
) -> Result<Value> {
    let mut conn = Conn::open(host, port).await?;
    let mut out = wire::encode_handshake(&wire::Handshake {
        protocol_version: protocol,
        server_address: host.to_string(),
        server_port: port,
        next_state: wire::STATE_LOGIN,
    });
    out.extend_from_slice(&wire::encode_login_start(protocol, username, &uuid));
    conn.write(&out).await?;
    let mut event = json!({});
    for _ in 0..MAX_LOGIN_PACKETS {
        let (id, body) = conn.recv().await?;
        let mut r = wire::Reader::new(&body);
        match id {
            0x00 => {
                let text = r.string(wire::MAX_JSON_CHARS)?;
                let reason = serde_json::from_str::<Value>(&text)
                    .map(|v| wire::plain_text(&v))
                    .unwrap_or(text);
                event["outcome"] = json!("disconnected");
                event["reason"] = json!(reason);
                return Ok(event);
            }
            0x01 => {
                event["outcome"] = json!("encryption_required");
                return Ok(event);
            }
            0x02 => {
                let uuid = if protocol >= 735 {
                    wire::format_uuid(r.bytes(16)?)
                } else {
                    r.string(36)?
                };
                event["outcome"] = json!("accepted");
                event["uuid"] = json!(uuid);
                event["username"] = json!(r.string(wire::MAX_USERNAME_CHARS)?);
                return Ok(event);
            }
            0x03 => {
                let threshold = r.varint()?;
                event["compression_threshold"] = json!(threshold);
                conn.threshold = Some(threshold);
            }
            // Login plugin request: answer "not understood", as a vanilla client does.
            0x04 => {
                let message_id = r.varint()?;
                let mut reply = Vec::new();
                wire::put_varint(&mut reply, message_id);
                reply.push(0);
                conn.send(0x02, &reply).await?;
            }
            // Cookie request (1.20.5+): answer with no cookie.
            0x05 => {
                let key = r.string(32767)?;
                let mut reply = Vec::new();
                wire::put_string(&mut reply, &key);
                reply.push(0);
                conn.send(0x04, &reply).await?;
            }
            other => anyhow::bail!("unexpected login packet 0x{other:02x}"),
        }
    }
    anyhow::bail!("no login verdict within {MAX_LOGIN_PACKETS} packets")
}

async fn run(host: &str, port: u16, action: &Value) -> Result<(Event, Value)> {
    match action["type"].as_str() {
        Some("minecraft_status") => {
            let data = status(host, port, actions::protocol_version(action)?).await?;
            Ok((Event::new(&actions::STATUS_EVENT, data.clone()), data))
        }
        Some("minecraft_legacy_status") => {
            let data = legacy_status(host, port).await?;
            Ok((Event::new(&actions::STATUS_EVENT, data.clone()), data))
        }
        Some("minecraft_login") => {
            let uuid = match action["uuid"].as_str() {
                Some(u) => wire::parse_uuid(u)?,
                None => [0u8; 16],
            };
            let data = login(
                host,
                port,
                actions::protocol_version(action)?,
                action["username"].as_str().unwrap_or_default(),
                uuid,
            )
            .await?;
            Ok((Event::new(&actions::LOGIN_EVENT, data.clone()), data))
        }
        _ => anyhow::bail!("Unknown Minecraft client action"),
    }
}

async fn session(
    ctx: &ConnectContext,
    host: &str,
    port: u16,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        match MinecraftClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        }
        let name = action["type"].as_str().unwrap_or_default().to_string();
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Minecraft",
                    None,
                    "injected_action",
                    json!({"action": name}),
                    vec![],
                )
                .await;
        }
        // An unreachable or misbehaving server is logged; the client stays up for the next try.
        match run(host, port, &action).await {
            Ok((event, data)) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Executed {
                            detail: data.to_string(),
                        }),
                    );
                }
                Log::new(Some(&ctx.status_tx)).info(format!("Minecraft {name}: {data}"));
                events
                    .try_send(event)
                    .context("Minecraft event queue full; consumer stalled")?;
            }
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Err(anyhow::anyhow!(e.to_string())),
                    );
                }
                Log::new(Some(&ctx.status_tx)).warn(format!("Minecraft {name} failed: {e:#}"));
            }
        }
    }
}
