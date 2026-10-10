//! The classic inetd services (RFC 862-868) over TCP and UDP, sharing one engine. Rust owns
//! the framing, the clock arithmetic, the chargen pattern and every bound; handlers decide
//! what each request receives. No handler failure ever produces fabricated output.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use actions::Service;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
};

/// How long Echo and Discard wait for the client's next bytes.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 3600;
pub const DEFAULT_TRANSPORT: &str = "both";
/// RFC 864's customary line: 72 characters and CRLF.
pub const DEFAULT_CHARGEN_LINE: usize = 72;
/// UDP requests handled at once; datagrams beyond this are dropped.
pub const MAX_UDP_IN_FLIGHT: usize = 64;
/// Attempts to find one port number free on both TCP and UDP.
const BOTH_BIND_ATTEMPTS: usize = 16;

#[derive(Clone, Copy, PartialEq)]
enum Transport {
    Tcp,
    Udp,
    Both,
}

fn config(ctx: &SpawnContext, service: Service) -> Result<(Transport, Duration)> {
    let params = ctx.startup_params.as_ref();
    let transport = match params
        .map(|p| p.get_optional_string("transport"))
        .transpose()?
        .flatten()
        .as_deref()
        .unwrap_or(DEFAULT_TRANSPORT)
    {
        "tcp" => Transport::Tcp,
        "udp" => Transport::Udp,
        "both" => Transport::Both,
        other => anyhow::bail!("transport must be tcp, udp or both, not {other}"),
    };
    let idle = if service.reads() {
        let secs = params
            .map(|p| p.get_optional_u64("idle_timeout_secs"))
            .transpose()?
            .flatten()
            .unwrap_or(IDLE_TIMEOUT.as_secs());
        anyhow::ensure!(
            (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
            "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
        );
        Duration::from_secs(secs)
    } else {
        IDLE_TIMEOUT
    };
    Ok((transport, idle))
}

async fn bind(
    requested: SocketAddr,
    transport: Transport,
) -> Result<(Option<TcpListener>, Option<UdpSocket>)> {
    match transport {
        Transport::Tcp => Ok((Some(TcpListener::bind(requested).await?), None)),
        Transport::Udp => Ok((None, Some(UdpSocket::bind(requested).await?))),
        Transport::Both => {
            for _ in 0..BOTH_BIND_ATTEMPTS {
                let tcp = TcpListener::bind(requested).await?;
                let port = tcp.local_addr()?.port();
                match UdpSocket::bind(SocketAddr::new(requested.ip(), port)).await {
                    Ok(udp) => return Ok((Some(tcp), Some(udp))),
                    // An explicit port is never replaced; an OS-assigned one is tried again.
                    Err(e) if requested.port() != 0 => return Err(e.into()),
                    Err(_) => continue,
                }
            }
            anyhow::bail!(
                "no port number was free on both TCP and UDP after {BOTH_BIND_ATTEMPTS} attempts"
            )
        }
    }
}

pub async fn spawn(ctx: SpawnContext, service: Service) -> Result<SocketAddr> {
    let (transport, idle) = config(&ctx, service)?;
    let (tcp, udp) = bind(ctx.legacy_listen_addr(), transport).await?;
    let addr = match (&tcp, &udp) {
        (Some(tcp), _) => tcp.local_addr()?,
        (None, Some(udp)) => udp.local_addr()?,
        _ => unreachable!("bind returns at least one socket"),
    };
    Log::new(Some(&ctx.status_tx)).info(format!(
        "{} listening on {addr} ({})",
        service.name(),
        match transport {
            Transport::Tcp => "tcp",
            Transport::Udp => "udp",
            Transport::Both => "tcp and udp",
        }
    ));
    if let Some(listener) = tcp {
        let child = ctx.clone();
        let accept = tokio::spawn(accept_loop(child, service, listener, addr, idle));
        ctx.state.register_server_task(ctx.server_id, accept).await;
    }
    if let Some(socket) = udp {
        let child = ctx.clone();
        let datagrams = tokio::spawn(udp_loop(child, service, Arc::new(socket)));
        ctx.state
            .register_server_task(ctx.server_id, datagrams)
            .await;
    }
    Ok(addr)
}

async fn accept_loop(
    ctx: SpawnContext,
    service: Service,
    listener: TcpListener,
    addr: SocketAddr,
    idle: Duration,
) {
    let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
        crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
    );
    let server_id = ctx.server_id;
    loop {
        let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
            &listener,
            &limiter,
            b"",
            service.name(),
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
                if let Err(e) = tcp_session(&child, id, service, socket, peer, idle).await {
                    Log::new(Some(&child.status_tx))
                        .warn(format!("{} connection {id} ended: {e}", service.name()));
                }
                child
                    .state
                    .update_connection_status(server_id, id, ConnectionStatus::Closed)
                    .await;
                let _ = child.status_tx.send("__UPDATE_UI__".into());
            })
            .await;
    }
}

