//! SRT listener over srt-tokio (ARQ, TSBPD, optional AES). Rust owns the handshake, stream-ID
//! parsing, one publisher per resource and the relay to every reader of it; the handler admits
//! each caller and can push text messages to a reader.
pub mod actions;
pub mod streamid;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::Result;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use srt_tokio::access::{RejectReason, ServerRejectReason};
use srt_tokio::{SocketStatistics, SrtListener, SrtSocket};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_LATENCY: Duration = Duration::from_millis(120);
pub const MAX_READERS: usize = 64;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const READER_QUEUE: usize = 4096;

/// Handler-facing reject reasons and their SRT server reject codes.
pub const REJECT_CODES: &[(&str, ServerRejectReason)] = &[
    ("bad_request", ServerRejectReason::BadRequest),
    ("unauthorized", ServerRejectReason::Unauthorized),
    ("overload", ServerRejectReason::Overload),
    ("forbidden", ServerRejectReason::Forbidden),
    ("not_found", ServerRejectReason::Notfound),
    ("conflict", ServerRejectReason::Conflict),
];

#[derive(Default)]
struct Resource {
    publisher: Option<ConnectionId>,
    readers: HashMap<ConnectionId, mpsc::Sender<Bytes>>,
}

struct Shared {
    ctx: SpawnContext,
    resources: Mutex<HashMap<String, Resource>>,
    idle: Duration,
}

/// srt-tokio 0.4.4 announces its live payload size (1316) in the handshake's MSS field, where
/// libsrt expects the MTU, so a libsrt peer limits itself to 1272-byte messages and cannot send
/// the standard 1316 (seven TS packets). Raising the payload size to the live maximum, 1456, makes
/// the announced MSS large enough for that; NetGet's own messages are 1316 bytes either way.
pub fn announce_mss(o: &mut srt_tokio::options::SocketOptions) {
    o.sender.max_payload_size = srt_tokio::options::PacketSize(1456);
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let latency_ms = p
        .map(|p| p.get_optional_u64("latency_ms"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_LATENCY.as_millis() as u64);
    anyhow::ensure!(
        (20..=8000).contains(&latency_ms),
        "latency_ms must be 20..=8000"
    );
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=3600).contains(&idle),
        "idle_timeout_secs must be 1..=3600"
    );
    let passphrase = p
        .map(|p| p.get_optional_string("passphrase"))
        .transpose()?
        .flatten();
    if let Some(pw) = &passphrase {
        anyhow::ensure!(
            (10..=79).contains(&pw.len()),
            "passphrase must be 10 to 79 characters (SRT)"
        );
    }
    let mut builder = SrtListener::builder()
        .latency(Duration::from_millis(latency_ms))
        .set(announce_mss);
    if let Some(pw) = passphrase {
        builder = builder.encryption(16, pw);
    }
    // Bound here so the real address (port 0 included) is known; srt-tokio does not report it.
    let udp = tokio::net::UdpSocket::bind(ctx.legacy_listen_addr()).await?;
    let local = udp.local_addr()?;
    let (listener, mut incoming) = builder.socket(udp).bind(local).await?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "SRT listener at srt://{local} (latency {latency_ms} ms)"
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        resources: Mutex::default(),
        idle: Duration::from_secs(idle),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        // The listener closes when dropped; it lives as long as this task.
        let _listener = listener;
        while let Some(request) = incoming.incoming().next().await {
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: request.remote(),
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
                    if let Err(e) = connection(&child, id, request).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("SRT connection {id}: {e:#}"));
                    }
                    child
                        .ctx
                        .state
                        .remove_peer_handle(server_id, id.as_u32())
                        .await;
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
    let summary = format!("SRT connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// Ask the handler; Ok(None) admits, Ok(Some(reason)) refuses, Err is a failure.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Option<ServerRejectReason>, ()> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::SrtProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(());
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
    match (result.failures.is_empty(), answers.as_slice()) {
        (true, [a]) if a["type"] == "srt_accept" || a["type"] == "srt_ignore" => {
            outcome(ctx, id, operation, "model_answer");
            Ok(None)
        }
        (true, [a]) if a["type"] == "srt_reject" => {
            outcome(ctx, id, operation, "model_reject");
            let code = REJECT_CODES
                .iter()
                .find(|(n, _)| a["reason"] == *n)
                .map(|(_, c)| *c)
                .unwrap_or(ServerRejectReason::Forbidden);
            Ok(Some(code))
        }
        (true, []) => {
            outcome(ctx, id, operation, "model_silent");
            Err(())
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Err(())
        }
    }
}

