//! MessagePack-RPC server. Rust owns the MessagePack codec, the envelope, message ids and
//! every bound; the handler answers each request and may send notifications back.
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
use wire::Rpc;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;

fn idle(ctx: &SpawnContext) -> Result<Duration> {
    let secs = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
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
    Log::new(Some(&ctx.status_tx)).info(format!("MessagePack-RPC listening on {addr}"));
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
                "MessagePack-RPC",
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
                            .warn(format!("MessagePack-RPC connection {id} ended: {e}"));
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

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary =
        format!("MessagePack-RPC connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// What to write for one incoming message.
async fn answer(
    ctx: &SpawnContext,
    id: ConnectionId,
    rpc: Rpc,
    peer: SocketAddr,
) -> Result<Vec<u8>> {
    let (event, msgid, op) = match &rpc {
        Rpc::Request {
            msgid,
            method,
            params,
        } => (
            Event::new(
                &actions::REQUEST_EVENT,
                json!({"method": method, "params": params, "msgid": msgid, "remote_addr": peer.to_string()}),
            ),
            Some(*msgid),
            "msgpack_request",
        ),
        Rpc::Notification { method, params } => (
            Event::new(
                &actions::NOTIFICATION_EVENT,
                json!({"method": method, "params": params, "remote_addr": peer.to_string()}),
            ),
            None,
            "msgpack_notification",
        ),
        // A response from a client: NetGet's server sends no requests, so there is nothing
        // it could answer.
        Rpc::Response { .. } => anyhow::bail!("unsolicited response from the client"),
    };
    let failed = |error: Option<&anyhow::Error>| -> Result<Vec<u8>> {
        match msgid {
            Some(m) => {
                let text = match error {
                    Some(e) => crate::utils::wire_failure::prefixed_wire_failure_text(e),
                    None => crate::utils::WireFailure::Unavailable.prefixed_text(),
                };
                wire::response(m, &json!(text), &Value::Null)
            }
            None => Ok(Vec::new()),
        }
    };
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::MsgpackRpcProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, op, "fail_closed_llm_error");
            return failed(Some(&e));
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, op, "fail_closed_invalid_reply");
        return failed(None);
    }
    let mut answers = Vec::new();
    let mut notes = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == "msgpack_notify" => notes.push(data),
            ActionResult::Custom { name, data } if name.starts_with("msgpack_") => {
                answers.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    notes.reverse();
    let mut out = Vec::new();
    match (msgid, answers.as_slice()) {
        (Some(m), [(name, data)]) if name == "msgpack_result" => {
            outcome(ctx, id, op, "model_answer");
            out.extend(wire::response(m, &Value::Null, &data["result"])?);
        }
        (Some(m), [(name, data)]) if name == "msgpack_error" => {
            outcome(ctx, id, op, "model_reject");
            out.extend(wire::response(m, &data["error"], &Value::Null)?);
        }
        (Some(_), []) => {
            outcome(ctx, id, op, "model_silent");
            return failed(None);
        }
        (Some(_), _) => {
            outcome(ctx, id, op, "fail_closed_invalid_reply");
            return failed(None);
        }
        (None, []) => outcome(
            ctx,
            id,
            op,
            if notes.is_empty() {
                "model_silent"
            } else {
                "model_answer"
            },
        ),
        (None, [(name, _)]) if name == "msgpack_ignore" => outcome(ctx, id, op, "model_answer"),
        (None, _) => {
            outcome(ctx, id, op, "fail_closed_invalid_reply");
            return Ok(Vec::new());
        }
    }
    for n in notes {
        out.extend(wire::notification(
            n["method"].as_str().unwrap_or_default(),
            &n["params"],
        )?);
    }
    Ok(out)
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    idle: Duration,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(socket);
    let mut stream = wire::Stream::new(reader);
    while let Some(value) = stream.next(idle).await? {
        ctx.state
            .update_connection_stats(ctx.server_id, id, None, None, Some(1), None)
            .await;
        let rpc = wire::parse(value)?;
        let out = answer(ctx, id, rpc, peer).await?;
        if !out.is_empty() {
            tokio::time::timeout(wire::IO_TIMEOUT, writer.write_all(&out))
                .await
                .context("MessagePack-RPC write deadline")??;
            ctx.state
                .update_connection_stats(
                    ctx.server_id,
                    id,
                    None,
                    Some(out.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
    }
    Ok(())
}