fn outcome(ctx: &SpawnContext, service: Service, id: Option<ConnectionId>, decision: &str) {
    let who = id
        .map(|id| format!("connection {id}"))
        .unwrap_or_else(|| "datagram".into());
    let summary = format!(
        "{} {who} operation={} decision={decision}",
        service.name(),
        service.event().id
    );
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// `Some(answer)` for the service's reply, `None` for a deliberate refusal, `Err` for a failure
/// or silence. Both of the last two send nothing; only the log tells them apart.
async fn decision(
    ctx: &SpawnContext,
    service: Service,
    id: Option<ConnectionId>,
    data: Value,
) -> Result<Option<Value>> {
    let event = Event::new(service.event_type(), data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        id,
        &event,
        actions::protocol(service),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            outcome(ctx, service, id, "fail_closed_llm_error");
            return Err(error);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, service, id, "fail_closed_invalid_reply");
        anyhow::bail!("{} handler supplied an invalid action", service.name());
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data }
                if name == service.action_name() || name == service.refuse_name() =>
            {
                if found.is_some() {
                    outcome(ctx, service, id, "fail_closed_invalid_reply");
                    anyhow::bail!("Multiple {} answers to one request", service.name());
                }
                found = Some((name == service.refuse_name(), data));
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match found {
        Some((true, _)) => {
            outcome(ctx, service, id, "model_reject");
            Ok(None)
        }
        Some((false, answer)) => {
            outcome(ctx, service, id, "model_answer");
            Ok(Some(answer))
        }
        None => {
            outcome(ctx, service, id, "model_silent");
            anyhow::bail!("Handler did not answer the {} request", service.name())
        }
    }
}

async fn send(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut (impl tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
) -> Result<()> {
    tokio::time::timeout(wire::IO_TIMEOUT, w.write_all(bytes))
        .await
        .context("write deadline")??;
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
    Ok(())
}

/// The bytes a stream service (Daytime, QOTD, Time) sends for a handler's answer.
fn single_reply(service: Service, answer: &Value) -> Result<Vec<u8>> {
    Ok(match service {
        Service::Daytime => {
            let text = match answer["text"].as_str() {
                Some(text) => wire::line(text, wire::MAX_DAYTIME_CHARS),
                None => wire::daytime_now(),
            };
            format!("{text}\r\n").into_bytes()
        }
        Service::Qotd => {
            let quote: String = answer["quote"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .map(crate::utils::sanitize::strip_controls)
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(wire::MAX_QUOTE_CHARS)
                .collect();
            format!("{}\r\n", quote.replace('\n', "\r\n")).into_bytes()
        }
        Service::Time => wire::time_bytes(wire::requested_time(answer)?).to_vec(),
        _ => anyhow::bail!("{} is not a single-reply service", service.name()),
    })
}

struct Chargen {
    charset: Vec<u8>,
    width: usize,
    max_bytes: Option<u64>,
}

fn chargen(answer: &Value) -> Result<Chargen> {
    Ok(Chargen {
        charset: wire::charset(answer.get("charset"))?.into_bytes(),
        width: answer["line_length"]
            .as_u64()
            .map(|w| w as usize)
            .unwrap_or(DEFAULT_CHARGEN_LINE),
        max_bytes: answer["max_bytes"].as_u64(),
    })
}

async fn tcp_session(
    ctx: &SpawnContext,
    id: ConnectionId,
    service: Service,
    socket: TcpStream,
    peer: SocketAddr,
    idle: Duration,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(socket);
    match service {
        Service::Echo => {
            let mut buf = vec![0u8; wire::READ_CHUNK];
            loop {
                let n = tokio::time::timeout(idle, reader.read(&mut buf))
                    .await
                    .context("idle deadline")??;
                if n == 0 {
                    return Ok(());
                }
                ctx.state
                    .update_connection_stats(ctx.server_id, id, Some(n as u64), None, Some(1), None)
                    .await;
                let (data, encoding) = wire::encode(&buf[..n]);
                let answer = match decision(
                    ctx,
                    service,
                    Some(id),
                    json!({"transport":"tcp","data":data,"encoding":encoding,"bytes":n}),
                )
                .await
                {
                    Ok(Some(answer)) => answer,
                    // Nothing is echoed that the handler did not approve: half-close instead.
                    Ok(None) | Err(_) => {
                        let _ = writer.shutdown().await;
                        return Ok(());
                    }
                };
                let reply = match answer["data"].as_str() {
                    Some(data) => wire::decode(data, answer["encoding"].as_str())?,
                    None => buf[..n].to_vec(),
                };
                send(ctx, id, &mut writer, &reply).await?;
            }
        }
        Service::Discard => {
            let Ok(Some(answer)) = decision(
                ctx,
                service,
                Some(id),
                json!({"transport":"tcp","remote_addr":peer.to_string()}),
            )
            .await
            else {
                return Ok(());
            };
            let limit = answer["max_bytes"].as_u64();
            let mut buf = vec![0u8; wire::READ_CHUNK];
            let mut total: u64 = 0;
            loop {
                let n = tokio::time::timeout(idle, reader.read(&mut buf))
                    .await
                    .context("idle deadline")??;
                if n == 0 {
                    return Ok(());
                }
                total += n as u64;
                ctx.state
                    .update_connection_stats(ctx.server_id, id, Some(n as u64), None, Some(1), None)
                    .await;
                if limit.is_some_and(|limit| total >= limit) {
                    let _ = writer.shutdown().await;
                    return Ok(());
                }
            }
        }
        Service::Daytime | Service::Qotd | Service::Time => {
            // Silence on failure: closing without a line is the honest answer.
            if let Ok(Some(answer)) =
                decision(ctx, service, Some(id), json!({"transport":"tcp"})).await
            {
                send(ctx, id, &mut writer, &single_reply(service, &answer)?).await?;
            }
            let _ = writer.shutdown().await;
            Ok(())
        }
        Service::Chargen => {
            let Ok(Some(answer)) =
                decision(ctx, service, Some(id), json!({"transport":"tcp"})).await
            else {
                return Ok(());
            };
            let pattern = chargen(&answer)?;
            // RFC 864: anything the client sends is thrown away. A client that half-closes
            // its side (netcat -N does at once) still wants the stream, so the end of its
            // input never ends the session; only a failed write or the limit does.
            let drain = async {
                let mut sink = vec![0u8; wire::READ_CHUNK];
                while let Ok(n) = reader.read(&mut sink).await {
                    if n == 0 {
                        break;
                    }
                }
                std::future::pending::<()>().await
            };
            let generate = async {
                let mut sent: u64 = 0;
                let mut n = 0usize;
                loop {
                    let mut line = wire::chargen_line(&pattern.charset, pattern.width, n);
                    if let Some(max) = pattern.max_bytes {
                        let left = max.saturating_sub(sent) as usize;
                        if left == 0 {
                            break;
                        }
                        line.truncate(left);
                    }
                    if send(ctx, id, &mut writer, &line).await.is_err() {
                        break;
                    }
                    sent += line.len() as u64;
                    n += 1;
                }
                let _ = writer.shutdown().await;
            };
            tokio::pin!(drain);
            tokio::select! {
                _ = generate => {}
                _ = &mut drain => {}
            }
            Ok(())
        }
    }
}

async fn udp_loop(ctx: SpawnContext, service: Service, socket: Arc<UdpSocket>) {
    let in_flight = Arc::new(tokio::sync::Semaphore::new(MAX_UDP_IN_FLIGHT));
    let mut buf = vec![0u8; wire::READ_CHUNK + 1];
    let mut chargen_offset = 0usize;
    loop {
        let (n, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if n > wire::READ_CHUNK {
            Log::new(Some(&ctx.status_tx)).warn(format!(
                "{} dropped an oversized datagram from {peer}",
                service.name()
            ));
            continue;
        }
        if service == Service::Discard {
            continue;
        }
        let Ok(permit) = in_flight.clone().try_acquire_owned() else {
            Log::new(Some(&ctx.status_tx)).warn(format!(
                "{} dropped a datagram from {peer}: too many in flight",
                service.name()
            ));
            continue;
        };
        let datagram = buf[..n].to_vec();
        let offset = chargen_offset;
        chargen_offset = chargen_offset.wrapping_add(1);
        let child = ctx.clone();
        let reply_socket = socket.clone();
        ctx.state
            .spawn_server_task(ctx.server_id, async move {
                let _permit = permit;
                let data = match service {
                    Service::Echo => {
                        let (data, encoding) = wire::encode(&datagram);
                        json!({"transport":"udp","data":data,"encoding":encoding,"bytes":datagram.len()})
                    }
                    _ => json!({"transport":"udp"}),
                };
                // A failed or silent handler leaves the datagram unanswered.
                let Ok(Some(answer)) = decision(&child, service, None, data).await else {
                    return;
                };
                let reply = match service {
                    Service::Echo => match answer["data"].as_str() {
                        Some(data) => wire::decode(data, answer["encoding"].as_str()),
                        None => Ok(datagram),
                    },
                    Service::Chargen => chargen(&answer).map(|p| {
                        let mut line = wire::chargen_line(&p.charset, p.width, offset);
                        line.truncate(wire::MAX_CHARGEN_UDP);
                        line
                    }),
                    other => single_reply(other, &answer),
                };
                match reply {
                    Ok(reply) => {
                        let _ = reply_socket.send_to(&reply, peer).await;
                    }
                    Err(e) => Log::new(Some(&child.status_tx)).warn(format!("{} reply to {peer} not sent: {e}", service.name())),
                }
            })
            .await;
    }
}