fn stats_json(s: &SocketStatistics) -> Value {
    json!({
        "rx_packets": s.rx_data, "rx_bytes": s.rx_bytes, "rx_lost_packets": s.rx_loss_data, "rx_retransmitted_packets": s.rx_retransmit_data,
        "rx_dropped_packets": s.rx_dropped_data, "tx_packets": s.tx_data, "tx_bytes": s.tx_bytes, "tx_retransmitted_packets": s.tx_retransmit_data,
        "tx_lost_packets": s.tx_loss_data, "rtt_ms": s.tx_average_rtt.as_millis() as u64,
    })
}

async fn connection(
    shared: &Shared,
    id: ConnectionId,
    request: srt_tokio::ConnectionRequest,
) -> Result<()> {
    let ctx = &shared.ctx;
    let raw = request
        .stream_id()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let target = match streamid::parse(&raw) {
        Ok(t) if !t.resource.is_empty() => t,
        _ => {
            outcome(ctx, id, "connect", "protocol_refusal");
            request
                .reject(RejectReason::Server(ServerRejectReason::BadRequest))
                .await?;
            return Ok(());
        }
    };
    let busy = target.mode == "publish"
        && shared
            .resources
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target.resource)
            .is_some_and(|r| r.publisher.is_some());
    if busy {
        outcome(ctx, id, "connect", "protocol_refusal");
        request
            .reject(RejectReason::Server(ServerRejectReason::Conflict))
            .await?;
        return Ok(());
    }
    let event = Event::new(
        &actions::CONNECT_EVENT,
        json!({"stream_id": raw, "resource": target.resource, "mode": target.mode, "user": target.user, "remote": request.remote().to_string()}),
    );
    match ask(shared, id, event, "connect").await {
        Ok(None) => {}
        Ok(Some(code)) => {
            request.reject(RejectReason::Server(code)).await?;
            return Ok(());
        }
        Err(()) => {
            request
                .reject(RejectReason::Server(
                    ServerRejectReason::InternalServerError,
                ))
                .await?;
            return Ok(());
        }
    }
    // Claim the resource now that the caller is admitted (a racing publisher loses here).
    let (tx, mut relayed) = mpsc::channel::<Bytes>(READER_QUEUE);
    let refused = {
        let mut resources = shared.resources.lock().unwrap_or_else(|e| e.into_inner());
        let r = resources.entry(target.resource.clone()).or_default();
        if target.mode == "publish" {
            r.publisher
                .is_some()
                .then_some(ServerRejectReason::Conflict)
                .or_else(|| {
                    r.publisher = Some(id);
                    None
                })
        } else if r.readers.len() >= MAX_READERS {
            Some(ServerRejectReason::Overload)
        } else {
            r.readers.insert(id, tx);
            None
        }
    };
    if let Some(code) = refused {
        request.reject(RejectReason::Server(code)).await?;
        return Ok(());
    }
    let started = Instant::now();
    let result = match request.accept(None).await {
        Ok(socket) => session(shared, id, socket, &target, &mut relayed).await,
        Err(e) => Err((e.into(), None)),
    };
    {
        let mut resources = shared.resources.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = resources.get_mut(&target.resource) {
            if r.publisher == Some(id) {
                r.publisher = None;
            }
            r.readers.remove(&id);
            if r.publisher.is_none() && r.readers.is_empty() {
                resources.remove(&target.resource);
            }
        }
    }
    let (reason, stats) = match result {
        Ok((reason, stats)) => (reason, stats),
        Err((e, stats)) => (format!("{e:#}"), stats),
    };
    let mut data = json!({"resource": target.resource, "mode": target.mode, "duration_ms": started.elapsed().as_millis() as u64, "reason": reason});
    if let Some(s) = stats {
        data["statistics"] = stats_json(&s);
    }
    let _ = ask(
        shared,
        id,
        Event::new(&actions::CLOSED_EVENT, data),
        "closed",
    )
    .await;
    Ok(())
}

type Ended = std::result::Result<
    (String, Option<SocketStatistics>),
    (anyhow::Error, Option<SocketStatistics>),
>;

