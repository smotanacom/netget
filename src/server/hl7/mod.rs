//! HL7 v2 over MLLP. Rust frames, parses and builds the acknowledgment envelope; the
//! handler decides what each message is acknowledged with.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

struct Shared {
    ctx: SpawnContext,
    idle: Duration,
    next_control: AtomicU64,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let idle = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=86400).contains(&idle),
        "idle_timeout_secs must be 1..=86400"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("HL7 MLLP listening on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        idle: Duration::from_secs(idle),
        next_control: AtomicU64::new(1),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (socket, peer, permit) =
                match accept_bounded(&listener, &limiter, b"", "HL7", Some(&shared.ctx.status_tx))
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
                        local_addr: local,
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
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = connection(&child, id, socket).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("HL7 connection {id} ended: {e}"));
                    }
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("HL7 connection {id} message={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

async fn decide(shared: &Shared, id: ConnectionId, event: Event) -> Result<Value> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::Hl7Protocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, event.id(), "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
        bail!("HL7 handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == "hl7_ack" => {
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("HL7 handler supplied more than one acknowledgment");
                }
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    found
        .context("HL7 handler did not acknowledge")
        .inspect_err(|_| outcome(ctx, id, event.id(), "model_silent"))
}

async fn connection(
    shared: &Shared,
    id: ConnectionId,
    socket: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let (mut reader, mut writer) = tokio::io::split(socket);
    loop {
        let Some(bytes) = wire::read_frame(&mut reader, shared.idle).await? else {
            return Ok(());
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(bytes.len() as u64 + 3),
                None,
                Some(1),
                None,
            )
            .await;
        // An unparseable message cannot be acknowledged: there is no MSH to answer.
        let message = match wire::parse(&bytes) {
            Ok(m) => m,
            Err(e) => {
                outcome(ctx, id, "unparseable", "protocol_refusal");
                return Err(e.context("HL7 message could not be parsed; no MSH to acknowledge"));
            }
        };
        let control = format!("NG{}", shared.next_control.fetch_add(1, Ordering::Relaxed));
        let label = message.message_type().to_owned();
        let decision = decide(
            shared,
            id,
            Event::new(&actions::MESSAGE_EVENT, message.to_event()),
        )
        .await;
        // The peer gets a category, the log gets the reason.
        let fallback = || {
            let error = json!({"code": "207^Application internal error^HL70357", "severity": "E", "message": "the receiver cannot process this message right now"});
            wire::ack(
                &message,
                "AE",
                "application error",
                &control,
                Some(&error),
                &[],
            )
        };
        let reply = match decision {
            // `decide` has logged which way it failed.
            Err(_) => fallback(),
            Ok(v) => match actions::validate_ack(&v)
                .and_then(|_| wire::segments_from(v.get("segments").unwrap_or(&Value::Null)))
            {
                Ok(extra) => {
                    let code = v["code"].as_str().unwrap_or("AE");
                    outcome(
                        ctx,
                        id,
                        &label,
                        if matches!(code, "AA" | "CA") {
                            "model_accept"
                        } else {
                            "model_reject"
                        },
                    );
                    wire::ack(
                        &message,
                        code,
                        v["text"].as_str().unwrap_or(""),
                        &control,
                        v.get("error").filter(|e| !e.is_null()),
                        &extra,
                    )
                }
                Err(_) => {
                    outcome(ctx, id, &label, "fail_closed_invalid_reply");
                    fallback()
                }
            },
        }?;
        let n = wire::write_frame(&mut writer, &reply).await?;
        ctx.state
            .update_connection_stats(ctx.server_id, id, None, Some(n as u64), None, Some(1))
            .await;
    }
}
