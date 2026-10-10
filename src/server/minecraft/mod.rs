//! Minecraft Java Edition server: the server-list ping (modern and legacy) and the front of
//! login. Rust owns framing, the handshake, ping/pong and every bound; the handler decides the
//! server-list entry and the reason a joining player is disconnected. No game is run.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// Longest per-packet deadline a caller may configure.
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 300;

fn idle(ctx: &SpawnContext) -> Result<Duration> {
    let secs = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(wire::IO_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Duration::from_secs(secs))
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let idle = idle(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("Minecraft listening on {addr}"));
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"",
                "Minecraft",
                Some(&ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
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
            let child = ctx.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, id, socket, peer, idle).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("Minecraft connection {id} ended: {e}"));
                    }
                    child
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(addr)
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut W,
    bytes: &[u8],
) -> Result<()> {
    tokio::time::timeout(wire::IO_TIMEOUT, async {
        w.write_all(bytes).await?;
        w.flush().await
    })
    .await
    .context("Minecraft write deadline")??;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            None,
            Some(bytes.len() as u64),
            None,
            Some(1),
        )
        .await;
    Ok(())
}

async fn received(ctx: &SpawnContext, id: ConnectionId, len: usize) {
    ctx.state
        .update_connection_stats(ctx.server_id, id, Some(len as u64), None, Some(1), None)
        .await;
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("Minecraft connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// Ask the handler and return the one answer it gave: (action name, data). `Ok(None)` when it
/// gave none (`model_silent`, already logged); `Err` on a failure (already logged).
async fn decide(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
) -> Result<Option<(String, Value)>> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::MinecraftProtocol,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(error);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        anyhow::bail!("Minecraft handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name.starts_with("minecraft_") => {
                if found.is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    anyhow::bail!("Multiple Minecraft answers to one request");
                }
                found = Some((name, data));
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    if found.is_none() {
        outcome(ctx, id, event.id(), "model_silent");
    }
    Ok(found)
}

/// A status answer for this event, or `None` when the connection should close unanswered.
async fn status_answer(ctx: &SpawnContext, id: ConnectionId, event: Event) -> Option<Value> {
    match decide(ctx, id, event).await {
        Ok(Some((name, data))) if name == "minecraft_status" => {
            outcome(ctx, id, "minecraft_status_request", "model_answer");
            Some(data)
        }
        Ok(Some((name, _))) if name == "minecraft_refuse" => {
            outcome(ctx, id, "minecraft_status_request", "model_reject");
            None
        }
        Ok(Some(_)) => {
            outcome(
                ctx,
                id,
                "minecraft_status_request",
                "fail_closed_invalid_reply",
            );
            None
        }
        // A ping the handler did not answer is closed: a fabricated status would assert a
        // server-list entry nobody decided.
        Ok(None) | Err(_) => None,
    }
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    idle: Duration,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    let first = match tokio::time::timeout(idle, reader.read_u8()).await {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return Ok(()),
        Err(_) => anyhow::bail!("no handshake within {}s", idle.as_secs()),
    };
    if first == 0xFE {
        let ping = wire::read_legacy_ping(&mut reader).await?;
        received(ctx, id, 1).await;
        let event = Event::new(
            &actions::STATUS_EVENT,
            json!({
                "protocol_version": ping.protocol_version,
                "server_address": ping.server_address,
                "server_port": ping.server_port,
                "legacy": true,
                "remote_addr": peer.to_string(),
            }),
        );
        if let Some(answer) = status_answer(ctx, id, event).await {
            let reply = wire::encode_legacy_reply(&ping, &answer)?;
            send(ctx, id, &mut writer, &reply).await?;
        }
        return Ok(());
    }
    let first = [first];
    let mut chained = (&first[..]).chain(&mut reader);
    let Some((packet_id, body)) =
        wire::read_packet(&mut chained, wire::MAX_SERVERBOUND_PACKET, idle).await?
    else {
        return Ok(());
    };
    received(ctx, id, body.len() + 2).await;
    anyhow::ensure!(
        packet_id == 0x00,
        "first packet is 0x{packet_id:02x}, not a handshake"
    );
    let handshake = wire::parse_handshake(&body)?;
    if handshake.next_state == wire::STATE_STATUS {
        status_state(ctx, id, &mut reader, &mut writer, peer, idle, &handshake).await
    } else {
        login_state(ctx, id, &mut reader, &mut writer, peer, idle, &handshake).await
    }
}

async fn status_state<R, W>(
    ctx: &SpawnContext,
    id: ConnectionId,
    reader: &mut R,
    writer: &mut W,
    peer: SocketAddr,
    idle: Duration,
    handshake: &wire::Handshake,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut answered = false;
    loop {
        let Some((packet_id, body)) =
            wire::read_packet(reader, wire::MAX_SERVERBOUND_PACKET, idle).await?
        else {
            return Ok(());
        };
        received(ctx, id, body.len() + 2).await;
        match packet_id {
            // One status request per connection, as the vanilla server allows.
            0x00 if !answered && body.is_empty() => {
                answered = true;
                let event = Event::new(
                    &actions::STATUS_EVENT,
                    json!({
                        "protocol_version": handshake.protocol_version,
                        "server_address": handshake.server_address,
                        "server_port": handshake.server_port,
                        "legacy": false,
                        "remote_addr": peer.to_string(),
                    }),
                );
                let Some(answer) = status_answer(ctx, id, event).await else {
                    return Ok(());
                };
                let json = wire::status_json(&answer, handshake.protocol_version)?;
                let mut body = Vec::new();
                wire::put_string(&mut body, &json);
                send(ctx, id, writer, &wire::frame(0x00, &body)).await?;
            }
            // Ping: echo the payload and close, as the vanilla server does.
            0x01 if body.len() == 8 => {
                send(ctx, id, writer, &wire::frame(0x01, &body)).await?;
                return Ok(());
            }
            other => anyhow::bail!(
                "unexpected status packet 0x{other:02x} ({} bytes)",
                body.len()
            ),
        }
    }
}

async fn login_state<R, W>(
    ctx: &SpawnContext,
    id: ConnectionId,
    reader: &mut R,
    writer: &mut W,
    peer: SocketAddr,
    idle: Duration,
    handshake: &wire::Handshake,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let Some((packet_id, body)) =
        wire::read_packet(reader, wire::MAX_SERVERBOUND_PACKET, idle).await?
    else {
        return Ok(());
    };
    received(ctx, id, body.len() + 2).await;
    anyhow::ensure!(
        packet_id == 0x00,
        "login packet 0x{packet_id:02x} is not Login Start"
    );
    let (username, uuid) = wire::parse_login_start(&body)?;
    let event = Event::new(
        &actions::LOGIN_EVENT,
        json!({
            "username": username,
            "uuid": uuid,
            "protocol_version": handshake.protocol_version,
            "server_address": handshake.server_address,
            "server_port": handshake.server_port,
            "transfer": handshake.next_state == wire::STATE_TRANSFER,
            "remote_addr": peer.to_string(),
        }),
    );
    let reason = match decide(ctx, id, event).await {
        Ok(Some((name, data))) if name == "minecraft_disconnect" => {
            outcome(ctx, id, "minecraft_login", "model_answer");
            data["reason"].as_str().unwrap_or_default().to_string()
        }
        Ok(Some((name, _))) if name == "minecraft_refuse" => {
            outcome(ctx, id, "minecraft_login", "model_reject");
            return Ok(());
        }
        Ok(Some(_)) => {
            outcome(ctx, id, "minecraft_login", "fail_closed_invalid_reply");
            crate::utils::WireFailure::Unavailable
                .prefixed_text()
                .to_string()
        }
        // Disconnecting is a refusal, so a generic reason invents nothing about the server.
        Ok(None) => crate::utils::WireFailure::Unavailable
            .prefixed_text()
            .to_string(),
        Err(e) => crate::utils::wire_failure::prefixed_wire_failure_text(&e).to_string(),
    };
    let mut body = Vec::new();
    wire::put_string(&mut body, &wire::disconnect_json(&reason)?);
    send(ctx, id, writer, &wire::frame(0x00, &body)).await
}