async fn session(
    shared: &Shared,
    id: ConnectionId,
    mut socket: SrtSocket,
    target: &streamid::Target,
    relayed: &mut mpsc::Receiver<Bytes>,
) -> Ended {
    let ctx = &shared.ctx;
    let mut stats_stream = socket.statistics().clone();
    let mut latest: Option<SocketStatistics> = None;
    let mut commands = if target.mode == "request" {
        Some(
            crate::server::peer_support::register_peer_channel(
                &ctx.state,
                ctx.server_id,
                id.as_u32(),
            )
            .await,
        )
    } else {
        None
    };
    let publishing = target.mode == "publish";
    // A fixed deadline from the last data: a timeout around each read would restart whenever
    // the statistics stream or a relay woke the loop, and never fire.
    let mut last_data = tokio::time::Instant::now();
    loop {
        enum Wake {
            Data(Option<std::io::Result<(std::time::Instant, Bytes)>>),
            Relay(Option<Bytes>),
            Stats(Option<SocketStatistics>),
            Command(Option<ClientCommand>),
            Idle,
        }
        let wake = {
            let command = async {
                match commands.as_mut() {
                    Some(c) => c.recv().await,
                    None => std::future::pending().await,
                }
            };
            let relay = async {
                if publishing {
                    std::future::pending().await
                } else {
                    relayed.recv().await
                }
            };
            tokio::select! {
                d = socket.next() => Wake::Data(d),
                _ = tokio::time::sleep_until(last_data + shared.idle), if publishing => Wake::Idle,
                r = relay => Wake::Relay(r),
                s = stats_stream.next() => Wake::Stats(s),
                c = command => Wake::Command(c),
            }
        };
        match wake {
            Wake::Idle if publishing => {
                return Ok((
                    "the publisher sent nothing for the idle timeout".into(),
                    latest,
                ))
            }
            Wake::Idle => {}
            Wake::Data(None) => return Ok(("the caller closed the connection".into(), latest)),
            Wake::Data(Some(Err(e))) => return Err((e.into(), latest)),
            Wake::Data(Some(Ok((_, data)))) => {
                last_data = tokio::time::Instant::now();
                ctx.state
                    .update_connection_stats(
                        ctx.server_id,
                        id,
                        Some(data.len() as u64),
                        None,
                        Some(1),
                        None,
                    )
                    .await;
                if publishing {
                    let mut resources = shared.resources.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(r) = resources.get_mut(&target.resource) {
                        // A reader whose queue is full is too slow to keep.
                        r.readers.retain(|_, tx| tx.try_send(data.clone()).is_ok());
                    }
                }
            }
            Wake::Relay(None) => {
                return Ok(("the reader was dropped for falling behind".into(), latest))
            }
            Wake::Relay(Some(data)) => {
                let n = data.len() as u64;
                if let Err(e) = socket.send((std::time::Instant::now(), data)).await {
                    return Err((e.into(), latest));
                }
                ctx.state
                    .update_connection_stats(ctx.server_id, id, None, Some(n), None, Some(1))
                    .await;
            }
            Wake::Stats(Some(s)) => latest = Some(s),
            Wake::Stats(None) => {}
            Wake::Command(None) => commands = None,
            Wake::Command(Some(cmd)) => {
                let a = cmd.action.clone();
                let outcome = match a["type"].as_str() {
                    Some("disconnect") => ClientSendOutcome::Disconnected,
                    Some("srt_send_text") => match actions::check_answer(&a) {
                        Err(e) => ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                        Ok(()) => {
                            let text =
                                Bytes::from(a["text"].as_str().unwrap_or_default().to_owned());
                            let n = text.len();
                            match socket.send((std::time::Instant::now(), text)).await {
                                Ok(()) => ClientSendOutcome::Sent { bytes_sent: n },
                                Err(e) => ClientSendOutcome::Rejected {
                                    error: e.to_string(),
                                },
                            }
                        }
                    },
                    _ => ClientSendOutcome::Rejected {
                        error: "an SRT reader accepts srt_send_text or disconnect".into(),
                    },
                };
                ctx.state
                    .record_access_log(
                        crate::state::AccessLogOwner::Server(ctx.server_id.as_u32()),
                        "SRT",
                        Some(id.as_u32()),
                        "injected_action",
                        json!({"type": a["type"]}),
                        vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                    )
                    .await;
                let done = matches!(outcome, ClientSendOutcome::Disconnected);
                let _ = cmd.reply_tx.send(Ok(outcome));
                if done {
                    let _ = socket.close().await;
                    return Ok(("the operator disconnected the reader".into(), latest));
                }
            }
        }
    }
}
