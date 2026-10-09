//! ZeroMQ server socket (REP, ROUTER or PULL) speaking ZMTP 3.1 with the NULL mechanism.
//! Rust owns the greeting, READY, the compatibility check, framing, envelopes, heartbeats
//! and every bound; the handler writes the reply to each message.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};
use wire::Incoming;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
pub const DEFAULT_SOCKET_TYPE: &str = "rep";

#[derive(Clone)]
struct Config {
    socket_type: &'static str,
    idle: Duration,
}

fn config(ctx: &SpawnContext) -> Result<Config> {
    let params = ctx.startup_params.as_ref();
    let socket_type = match params
        .map(|p| p.get_optional_string("socket_type"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_SOCKET_TYPE.into())
        .to_ascii_lowercase()
        .as_str()
    {
        "rep" => "REP",
        "router" => "ROUTER",
        "pull" => "PULL",
        other => anyhow::bail!("socket_type must be rep, router or pull, not {other}"),
    };
    let secs = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Config {
        socket_type,
        idle: Duration::from_secs(secs),
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let cfg = config(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ZeroMQ {} socket listening on {addr}",
        cfg.socket_type
    ));
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
                "ZeroMQ",
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
            let cfg = cfg.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, id, socket, peer, &cfg).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("ZeroMQ connection {id} ended: {e}"));
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

fn outcome(ctx: &SpawnContext, id: ConnectionId, decision: &str) {
    let summary = format!("ZeroMQ connection {id} operation=zmq_message decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The handler's answer: `Some(frames)` to reply, `None` for no reply (ignored, silent or
/// failed — each already logged).
async fn decide(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    socket_type: &str,
) -> Option<Vec<Vec<u8>>> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::ZeromqProtocol,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            outcome(ctx, id, "fail_closed_llm_error");
            return None;
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, "fail_closed_invalid_reply");
        return None;
    }
    let mut found = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name.starts_with("zmq_") => {
                found.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match found.as_slice() {
        [] => {
            outcome(ctx, id, "model_silent");
            None
        }
        // On PULL, ignoring is the normal answer; elsewhere it withholds a reply.
        [(name, _)] if name == "zmq_ignore" => {
            outcome(
                ctx,
                id,
                if socket_type == "PULL" {
                    "model_answer"
                } else {
                    "model_reject"
                },
            );
            None
        }
        [(_, data)] => match wire::frames_from_text(
            data["frames"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default(),
            data["encoding"].as_str(),
        ) {
            Ok(frames) => {
                outcome(ctx, id, "model_answer");
                Some(frames)
            }
            Err(_) => {
                outcome(ctx, id, "fail_closed_invalid_reply");
                None
            }
        },
        _ => {
            outcome(ctx, id, "fail_closed_invalid_reply");
            None
        }
    }
}

async fn write<W: tokio::io::AsyncWrite + Unpin>(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut W,
    bytes: &[u8],
) -> Result<()> {
    tokio::time::timeout(wire::HANDSHAKE_TIMEOUT, w.write_all(bytes))
        .await
        .context("ZMTP write deadline")??;
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

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    mut socket: TcpStream,
    peer: SocketAddr,
    cfg: &Config,
) -> Result<()> {
    let (peer_type, peer_identity) = wire::handshake(&mut socket, cfg.socket_type, None).await?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ZeroMQ connection {id}: {} peer from {peer}",
        peer_type
    ));
    let (mut reader, mut writer) = tokio::io::split(socket);
    loop {
        let Some(incoming) = wire::read_incoming(&mut reader, cfg.idle).await? else {
            return Ok(());
        };
        let mut frames = match incoming {
            Incoming::Command(name, body) => {
                if name == "PING" && body.len() >= 2 {
                    // PONG echoes the ping's context (after its 2-byte TTL).
                    write(
                        ctx,
                        id,
                        &mut writer,
                        &wire::encode_command("PONG", &body[2..]),
                    )
                    .await?;
                }
                continue;
            }
            Incoming::Message(frames) => frames,
        };
        let size: usize = frames.iter().map(Vec::len).sum();
        ctx.state
            .update_connection_stats(ctx.server_id, id, Some(size as u64), None, Some(1), None)
            .await;
        // A REQ peer's request starts with an empty delimiter frame: the envelope.
        let envelope = cfg.socket_type != "PULL" && frames.first().is_some_and(Vec::is_empty);
        if envelope {
            frames.remove(0);
        } else if cfg.socket_type == "REP" {
            anyhow::bail!("REQ request without its empty delimiter frame");
        }
        let (text, encoding) = wire::frames_to_text(&frames);
        let event = Event::new(
            &actions::MESSAGE_EVENT,
            json!({
                "socket_type": cfg.socket_type,
                "peer_socket_type": peer_type,
                "peer_identity": (!peer_identity.is_empty()).then(|| String::from_utf8_lossy(&peer_identity).to_string()),
                "frames": text,
                "encoding": encoding,
                "remote_addr": peer.to_string(),
            }),
        );
        let reply = decide(ctx, id, event, cfg.socket_type).await;
        match (cfg.socket_type, reply) {
            ("PULL", Some(_)) => {
                Log::new(Some(&ctx.status_tx)).warn(format!(
                    "ZeroMQ connection {id}: a PULL socket cannot reply; dropped"
                ));
            }
            (_, Some(mut frames)) => {
                if envelope {
                    frames.insert(0, Vec::new());
                }
                write(ctx, id, &mut writer, &wire::encode_message(&frames)).await?;
            }
            // REP must answer in turn: with no answer the peer would wait forever.
            ("REP", None) => return Ok(()),
            (_, None) => {}
        }
    }
}
