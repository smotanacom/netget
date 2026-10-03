//! StatsD collector with bounded, sequential batch handling and no reply traffic.
pub mod actions;
pub mod codec;

use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::{
    server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
    AccessLogOwner,
};
use crate::{console_debug, console_error, console_info};
use anyhow::Result;
use codec::{Dialect, DEFAULT_DIALECT, DEFAULT_LLM_FALLBACK, MAX_DATAGRAM_BYTES};
use serde_json::json;
use tokio::net::UdpSocket;

pub struct StatsdServer;
impl StatsdServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<std::net::SocketAddr> {
        let dialect = Dialect::parse(
            &ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_string("dialect"))
                .transpose()?
                .flatten()
                .unwrap_or_else(|| DEFAULT_DIALECT.into()),
        )?;
        let llm_fallback = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_bool("llm_fallback"))
            .transpose()?
            .flatten()
            .unwrap_or(DEFAULT_LLM_FALLBACK);
        let socket = UdpSocket::bind(ctx.legacy_listen_addr()).await?;
        let local_addr = socket.local_addr()?;
        console_info!(
            ctx.status_tx,
            "StatsD listening on {} ({}, llm_fallback={})",
            local_addr,
            dialect.as_str(),
            llm_fallback
        );
        let registrar = ctx.state.clone();
        let server_id = ctx.server_id;
        let handle = tokio::spawn(async move {
            // +1 detects a truncated datagram: anything above the cap is rejected,
            // even if its prefix would be a valid message. No unbounded task queue.
            let mut buffer = vec![0u8; MAX_DATAGRAM_BYTES + 1];
            let protocol = actions::StatsdProtocol::new();
            loop {
                let (n, peer) = match socket.recv_from(&mut buffer).await {
                    Ok(received) => received,
                    Err(error) => {
                        console_error!(ctx.status_tx, "StatsD receive failed: {}", error);
                        break;
                    }
                };
                let records = match codec::parse_datagram(&buffer[..n], dialect) {
                    Ok(records) => records,
                    Err(error) => {
                        console_debug!(
                            ctx.status_tx,
                            "StatsD rejected datagram from {}: {}",
                            peer,
                            error
                        );
                        ctx.state
                            .record_access_log(
                                AccessLogOwner::Server(server_id.as_u32()),
                                "StatsD",
                                None,
                                "statsd_invalid_datagram",
                                json!({"source_addr":peer.to_string(),"received_bytes":n}),
                                vec![json!({"error":error.to_string()})],
                            )
                            .await;
                        continue;
                    }
                };
                // Only the batch currently being handled needs a peer row. Remove it
                // afterward rather than accumulate one connection per telemetry packet.
                let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
                let now = crate::utils::clock::Instant::now();
                ctx.state
                    .add_connection_to_server(
                        server_id,
                        ConnectionState {
                            id,
                            remote_addr: peer,
                            local_addr,
                            bytes_sent: 0,
                            bytes_received: n as u64,
                            packets_sent: 0,
                            packets_received: 1,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        },
                    )
                    .await;
                let event = Event::new(
                    &actions::STATSD_BATCH_EVENT,
                    json!({"records":records,"record_count":records.len(),"source_addr":peer.to_string(),"dialect":dialect.as_str()}),
                );
                let configured = ctx
                    .state
                    .get_event_handler_config(server_id)
                    .await
                    .is_some_and(|config| config.find_handler("statsd_batch").is_some());
                if llm_fallback || configured {
                    match crate::llm::action_helper::call_llm(
                        &ctx.llm_client,
                        &ctx.state,
                        server_id,
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
                                "StatsD decision=fail_closed_handler_error from {}: {}",
                                peer,
                                error
                            );
                        }
                    }
                } else {
                    ctx.state
                        .record_access_log(
                            AccessLogOwner::Server(server_id.as_u32()),
                            "StatsD",
                            Some(id.as_u32()),
                            "statsd_batch",
                            event.data,
                            vec![json!({"type":"collect_statsd_batch"})],
                        )
                        .await;
                }
                ctx.state.remove_connection_from_server(server_id, id).await;
                let _ = ctx.status_tx.send("__UPDATE_UI__".into());
            }
        });
        registrar.register_server_task(server_id, handle).await;
        Ok(local_addr)
    }
}
