//! One-way GELF collector with bounded stream parsing and owned tasks.
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
use codec::{Message, Reassembler, Transport};
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
pub struct GelfServer;
impl GelfServer {
    pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
        let llm_fallback = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_bool("llm_fallback"))
            .transpose()?
            .flatten()
            .unwrap_or(codec::DEFAULT_LLM_FALLBACK);
        let transport = Transport::parse(
            &ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_string("transport"))
                .transpose()?
                .flatten()
                .unwrap_or_else(|| codec::DEFAULT_TRANSPORT.into()),
        )?;
        if transport == Transport::Udp {
            return udp(ctx, llm_fallback).await;
        }
        let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
        let local = listener.local_addr()?;
        console_info!(
            ctx.status_tx,
            "GELF TCP listening on {} (llm_fallback={})",
            local,
            llm_fallback
        );
        let registrar = ctx.state.clone();
        let server_id = ctx.server_id;
        let task = tokio::spawn(async move {
            let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
            loop {
                // GELF has no response grammar: refuse excess peers by closing.
                let (socket, peer, permit) =
                    match accept_bounded(&listener, &limiter, b"", "GELF", Some(&ctx.status_tx))
                        .await
                    {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            console_error!(ctx.status_tx, "GELF accept failed: {}", error);
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
                        console_error!(child_ctx.status_tx, "GELF peer {} closed: {}", peer, error);
                        child_ctx
                            .state
                            .record_access_log(
                                AccessLogOwner::Server(server_id.as_u32()),
                                "GELF",
                                Some(id.as_u32()),
                                "gelf_invalid_stream",
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
    let mut decoder = codec::TcpDecoder::default();
    let mut buffer = [0u8; 8192];
    loop {
        let deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        let message = loop {
            if let Some(message) = decoder.next_message()? {
                break message;
            }
            let n = tokio::time::timeout_at(deadline, socket.read(&mut buffer))
                .await
                .context("GELF frame read deadline exceeded")??;
            if n == 0 {
                return decoder.finish();
            }
            ctx.state
                .update_connection_stats(ctx.server_id, id, Some(n as u64), None, Some(1), None)
                .await;
            decoder.feed(&buffer[..n])?;
        };
        dispatch(message, peer, id, ctx, Transport::Tcp, llm_fallback).await?;
    }
}
async fn dispatch(
    mut message: Message,
    peer: SocketAddr,
    id: ConnectionId,
    ctx: &SpawnContext,
    transport: Transport,
    llm_fallback: bool,
) -> Result<()> {
    if message.timestamp.is_none() {
        message.timestamp = Some(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64());
    }
    if message.level.is_none() {
        message.level = Some(1);
    }
    let event = Event::new(
        &actions::GELF_MESSAGE_EVENT,
        json!({"message":message,"source_addr":peer.to_string(),"transport":transport.as_str()}),
    );
    let configured = ctx
        .state
        .get_event_handler_config(ctx.server_id)
        .await
        .is_some_and(|c| c.find_handler("gelf_message").is_some());
    if llm_fallback || configured {
        let result = crate::llm::action_helper::call_llm(
            &ctx.llm_client,
            &ctx.state,
            ctx.server_id,
            Some(id),
            &event,
            &actions::GelfProtocol::new(),
        )
        .await?;
        for message in result.messages {
            console_info!(ctx.status_tx, "{}", message);
        }
    } else {
        ctx.state
            .record_access_log(
                AccessLogOwner::Server(ctx.server_id.as_u32()),
                "GELF",
                Some(id.as_u32()),
                "gelf_message",
                event.data,
                vec![json!({"type":"collect_gelf_message"})],
            )
            .await;
    }
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    Ok(())
}
async fn udp(ctx: SpawnContext, llm_fallback: bool) -> Result<SocketAddr> {
    let socket = tokio::net::UdpSocket::bind(ctx.legacy_listen_addr()).await?;
    let local = socket.local_addr()?;
    console_info!(
        ctx.status_tx,
        "GELF UDP listening on {} (llm_fallback={})",
        local,
        llm_fallback
    );
    let registrar = ctx.state.clone();
    let server_id = ctx.server_id;
    let task = tokio::spawn(async move {
        let mut buffer = vec![0; codec::MAX_DATAGRAM_BYTES + 1];
        let mut reassembler = Reassembler::default();
        let mut expiry = tokio::time::interval(Duration::from_secs(1));
        loop {
            let (n, peer) = tokio::select! {
                _=expiry.tick()=>{reassembler.expire(tokio::time::Instant::now());continue;},
                result=socket.recv_from(&mut buffer)=>match result {Ok(r)=>r,Err(error)=>{console_error!(ctx.status_tx,"GELF receive failed: {}",error);break;}}
            };
            let message = match reassembler.push(peer, &buffer[..n], tokio::time::Instant::now()) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(error) => {
                    ctx.state
                        .record_access_log(
                            AccessLogOwner::Server(server_id.as_u32()),
                            "GELF",
                            None,
                            "gelf_invalid_datagram",
                            json!({"source_addr":peer.to_string(),"received_bytes":n}),
                            vec![json!({"error":error.to_string()})],
                        )
                        .await;
                    continue;
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
            if let Err(error) =
                dispatch(message, peer, id, &ctx, Transport::Udp, llm_fallback).await
            {
                console_error!(
                    ctx.status_tx,
                    "GELF decision=fail_closed_handler_error: {}",
                    error
                );
            }
            ctx.state.remove_connection_from_server(server_id, id).await;
        }
    });
    registrar.register_server_task(server_id, task).await;
    Ok(local)
}
