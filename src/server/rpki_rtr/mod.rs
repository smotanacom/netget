//! RPKI-to-Router cache (RFC 8210 version 1, RFC 6810 version 0). Rust owns framing, the
//! session id, timers and version negotiation; the handler owns the VRP data.
pub mod actions;
pub mod codec;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use actions::Response;
use anyhow::{bail, Context, Result};
use codec::{Intervals, Packet, Pdu};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Mutex};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(codec::DEFAULT_EXPIRE_SECONDS as u64);

struct Shared {
    ctx: SpawnContext,
    session_id: u16,
    intervals: Intervals,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let u64_param = |k: &str| {
        p.map(|p| p.get_optional_u64(k))
            .transpose()
            .map(Option::flatten)
    };
    let session_id = match u64_param("session_id")? {
        Some(v) => u16::try_from(v).context("session_id must be 0..=65535")?,
        None => rand::random::<u16>(),
    };
    let u32_param = |k: &str, default: u32| -> Result<u32> {
        Ok(match u64_param(k)? {
            Some(v) => u32::try_from(v).with_context(|| format!("{k} must fit in 32 bits"))?,
            None => default,
        })
    };
    let intervals = Intervals {
        refresh: u32_param("refresh_interval_secs", codec::DEFAULT_REFRESH_SECONDS)?,
        retry: u32_param("retry_interval_secs", codec::DEFAULT_RETRY_SECONDS)?,
        expire: u32_param("expire_interval_secs", codec::DEFAULT_EXPIRE_SECONDS)?,
    };
    intervals.validate()?;
    let idle = codec::seconds(
        u64_param("idle_timeout_secs")?,
        IDLE_TIMEOUT.as_secs(),
        172800,
        "idle_timeout_secs",
    )?;
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "RPKI-RTR cache listening on {addr}, session id {session_id}"
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        session_id,
        intervals,
        idle,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        // An Error Report (Internal Error) is the protocol's own way to refuse a router.
        let refusal = codec::error_report(1, 1, &[], "too many connections")
            .encode()
            .unwrap_or_default();
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                &refusal,
                "RPKI-RTR",
                Some(&shared.ctx.status_tx),
            )
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
                        local_addr: addr,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: now,
                        status: ConnectionStatus::Active,
                        status_changed_at: now,
                        protocol_info: ProtocolConnectionInfo::new(
                            json!({"session_id": session_id}),
                        ),
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
                            .debug(format!("RPKI-RTR connection {id} ended: {e}"));
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
    Ok(addr)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("RPKI-RTR connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

type Writer = Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>;

async fn send(
    shared: &Shared,
    id: ConnectionId,
    writer: &Writer,
    packets: &[Packet],
) -> Result<usize> {
    let mut bytes = Vec::new();
    for p in packets {
        bytes.extend(p.encode()?);
    }
    let mut w = writer.lock().await;
    tokio::time::timeout(codec::WRITE_TIMEOUT, async {
        w.write_all(&bytes).await?;
        w.flush().await
    })
    .await
    .context("RPKI-RTR write deadline")??;
    drop(w);
    shared
        .ctx
        .state
        .update_connection_stats(
            shared.ctx.server_id,
            id,
            None,
            Some(bytes.len() as u64),
            None,
            Some(packets.len() as u64),
        )
        .await;
    Ok(bytes.len())
}

/// The session's version once the router's first PDU fixed it (RFC 8210 §7).
type Version = Arc<Mutex<Option<u8>>>;

async fn connection(
    shared: &Arc<Shared>,
    id: ConnectionId,
    socket: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let (mut reader, writer) = tokio::io::split(socket);
    let writer: Writer = Arc::new(Mutex::new(writer));
    let version: Version = Arc::new(Mutex::new(None));
    // Serial Notify and disconnect from the dashboard or MCP, written through the same writer.
    let peer_rx =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    let notifier = ctx
        .state
        .spawn_server_task(
            ctx.server_id,
            peer_commands(shared.clone(), id, peer_rx, writer.clone(), version.clone()),
        )
        .await;
    let result = serve(shared, id, &mut reader, &writer, &version).await;
    notifier.abort();
    result
}

async fn peer_commands(
    shared: Arc<Shared>,
    id: ConnectionId,
    mut rx: mpsc::Receiver<ClientCommand>,
    writer: Writer,
    version: Version,
) {
    while let Some(command) = rx.recv().await {
        let outcome: Result<ClientSendOutcome> = async {
            match command.action["type"].as_str() {
                Some("rpki_rtr_serial_notify") => {
                    let serial = actions::serial(&command.action)?;
                    let v = version.lock().await.unwrap_or(1);
                    let n = send(
                        &shared,
                        id,
                        &writer,
                        &[Packet {
                            version: v,
                            pdu: Pdu::SerialNotify {
                                session: shared.session_id,
                                serial,
                            },
                        }],
                    )
                    .await?;
                    Ok(ClientSendOutcome::Sent { bytes_sent: n })
                }
                Some("disconnect") => {
                    let _ = writer.lock().await.shutdown().await;
                    Ok(ClientSendOutcome::Disconnected)
                }
                _ => Ok(ClientSendOutcome::Rejected {
                    error: "RPKI-RTR peers accept rpki_rtr_serial_notify or disconnect".into(),
                }),
            }
        }
        .await;
        let record = match &outcome {
            Ok(o) => serde_json::to_value(o).unwrap_or(Value::Null),
            Err(e) => json!({"error": e.to_string()}),
        };
        shared
            .ctx
            .state
            .record_access_log(
                crate::state::AccessLogOwner::Server(shared.ctx.server_id.as_u32()),
                "RPKI-RTR",
                Some(id.as_u32()),
                "injected_action",
                command.action.clone(),
                vec![record],
            )
            .await;
        let _ = command.reply_tx.send(outcome);
    }
}

async fn decide(shared: &Shared, id: ConnectionId, event: Event) -> Result<Response> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::RpkiRtrProtocol,
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
        bail!("RPKI-RTR handler supplied an invalid action");
    }
    let mut found = None;
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name == "rpki_rtr_response" => {
                if found.replace(data).is_some() {
                    outcome(ctx, id, event.id(), "fail_closed_invalid_reply");
                    bail!("RPKI-RTR handler supplied more than one response");
                }
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    let Some(data) = found else {
        outcome(ctx, id, event.id(), "model_silent");
        bail!("RPKI-RTR handler did not answer");
    };
    Response::from_action(&data)
}

