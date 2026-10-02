//! Programmable NUT attachment daemon; application data belongs to event handlers.
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
    io::{AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;

pub fn idle_timeout(seconds: Option<u64>) -> Result<Duration> {
    let seconds = seconds.unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&seconds),
        "idle_timeout_secs must be between 1 and 86400"
    );
    Ok(Duration::from_secs(seconds))
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let idle = idle_timeout(
        ctx.startup_params
            .as_ref()
            .map(|p| p.get_optional_u64("idle_timeout_secs"))
            .transpose()?
            .flatten(),
    )?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("NUT listening on {addr}"));
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
                b"ERR ACCESS-DENIED\n",
                "NUT",
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
                    if let Err(e) = session(&child, id, socket, idle).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("NUT connection {id} ended: {e}"));
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
async fn write<W: tokio::io::AsyncWrite + Unpin>(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut W,
    reply: &str,
) -> Result<()> {
    tokio::time::timeout(wire::IO_TIMEOUT, w.write_all(reply.as_bytes()))
        .await
        .context("NUT write deadline")??;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            None,
            Some(reply.len() as u64),
            None,
            Some(1),
        )
        .await;
    Ok(())
}
async fn decision(
    ctx: &SpawnContext,
    id: ConnectionId,
    event: Event,
    expected: &str,
) -> Result<Value> {
    let result = call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::NutProtocol,
    )
    .await?;
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name == expected => {
                anyhow::ensure!(found.is_none(), "Multiple NUT replies");
                found = Some(data);
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    found.context("Handler did not answer NUT request")
}
async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    idle: Duration,
) -> Result<()> {
    let (read, mut write_half) = tokio::io::split(socket);
    let mut reader = BufReader::new(read);
    let mut username: Option<String> = None;
    let mut password_set = false;
    let mut authenticated = false;
    loop {
        let line = match wire::read_line(&mut reader, idle).await {
            Ok(Some(line)) => line,
            Ok(None) => return Ok(()),
            Err(e) => {
                let _ = write(ctx, id, &mut write_half, "ERR INVALID-ARGUMENT\n").await;
                return Err(e);
            }
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some((line.len() + 1) as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let fixed = match line.as_str() {
            "STARTTLS" => Some("ERR FEATURE-NOT-SUPPORTED\n"),
            "VER" => Some("Network UPS Tools netget\n"),
            "HELP" => Some("Commands: HELP VER GET LIST SET INSTCMD USERNAME PASSWORD LOGOUT\n"),
            _ => None,
        };
        if let Some(reply) = fixed {
            write(ctx, id, &mut write_half, reply).await?;
            continue;
        }
        let request = match wire::Request::parse(&line) {
            Ok(r) => r,
            Err(_) => {
                write(ctx, id, &mut write_half, "ERR UNKNOWN-COMMAND\n").await?;
                continue;
            }
        };
        if request.operation == "logout" {
            write(ctx, id, &mut write_half, "OK Goodbye\n").await?;
            write_half.shutdown().await?;
            return Ok(());
        }
        if request.operation == "username" {
            if username.is_some() {
                write(ctx, id, &mut write_half, "ERR ALREADY-SET\n").await?;
            } else {
                username = request.value;
                write(ctx, id, &mut write_half, "OK\n").await?;
            }
            continue;
        }
        if request.operation == "password" {
            let reply = if password_set {
                "ERR ALREADY-SET\n"
            } else if username.is_none() {
                "ERR USERNAME-REQUIRED\n"
            } else {
                password_set = true;
                let result = decision(
                    ctx,
                    id,
                    Event::new(
                        &actions::AUTH_EVENT,
                        json!({"username":username,"password":request.value}),
                    ),
                    "nut_auth_decision",
                )
                .await;
                authenticated = result.is_ok_and(|v| v["allowed"] == true);
                if authenticated {
                    "OK\n"
                } else {
                    "ERR ACCESS-DENIED\n"
                }
            };
            write(ctx, id, &mut write_half, reply).await?;
            continue;
        }
        if request.is_write() && !authenticated {
            write(ctx, id, &mut write_half, "ERR ACCESS-DENIED\n").await?;
            continue;
        }
        let mut data = json!(request);
        data["username"] = json!(if authenticated {
            username.clone()
        } else {
            None
        });
        let result = decision(
            ctx,
            id,
            Event::new(&actions::REQUEST_EVENT, data),
            "nut_reply",
        )
        .await
        .and_then(|v| wire::render(&request, &v));
        match result {
            Ok(reply) => write(ctx, id, &mut write_half, &reply).await?,
            Err(e) => {
                Log::new(Some(&ctx.status_tx))
                    .warn(format!("NUT handler failed for {}: {e}", request.operation));
                write(ctx, id, &mut write_half, "ERR DATA-STALE\n").await?;
                return Ok(());
            }
        }
    }
}
