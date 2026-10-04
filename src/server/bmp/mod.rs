//! BGP Monitoring Protocol collector (RFC 7854). Rust owns framing, the per-peer header, TLVs
//! and the embedded BGP PDUs; the handler sees each message as JSON and decides whether to keep
//! monitoring the router. A collector never writes to the router.
pub mod actions;
pub mod codec;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// Once a message has started, the rest of it must arrive within this long. Between messages a
/// router may stay silent indefinitely: BMP has no keep-alive.
pub const MESSAGE_DEADLINE: Duration = Duration::from_secs(60);

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("BMP collector on {local}"));
    let shared = Arc::new(ctx.clone());
    let server_id = ctx.server_id;
    let accept =
        tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                let (stream, peer, permit) =
                    match accept_bounded(&listener, &limiter, b"", "BMP", Some(&shared.status_tx))
                        .await
                    {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                let id = ConnectionId::new(shared.state.get_next_unified_id().await);
                let now = Instant::now();
                shared
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
                    .state
                    .spawn_server_task(server_id, async move {
                        let _permit = permit;
                        if let Err(e) = connection(&child, id, peer, stream).await {
                            Log::new(Some(&child.status_tx))
                                .debug(format!("BMP connection {id}: {e:#}"));
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
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("BMP connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The handler's answer; None closes the session.
async fn ask(ctx: &SpawnContext, id: ConnectionId, event: Event, operation: &str) -> Option<Json> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::BmpProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return None;
        }
    };
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match (result.failures.is_empty(), answers.len()) {
        (true, 1) => Some(answers.remove(0)),
        (true, 0) => {
            outcome(ctx, id, operation, "model_silent");
            None
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            None
        }
    }
}

/// Read one whole message: Ok(None) when the router closed between messages.
async fn read_message(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        if let Some(len) = codec::message_len(buf)? {
            if buf.len() >= len {
                return Ok(Some(buf.drain(..len).collect()));
            }
        }
        if !buf.is_empty() && deadline.is_none() {
            deadline = Some(tokio::time::Instant::now() + MESSAGE_DEADLINE);
        }
        let mut chunk = [0u8; 16 * 1024];
        let n = match deadline {
            Some(d) => tokio::time::timeout_at(d, stream.read(&mut chunk))
                .await
                .context("a BMP message did not finish within the deadline")??,
            None => stream.read(&mut chunk).await?,
        };
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            bail!("the router closed the session mid-message");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn connection(
    ctx: &SpawnContext,
    id: ConnectionId,
    peer: SocketAddr,
    mut stream: TcpStream,
) -> Result<()> {
    let mut buf = Vec::new();
    let mut router = peer.ip().to_string();
    let mut first = true;
    loop {
        let message = match read_message(&mut stream, &mut buf).await {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(()),
            Err(e) => {
                outcome(ctx, id, "read", "protocol_refusal");
                return Err(e);
            }
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(message.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let kind = message[5];
        if first && kind != codec::INITIATION {
            outcome(ctx, id, "read", "protocol_refusal");
            bail!("the router's first message is type {kind}, not an Initiation");
        }
        first = false;
        let (event_id, mut data) = match codec::describe(kind, &message[codec::HEADER_LEN..]) {
            Ok(Some(d)) => d,
            Ok(None) => {
                Log::new(Some(&ctx.status_tx))
                    .debug(format!("BMP connection {id}: skipped message type {kind}"));
                continue;
            }
            Err(e) => {
                outcome(ctx, id, "decode", "protocol_refusal");
                return Err(e);
            }
        };
        if kind == codec::INITIATION {
            if let Some(name) = data["sys_name"].as_str().filter(|n| !n.is_empty()) {
                router = name.to_owned();
            }
        } else {
            data["router"] = json!(router);
        }
        let event_type: &'static crate::protocol::EventType = match kind {
            codec::INITIATION => &actions::INITIATION_EVENT,
            codec::PEER_UP => &actions::PEER_UP_EVENT,
            codec::ROUTE_MONITORING => &actions::ROUTE_MONITORING_EVENT,
            codec::STATISTICS => &actions::STATISTICS_EVENT,
            codec::PEER_DOWN => &actions::PEER_DOWN_EVENT,
            codec::TERMINATION => &actions::TERMINATION_EVENT,
            _ => &actions::ROUTE_MIRRORING_EVENT,
        };
        let operation = event_id.trim_start_matches("bmp_");
        let Some(answer) = ask(ctx, id, Event::new(event_type, data), operation).await else {
            return Ok(());
        };
        if answer["type"] == "bmp_close" {
            outcome(ctx, id, operation, "model_reject");
            return Ok(());
        }
        outcome(ctx, id, operation, "model_answer");
        if kind == codec::TERMINATION {
            return Ok(());
        }
    }
}
