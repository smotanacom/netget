//! One-way Carbon plaintext collector with bounded stream parsing and owned tasks.
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
use std::{
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
};

pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub struct GraphiteServer;
impl GraphiteServer {
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
            "Graphite Carbon TCP listening on {} (llm_fallback={})",
            local,
            llm_fallback
        );
        let registrar = ctx.state.clone();
        let server_id = ctx.server_id;
        let task = tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                // Carbon has no response grammar: refuse excess peers by closing.
                let (socket, peer, permit) = match accept_bounded(
                    &listener,
                    &limiter,
                    b"",
                    "Graphite",
                    Some(&ctx.status_tx),
                )
                .await
                {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        console_error!(ctx.status_tx, "Graphite accept failed: {}", error);
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
                            "Graphite peer {} closed: {}",
                            peer,
                            error
                        );
                        child_ctx
                            .state
                            .record_access_log(
                                AccessLogOwner::Server(server_id.as_u32()),
                                "Graphite",
                                Some(id.as_u32()),
                                "graphite_invalid_stream",
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
    let mut buffer = [0u8; codec::READ_BYTES];
    let protocol = actions::GraphiteProtocol::new();
    loop {
        // One absolute deadline for obtaining the next complete line, even if a
        // slow sender trickles bytes. Handler time is outside this deadline.
        let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        let mut metrics = loop {
            let metrics = decoder.next_batch()?;
            if !metrics.is_empty() {
                break metrics;
            }
            let n = tokio::time::timeout_at(deadline, socket.read(&mut buffer))
                .await
                .context("Carbon metric frame read deadline exceeded")??;
            if n == 0 {
                return decoder.finish();
            }
            ctx.state
                .update_connection_stats(ctx.server_id, id, Some(n as u64), None, Some(1), None)
                .await;
            decoder.feed(&buffer[..n])?;
        };
        let received_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
        for metric in &mut metrics {
            if metric.timestamp == -1.0 {
                metric.timestamp = received_time;
            }
        }
        let event = Event::new(
            &actions::GRAPHITE_BATCH_EVENT,
            json!({"record_count":metrics.len(),"metrics":metrics,"source_addr":peer.to_string()}),
        );
        let configured = ctx
            .state
            .get_event_handler_config(ctx.server_id)
            .await
            .is_some_and(|c| c.find_handler("graphite_batch").is_some());
        if llm_fallback || configured {
            match crate::llm::action_helper::call_llm(
                &ctx.llm_client,
                &ctx.state,
                ctx.server_id,
                Some(id),
                &event,
                &protocol,
            )
            .await
            {
                Ok(result) => {
                    for message in result.messages {
                        console_info!(ctx.status_tx, "{}", message);
                    }
                }
                Err(error) => {
                    console_error!(
                        ctx.status_tx,
                        "Graphite decision=fail_closed_handler_error: {}",
                        error
                    );
                    return Err(error);
                }
            }
        } else {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Server(ctx.server_id.as_u32()),
                    "Graphite",
                    Some(id.as_u32()),
                    "graphite_batch",
                    event.data,
                    vec![json!({"type":"collect_graphite_batch"})],
                )
                .await;
        }
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
}