async fn serve(
    shared: &Arc<Shared>,
    id: ConnectionId,
    reader: &mut tokio::io::ReadHalf<tokio::net::TcpStream>,
    writer: &Writer,
    version: &Version,
) -> Result<()> {
    let ctx = &shared.ctx;
    loop {
        let bytes = match codec::read_frame(reader, shared.idle).await {
            Ok(b) => b,
            // A clean close between PDUs is the router going away.
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                return Ok(())
            }
            Err(e) => {
                let v = version.lock().await.unwrap_or(1);
                let _ = send(
                    shared,
                    id,
                    writer,
                    &[codec::error_report(
                        v,
                        0,
                        &[],
                        "malformed or over-long PDU header",
                    )],
                )
                .await;
                return Err(e);
            }
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(bytes.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;
        let wire_version = bytes[0];
        let session_version = {
            let mut slot = version.lock().await;
            match *slot {
                None if wire_version > 1 => {
                    // RFC 8210 §7: answer with our highest version, then the router may retry lower.
                    let _ = send(
                        shared,
                        id,
                        writer,
                        &[codec::error_report(
                            1,
                            4,
                            &bytes,
                            "this cache speaks RTR versions 0 and 1",
                        )],
                    )
                    .await;
                    outcome(ctx, id, "version", "protocol_refusal");
                    return Ok(());
                }
                None => {
                    *slot = Some(wire_version);
                    wire_version
                }
                Some(v) if v != wire_version => {
                    let _ = send(
                        shared,
                        id,
                        writer,
                        &[codec::error_report(
                            v,
                            8,
                            &bytes,
                            "version changed within the session",
                        )],
                    )
                    .await;
                    outcome(ctx, id, "version", "protocol_refusal");
                    return Ok(());
                }
                Some(v) => v,
            }
        };
        let packet = match Packet::decode(&bytes) {
            Ok(p) => p,
            Err(e) => {
                let code = if matches!(bytes[1], 0..=4 | 6..=8 | 10) {
                    0
                } else {
                    5
                };
                let _ = send(
                    shared,
                    id,
                    writer,
                    &[codec::error_report(
                        session_version,
                        code,
                        &bytes,
                        &e.to_string(),
                    )],
                )
                .await;
                outcome(ctx, id, "decode", "protocol_refusal");
                return Err(e);
            }
        };
        let (event, router_serial) = match packet.pdu {
            Pdu::ResetQuery => (
                Event::new(
                    &actions::RESET_QUERY_EVENT,
                    json!({"session_id": shared.session_id, "version": session_version}),
                ),
                None,
            ),
            Pdu::SerialQuery { session, serial } => {
                if session != shared.session_id {
                    // Not our session: the router's data is from another cache instance.
                    send(
                        shared,
                        id,
                        writer,
                        &[Packet {
                            version: session_version,
                            pdu: Pdu::CacheReset,
                        }],
                    )
                    .await?;
                    outcome(ctx, id, "serial_query", "protocol_cache_reset");
                    continue;
                }
                (
                    Event::new(
                        &actions::SERIAL_QUERY_EVENT,
                        json!({"session_id": shared.session_id, "version": session_version, "router_serial": serial}),
                    ),
                    Some(serial),
                )
            }
            Pdu::ErrorReport {
                code, diagnostic, ..
            } => {
                Log::new(Some(&ctx.status_tx)).warn(format!(
                    "RPKI-RTR router on connection {id} reported {} ({code}): {}",
                    codec::error_name(code),
                    crate::utils::sanitize::strip_controls(&diagnostic)
                ));
                return Ok(());
            }
            _ => {
                let _ = send(
                    shared,
                    id,
                    writer,
                    &[codec::error_report(
                        session_version,
                        3,
                        &bytes,
                        "a router sends Reset Query, Serial Query or Error Report",
                    )],
                )
                .await;
                outcome(ctx, id, "pdu", "protocol_refusal");
                return Ok(());
            }
        };
        let operation = event.id().to_owned();
        // `decide` logs which way it failed; `check` failures are logged here.
        let answer = match decide(shared, id, event).await {
            Ok(r) => check(r, router_serial)
                .inspect_err(|_| outcome(ctx, id, &operation, "fail_closed_invalid_reply")),
            Err(e) => Err(e),
        };
        let answer = match answer {
            Ok(r) => r,
            Err(_) => {
                // RFC 8210 §12: an Internal Error tells the router to try again later.
                send(
                    shared,
                    id,
                    writer,
                    &[codec::error_report(
                        session_version,
                        1,
                        &bytes,
                        "the cache cannot answer right now",
                    )],
                )
                .await?;
                return Ok(());
            }
        };
        let packets = match answer {
            Response::CacheReset => {
                outcome(ctx, id, &operation, "model_cache_reset");
                vec![Packet {
                    version: session_version,
                    pdu: Pdu::CacheReset,
                }]
            }
            Response::NoData => {
                outcome(ctx, id, &operation, "model_no_data");
                vec![codec::error_report(
                    session_version,
                    2,
                    &bytes,
                    "no data available",
                )]
            }
            Response::Data { serial, records } => {
                outcome(ctx, id, &operation, "model_answer");
                let mut out = Vec::with_capacity(records.len() + 2);
                out.push(Packet {
                    version: session_version,
                    pdu: Pdu::CacheResponse {
                        session: shared.session_id,
                    },
                });
                for r in records {
                    out.push(Packet {
                        version: session_version,
                        pdu: Pdu::Prefix(r.canonical()?),
                    });
                }
                out.push(Packet {
                    version: session_version,
                    pdu: Pdu::EndOfData {
                        session: shared.session_id,
                        serial,
                        intervals: shared.intervals,
                    },
                });
                out
            }
        };
        // No Data Available is the one non-fatal Error Report: the session stays open.
        send(shared, id, writer, &packets).await?;
    }
}

/// Protocol rules on top of a well-formed answer.
fn check(response: Response, router_serial: Option<u32>) -> Result<Response> {
    match (&response, router_serial) {
        (Response::CacheReset, None) => {
            bail!("cache_reset answers a serial query, not a reset query")
        }
        (Response::Data { records, .. }, None) => {
            anyhow::ensure!(
                records.iter().all(|r| r.announcement),
                "a reset answer cannot withdraw records"
            );
        }
        (Response::Data { serial, .. }, Some(old)) => {
            anyhow::ensure!(
                *serial == old || codec::serial_newer(*serial, old),
                "serial {serial} is older than the router's {old}"
            );
        }
        _ => {}
    }
    Ok(response)
}
