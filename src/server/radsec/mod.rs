//! RadSec server (RFC 6614): RADIUS over TLS. TLS and framing live here; every request goes
//! through the RADIUS server's own decision path, fail-closed rule included.
pub mod actions;
pub mod tls;

use crate::logging::emit::Log;
use crate::protocol::SpawnContext;
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
    radius::{packet, packet::RadiusPacket, RadiusServer},
};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{ensure, Context, Result};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc};

/// How long a connection may stay silent between packets by default.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86_400;
/// Requests on one connection being answered at once; the next waits.
pub const MAX_IN_FLIGHT: usize = 32;

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let get = |k: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten())
    };
    let config = tls::server_config(
        get("certificate_file")?.as_deref(),
        get("private_key_file")?.as_deref(),
        get("ca_file")?.as_deref(),
    )?;
    let mutual = get("ca_file")?.is_some();
    let secret = get("shared_secret")?.unwrap_or_else(|| tls::DEFAULT_SECRET.to_string());
    ensure!(!secret.is_empty(), "shared_secret must not be empty");
    let secret = Arc::new(secret.into_bytes());
    let idle_secs = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&idle_secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    let idle = Duration::from_secs(idle_secs);
    let listener = TcpListener::bind(ctx.legacy_listen_addr())
        .await
        .context("RadSec failed to bind")?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "RadSec listening on {local} ({})",
        if mutual {
            "client certificates required"
        } else {
            "no client certificate required"
        }
    ));
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let Ok((stream, peer, permit)) =
                accept_bounded(&listener, &limiter, b"", "RadSec", Some(&ctx.status_tx)).await
            else {
                break;
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
            let child = ctx.clone();
            let acceptor = acceptor.clone();
            let secret = secret.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let log = Log::new(Some(&child.status_tx));
                    match tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, acceptor.accept(stream))
                        .await
                    {
                        Ok(Ok(tls)) => {
                            if let Err(e) = session(&child, id, peer, tls, secret, idle).await {
                                log.warn(format!(
                                    "RadSec connection {id} from {peer} ended: {e:#}"
                                ));
                            }
                        }
                        Ok(Err(e)) => log.warn(format!("RadSec handshake with {peer} failed: {e}")),
                        Err(_) => log.warn(format!("RadSec handshake with {peer} timed out")),
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
    Ok(local)
}

/// Serve one TLS connection: read packets, answer each through RADIUS's decision path, write
/// the replies in the order they are ready.
async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    peer: SocketAddr,
    tls: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    secret: Arc<Vec<u8>>,
    idle: Duration,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(tls);
    let (frames_tx, mut frames) = mpsc::channel(8);
    let reader_task = tokio::spawn(packet::read_frames(reader, frames_tx));
    let (replies_tx, mut replies) = mpsc::channel::<Vec<u8>>(MAX_IN_FLIGHT);
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT));
    let mut answering = tokio::task::JoinSet::new();
    let result = loop {
        tokio::select! {
            frame = tokio::time::timeout(idle, frames.recv()) => {
                let frame = match frame {
                    Err(_) => break Ok(()),
                    Ok(None) => break Ok(()),
                    Ok(Some(Err(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => break Ok(()),
                    Ok(Some(Err(e))) => break Err(anyhow::anyhow!(e)),
                    Ok(Some(Ok(f))) => f,
                };
                ctx.state
                    .update_connection_stats(ctx.server_id, id, Some(frame.len() as u64), None, Some(1), None)
                    .await;
                let request = match RadiusPacket::decode(&frame) {
                    Ok(p) => p,
                    Err(e) => {
                        Log::new(Some(&ctx.status_tx)).warn(format!("RadSec dropped a packet from {peer}: {e}"));
                        continue;
                    }
                };
                if let Err(e) = packet::verify_request_message_authenticator(&frame, &secret) {
                    Log::new(Some(&ctx.status_tx)).warn(format!(
                        "RadSec dropped {} id={} from {peer}: {e}",
                        packet::code_name(request.code),
                        request.identifier
                    ));
                    continue;
                }
                if request.code == packet::CODE_ACCOUNTING_REQUEST {
                    if let Err(e) = packet::verify_accounting_request(&request, &secret) {
                        Log::new(Some(&ctx.status_tx)).warn(format!(
                            "RadSec dropped Accounting-Request id={} from {peer}: {e}", request.identifier
                        ));
                        continue;
                    }
                }
                let Ok(slot) = slots.clone().acquire_owned().await else { break Ok(()) };
                let (llm, state, status, secret, tx) =
                    (ctx.llm_client.clone(), ctx.state.clone(), ctx.status_tx.clone(), secret.clone(), replies_tx.clone());
                let server_id = ctx.server_id;
                answering.spawn(async move {
                    let _slot = slot;
                    if let Some(reply) =
                        RadiusServer::answer(request, peer, Some(id), llm, state, status, server_id, secret).await
                    {
                        let _ = tx.send(reply).await;
                    }
                });
            }
            Some(reply) = replies.recv() => {
                writer.write_all(&reply).await?;
                writer.flush().await?;
                ctx.state
                    .update_connection_stats(ctx.server_id, id, None, Some(reply.len() as u64), None, Some(1))
                    .await;
            }
            Some(_) = answering.join_next(), if !answering.is_empty() => {}
        }
    };
    // Requests already being answered still get their replies before the connection closes.
    drop(replies_tx);
    while answering.join_next().await.is_some() {}
    while let Ok(reply) = replies.try_recv() {
        let _ = writer.write_all(&reply).await;
    }
    let _ = writer.shutdown().await;
    reader_task.abort();
    result
}
