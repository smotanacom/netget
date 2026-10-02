//! One-way Fluent Forward collector with bounded stream parsing and owned tasks.
pub mod actions;
pub mod codec;
use crate::protocol::{Event, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::{
    server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
    AccessLogOwner,
};
use crate::{console_error, console_info};
use anyhow::{Context, Result};
use serde_json::json;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub struct FluentForwardServer;
impl FluentForwardServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
        let llm_fallback = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_bool("llm_fallback"))
            .transpose()?
            .flatten()
            .unwrap_or(codec::DEFAULT_LLM_FALLBACK);
        let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
        let local = listener.local_addr()?;
        console_info!(
            ctx.status_tx,
            "FluentForward TCP listening on {} (llm_fallback={})",
            local,
            llm_fallback
        );
        let registrar = ctx.state.clone();
        let server_id = ctx.server_id;
        let task = tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                // Forward has no negative-ACK grammar: refuse excess peers by closing.
                let (socket, peer, permit) = match accept_bounded(
                    &listener,
                    &limiter,
                    b"",
                    "FluentForward",
                    Some(&ctx.status_tx),
                )
                .await
                {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        console_error!(ctx.status_tx, "FluentForward accept failed: {}", error);
                        break;
                    }
                };
                let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
                let now = crate::utils::clock::Instant::now();
                ctx.state
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
                let child_ctx = ctx.clone();
                let child = tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = session(socket, peer, id, &child_ctx, llm_fallback).await {
                        console_error!(
                            child_ctx.status_tx,
                            "FluentForward peer {} closed: {}",
                            peer,
                            error
                        );
                        child_ctx
                            .state
                            .record_access_log(
                                AccessLogOwner::Server(server_id.as_u32()),
                                "FluentForward",
                                Some(id.as_u32()),
                                "forward_invalid_stream",
                                json!({"source_addr":peer.to_string()}),
                                vec![json!({"error":error.to_string()})],
                            )
                            .await;
                    }
                    child_ctx
                        .state
                        .remove_connection_from_server(server_id, id)
                        .await;
                    let _ = child_ctx.status_tx.send("__UPDATE_UI__".into());
                });
                ctx.state.register_server_task(server_id, child).await;
            }
        });
        registrar.register_server_task(server_id, task).await;
        Ok(local)
    }
}
async fn session(
    mut socket: TcpStream,
    peer: SocketAddr,
    id: ConnectionId,
    ctx: &SpawnContext,
    llm_fallback: bool,
) -> Result<()> {
    let mut decoder = codec::Decoder::default();
    let mut buffer = [0; 8192];
    let protocol = actions::FluentForwardProtocol::new();
    loop {
        let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        let batch = loop {
            if let Some(node) = decoder.next_node()? {
                if let Some(batch) = codec::parse_batch(node)? {
                    break batch;
                } else {
                    continue;
                }
            }
            let n = tokio::time::timeout_at(deadline, socket.read(&mut buffer))
                .await
                .context("Forward frame read deadline exceeded")??;
            if n == 0 {
                return decoder.finish();
            }
            ctx.state
                .update_connection_stats(ctx.server_id, id, Some(n as u64), None, Some(1), None)
                .await;
            decoder.feed(&buffer[..n])?;
        };
        let event = Event::new(
            &actions::FORWARD_BATCH_EVENT,
            json!({"tag":batch.tag,"entries":batch.entries,"mode":batch.mode,"record_count":batch.entries.len(),"ack_requested":batch.chunk.is_some(),"source_addr":peer.to_string()}),
        );
        let configured = ctx
            .state
            .get_event_handler_config(ctx.server_id)
            .await
            .is_some_and(|c| c.find_handler("forward_batch").is_some());
        if llm_fallback || configured {
            let result = crate::llm::action_helper::call_llm(
                &ctx.llm_client,
                &ctx.state,
                ctx.server_id,
                Some(id),
                &event,
                &protocol,
            )
            .await;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    console_error!(
                        ctx.status_tx,
                        "Forward decision=fail_closed_handler_error: {}",
                        error
                    );
                    ctx.state.record_access_log(
                        AccessLogOwner::Server(ctx.server_id.as_u32()), "FluentForward", Some(id.as_u32()),
                        "forward_handler_failed", event.data,
                        vec![json!({"decision":"fail_closed_handler_error","error":error.to_string()})],
                    ).await;
                    return Err(error.context("Forward handler failed"));
                }
            };
            if !result.failures.is_empty() {
                console_error!(
                    ctx.status_tx,
                    "Forward decision=fail_closed_handler_action_error failed_actions={}",
                    result.failures.len()
                );
                ctx.state.record_access_log(
                    AccessLogOwner::Server(ctx.server_id.as_u32()), "FluentForward", Some(id.as_u32()),
                    "forward_handler_failed", event.data,
                    vec![json!({"decision":"fail_closed_handler_action_error","failed_action_count":result.failures.len()})],
                ).await;
                anyhow::bail!("Forward handler action failed");
            }
            if result
                .protocol_results
                .iter()
                .any(|r| r.closes_connection())
            {
                return Ok(());
            }
            for message in result.messages {
                console_info!(ctx.status_tx, "{}", message);
            }
        } else {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Server(ctx.server_id.as_u32()),
                    "FluentForward",
                    Some(id.as_u32()),
                    "forward_batch",
                    event.data,
                    vec![json!({"type":"accept_forward_batch"})],
                )
                .await;
        }
        if let Some(chunk) = batch.chunk {
            let bytes = codec::encode_ack(&chunk)?;
            tokio::time::timeout(WRITE_TIMEOUT, socket.write_all(&bytes))
                .await
                .context("Forward ACK write deadline exceeded")??;
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
        }
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
}
