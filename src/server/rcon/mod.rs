//! Source RCON server (the same framing Minecraft's RCON uses). Rust owns framing, ids,
//! splitting, the password check when one is configured, and every bound; handlers answer
//! commands, and logins when no password is configured.
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
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};
use wire::Packet;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
pub const DEFAULT_DIALECT: &str = "source";
/// Failed logins before the connection is closed.
pub const MAX_AUTH_FAILURES: u32 = 3;

#[derive(Clone)]
struct Config {
    password: Option<String>,
    source: bool,
    idle: Duration,
}

fn config(ctx: &SpawnContext) -> Result<Config> {
    let params = ctx.startup_params.as_ref();
    let password = params
        .map(|p| p.get_optional_string("password"))
        .transpose()?
        .flatten();
    if let Some(password) = &password {
        anyhow::ensure!(
            !password.is_empty() && password.len() <= wire::MAX_BODY_OUT,
            "password must be 1 to {} bytes",
            wire::MAX_BODY_OUT
        );
    }
    let source = match params
        .map(|p| p.get_optional_string("dialect"))
        .transpose()?
        .flatten()
        .as_deref()
        .unwrap_or(DEFAULT_DIALECT)
    {
        "source" => true,
        "minecraft" => false,
        other => anyhow::bail!("dialect must be source or minecraft, not {other}"),
    };
    let idle = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&idle),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Config {
        password,
        source,
        idle: Duration::from_secs(idle),
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let cfg = config(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("RCON listening on {addr}"));
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
                "RCON",
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
                    if let Err(e) = session(&child, id, socket, &cfg).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("RCON connection {id} ended: {e}"));
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
    packets: &[Packet],
) -> Result<()> {
    let bytes: Vec<u8> = packets.iter().flat_map(Packet::encode).collect();
    tokio::time::timeout(wire::IO_TIMEOUT, w.write_all(&bytes))
        .await
        .context("RCON write deadline")??;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            None,
            Some(bytes.len() as u64),
            None,
            Some(packets.len() as u64),
        )
        .await;
    Ok(())
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("RCON connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn decision(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    expected: &str,
) -> Result<Value> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RconProtocol,
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
        anyhow::bail!("RCON handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name == expected => {
                if found.is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    anyhow::bail!("Multiple RCON answers to one packet");
                }
                found = Some(data);
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    if found.is_none() {
        outcome(ctx, id, event.id(), "model_silent");
    }
    found.context("Handler did not answer the RCON packet")
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    cfg: &Config,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    let mut authenticated = false;
    let mut failures = 0u32;
    loop {
        let Some(packet) = wire::read_packet(&mut reader, cfg.idle, wire::MAX_PACKET).await? else {
            return Ok(());
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(packet.body.len() as u64 + 14),
                None,
                Some(1),
                None,
            )
            .await;
        match packet.kind {
            wire::SERVERDATA_AUTH => {
                let allowed = match &cfg.password {
                    Some(password) => wire::same_secret(&packet.body, password.as_bytes()),
                    None => {
                        let event =
                            Event::new(&actions::AUTH_EVENT, json!({"password": packet.text()}));
                        match decision(ctx, id, event, "rcon_auth_decision").await {
                            Ok(answer) => {
                                let allowed = answer["allowed"] == true;
                                outcome(
                                    ctx,
                                    id,
                                    "rcon_auth",
                                    if allowed {
                                        "model_answer"
                                    } else {
                                        "model_reject"
                                    },
                                );
                                allowed
                            }
                            Err(_) => false,
                        }
                    }
                };
                let mut reply = Vec::new();
                if cfg.source {
                    // srcds sends an empty RESPONSE_VALUE before every AUTH_RESPONSE.
                    reply.push(Packet::new(
                        packet.id,
                        wire::SERVERDATA_RESPONSE_VALUE,
                        Vec::new(),
                    ));
                }
                reply.push(Packet::new(
                    if allowed { packet.id } else { -1 },
                    wire::SERVERDATA_AUTH_RESPONSE,
                    Vec::new(),
                ));
                send(ctx, id, &mut writer, &reply).await?;
                if allowed {
                    authenticated = true;
                } else {
                    failures += 1;
                    if failures >= MAX_AUTH_FAILURES {
                        Log::new(Some(&ctx.status_tx)).warn(format!(
                            "RCON connection {id} closed after {failures} failed logins"
                        ));
                        return Ok(());
                    }
                }
            }
            wire::SERVERDATA_EXECCOMMAND if authenticated => {
                let command = packet.text();
                let event = Event::new(&actions::COMMAND_EVENT, json!({"command": command}));
                match decision(ctx, id, event, "rcon_response").await {
                    Ok(answer) => {
                        outcome(ctx, id, "rcon_command", "model_answer");
                        let body =
                            wire::body_from_text(answer["output"].as_str().unwrap_or_default());
                        let packets: Vec<Packet> = wire::chunks(&body)
                            .into_iter()
                            .map(|chunk| {
                                Packet::new(packet.id, wire::SERVERDATA_RESPONSE_VALUE, chunk)
                            })
                            .collect();
                        send(ctx, id, &mut writer, &packets).await?;
                    }
                    // No output was decided: close rather than send an empty "success".
                    Err(_) => return Ok(()),
                }
            }
            // The multi-packet sentinel: mirror an empty response with the same id, after the
            // command output it follows, so a client knows that output is complete.
            wire::SERVERDATA_RESPONSE_VALUE if authenticated => {
                send(
                    ctx,
                    id,
                    &mut writer,
                    &[Packet::new(
                        packet.id,
                        wire::SERVERDATA_RESPONSE_VALUE,
                        Vec::new(),
                    )],
                )
                .await?;
            }
            kind => {
                Log::new(Some(&ctx.status_tx)).warn(format!("RCON connection {id} sent packet type {kind} (authenticated={authenticated}); closing"));
                return Ok(());
            }
        }
    }
}
