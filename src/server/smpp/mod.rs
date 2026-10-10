//! SMPP 3.4 SMSC. Rust owns PDU framing, the bind state machine, sequence numbers, response
//! status codes, message ids, enquire_link, unbind, delivery receipts and every bound; the
//! handler decides binds (unless credentials are configured) and the fate of each message.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncWriteExt, WriteHalf},
    net::{TcpListener, TcpStream},
};
use wire::Pdu;

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
pub const DEFAULT_SYSTEM_ID: &str = "NETGET";

#[derive(Clone)]
struct Config {
    system_id: String,
    credentials: Option<(String, String)>,
    idle: Duration,
    next_message_id: Arc<AtomicU64>,
}

fn config(ctx: &SpawnContext) -> Result<Config> {
    let params = ctx.startup_params.as_ref();
    let get = |name: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(name))
            .transpose()?
            .flatten())
    };
    let system_id = get("system_id")?.unwrap_or_else(|| DEFAULT_SYSTEM_ID.into());
    anyhow::ensure!(
        !system_id.is_empty() && system_id.len() <= 15,
        "system_id must be 1 to 15 characters"
    );
    let credentials = match (get("esme_system_id")?, get("password")?) {
        (Some(id), Some(pw)) => {
            anyhow::ensure!(
                id.len() <= 15 && pw.len() <= 8,
                "esme_system_id is at most 15 characters, password at most 8"
            );
            Some((id, pw))
        }
        (None, None) => None,
        _ => anyhow::bail!("esme_system_id and password are configured together"),
    };
    let secs = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Config {
        system_id,
        credentials,
        idle: Duration::from_secs(secs),
        next_message_id: Arc::new(AtomicU64::new(1)),
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let cfg = config(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("SMPP SMSC listening on {addr}"));
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"",
                "SMPP",
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
            let cfg = cfg.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, id, socket, peer, &cfg).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("SMPP connection {id} ended: {e}"));
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
    Ok(addr)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("SMPP connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The handler's one answer, or `Err(status)` for the failure path (already logged).
async fn decide(
    ctx: &SpawnContext,
    id: ConnectionId,
    event_type: &'static EventType,
    data: Value,
    failure_status: u32,
) -> std::result::Result<(String, Value), u32> {
    let op = event_type.id.clone();
    let event = Event::new(event_type, data);
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::SmppProtocol,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            outcome(ctx, id, &op, "fail_closed_llm_error");
            return Err(
                if crate::llm::is_overload_error(&error) && failure_status == wire::ESME_RSYSERR {
                    wire::ESME_RTHROTTLED
                } else {
                    failure_status
                },
            );
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, &op, "fail_closed_invalid_reply");
        return Err(failure_status);
    }
    let mut found = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(result) = pending.pop() {
        match result {
            ActionResult::Custom { name, data } if name.starts_with("smpp_") => {
                found.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => {
            outcome(ctx, id, &op, "model_silent");
            Err(failure_status)
        }
        _ => {
            outcome(ctx, id, &op, "fail_closed_invalid_reply");
            Err(failure_status)
        }
    }
}

struct Session<'a> {
    ctx: &'a SpawnContext,
    id: ConnectionId,
    writer: WriteHalf<TcpStream>,
    next_sequence: AtomicU32,
}

impl Session<'_> {
    async fn send(&mut self, pdu: Pdu) -> Result<()> {
        let bytes = pdu.encode();
        tokio::time::timeout(wire::IO_TIMEOUT, self.writer.write_all(&bytes))
            .await
            .context("SMPP write deadline")??;
        self.ctx
            .state
            .update_connection_stats(
                self.ctx.server_id,
                self.id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    async fn respond(&mut self, req: &Pdu, status: u32, body: Vec<u8>) -> Result<()> {
        self.send(Pdu::new(
            req.command_id | wire::RESP,
            status,
            req.sequence,
            body,
        ))
        .await
    }

    fn sequence(&self) -> u32 {
        // SMPP sequence numbers run 1..=0x7FFFFFFF.
        let n = self.next_sequence.fetch_add(1, Ordering::Relaxed);
        (n % 0x7FFF_FFFF) + 1
    }
}

fn mode_name(command_id: u32) -> &'static str {
    match command_id {
        wire::BIND_RECEIVER => "receiver",
        wire::BIND_TRANSMITTER => "transmitter",
        _ => "transceiver",
    }
}

fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    cfg: &Config,
) -> Result<()> {
    let (mut reader, writer) = tokio::io::split(socket);
    let mut s = Session {
        ctx,
        id,
        writer,
        next_sequence: AtomicU32::new(0),
    };
    let mut bound: Option<(&'static str, String)> = None;
    loop {
        let Some(pdu) = wire::read_pdu(&mut reader, cfg.idle).await? else {
            return Ok(());
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(pdu.body.len() as u64 + 16),
                None,
                Some(1),
                None,
            )
            .await;
        match pdu.command_id {
            wire::BIND_RECEIVER | wire::BIND_TRANSMITTER | wire::BIND_TRANSCEIVER => {
                if bound.is_some() {
                    s.respond(&pdu, wire::ESME_RALYBND, Vec::new()).await?;
                    continue;
                }
                let Ok(bind) = wire::parse_bind(&pdu.body) else {
                    s.send(Pdu::new(
                        wire::GENERIC_NACK,
                        wire::ESME_RINVCMDLEN,
                        pdu.sequence,
                        Vec::new(),
                    ))
                    .await?;
                    return Ok(());
                };
                let mode = mode_name(pdu.command_id);
                let status = match &cfg.credentials {
                    Some((user, pass)) => {
                        if bind.system_id != *user {
                            wire::ESME_RINVSYSID
                        } else if !same_secret(bind.password.as_bytes(), pass.as_bytes()) {
                            wire::ESME_RINVPASWD
                        } else {
                            wire::ESME_ROK
                        }
                    }
                    None => match decide(
                        ctx,
                        id,
                        &actions::BIND_EVENT,
                        json!({
                            "mode": mode,
                            "system_id": bind.system_id,
                            "password": bind.password,
                            "system_type": bind.system_type,
                            "interface_version": bind.interface_version,
                            "remote_addr": peer.to_string(),
                        }),
                        wire::ESME_RBINDFAIL,
                    )
                    .await
                    {
                        Ok((name, _)) if name == "smpp_bind_accept" => {
                            outcome(ctx, id, "smpp_bind", "model_answer");
                            wire::ESME_ROK
                        }
                        Ok((name, data)) if name == "smpp_bind_reject" => {
                            outcome(ctx, id, "smpp_bind", "model_reject");
                            data["status"]
                                .as_str()
                                .and_then(wire::status_code)
                                .unwrap_or(wire::ESME_RBINDFAIL)
                        }
                        Ok(_) => {
                            outcome(ctx, id, "smpp_bind", "fail_closed_invalid_reply");
                            wire::ESME_RBINDFAIL
                        }
                        Err(status) => status,
                    },
                };
                let mut body = Vec::new();
                if status == wire::ESME_ROK {
                    wire::put_cstring(&mut body, &cfg.system_id, 16);
                }
                s.respond(&pdu, status, body).await?;
                if status != wire::ESME_ROK {
                    return Ok(());
                }
                Log::new(Some(&ctx.status_tx)).info(format!(
                    "SMPP connection {id}: {} bound as {mode}",
                    bind.system_id
                ));
                bound = Some((mode, bind.system_id));
            }
            wire::SUBMIT_SM => {
                let Some((mode, system_id)) = bound.clone() else {
                    s.respond(&pdu, wire::ESME_RINVBNDSTS, vec![0]).await?;
                    continue;
                };
                if mode == "receiver" {
                    s.respond(&pdu, wire::ESME_RINVBNDSTS, vec![0]).await?;
                    continue;
                }
                let message = match wire::parse_message(&pdu.body) {
                    Ok(m) => m,
                    Err(_) => {
                        s.respond(&pdu, wire::ESME_RINVMSGLEN, vec![0]).await?;
                        continue;
                    }
                };
                submit(&mut s, cfg, &pdu, &message, mode, &system_id, peer).await?;
            }
            wire::ENQUIRE_LINK => s.respond(&pdu, wire::ESME_ROK, Vec::new()).await?,
            wire::UNBIND => {
                s.respond(&pdu, wire::ESME_ROK, Vec::new()).await?;
                return Ok(());
            }
            id if id & wire::RESP != 0 => {} // deliver_sm_resp and the like: nothing to do
            _ => {
                s.send(Pdu::new(
                    wire::GENERIC_NACK,
                    wire::ESME_RINVCMDID,
                    pdu.sequence,
                    Vec::new(),
                ))
                .await?;
            }
        }
    }
}

async fn submit(
    s: &mut Session<'_>,
    cfg: &Config,
    pdu: &Pdu,
    message: &wire::Message,
    mode: &str,
    system_id: &str,
    peer: SocketAddr,
) -> Result<()> {
    let (ctx, id) = (s.ctx, s.id);
    let text = wire::decode_text(message.data_coding, &message.payload);
    let mut data = json!({
        "system_id": system_id,
        "source_addr": message.source_addr,
        "destination_addr": message.destination_addr,
        "data_coding": message.data_coding,
        "registered_delivery": message.registered_delivery & 0x03 != 0,
        "esm_class": message.esm_class,
        "remote_addr": peer.to_string(),
    });
    match &text {
        Some(t) => data["text"] = json!(t),
        None => {
            data["data"] = json!(message
                .payload
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>())
        }
    }
    let answer = decide(ctx, id, &actions::SUBMIT_EVENT, data, wire::ESME_RSYSERR).await;
    let accepted = match answer {
        Ok((name, data)) if name == "smpp_accept" => {
            outcome(ctx, id, "smpp_submit", "model_answer");
            data
        }
        Ok((name, data)) if name == "smpp_reject" => {
            outcome(ctx, id, "smpp_submit", "model_reject");
            let status = data["status"]
                .as_str()
                .and_then(wire::status_code)
                .unwrap_or(wire::ESME_RSUBMITFAIL);
            return s.respond(pdu, status, vec![0]).await;
        }
        Ok(_) => {
            outcome(ctx, id, "smpp_submit", "fail_closed_invalid_reply");
            return s.respond(pdu, wire::ESME_RSYSERR, vec![0]).await;
        }
        Err(status) => return s.respond(pdu, status, vec![0]).await,
    };
    let message_id = accepted["message_id"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "NG{:08X}",
                cfg.next_message_id.fetch_add(1, Ordering::Relaxed)
            )
        });
    let mut body = Vec::new();
    wire::put_cstring(&mut body, &message_id, 65);
    s.respond(pdu, wire::ESME_ROK, body).await?;
    if mode == "transmitter" {
        return Ok(());
    }
    let now = chrono::Utc::now().format("%y%m%d%H%M").to_string();
    if let Some(stat) = accepted["receipt"].as_str() {
        if message.registered_delivery & 0x03 != 0 {
            let original = text.clone().unwrap_or_default();
            let receipt = wire::receipt_text(&message_id, stat, &now, &now, &original);
            let mut receipted = message_id.as_bytes().to_vec();
            receipted.push(0);
            let deliver = wire::Message {
                source_ton: message.dest_ton,
                source_npi: message.dest_npi,
                source_addr: message.destination_addr.clone(),
                dest_ton: message.source_ton,
                dest_npi: message.source_npi,
                destination_addr: message.source_addr.clone(),
                esm_class: 0x04,
                data_coding: 0,
                payload: receipt.into_bytes(),
                tlvs: vec![
                    (wire::TLV_RECEIPTED_MESSAGE_ID, receipted),
                    (wire::TLV_MESSAGE_STATE, vec![wire::message_state(stat)]),
                ],
                ..Default::default()
            };
            let seq = s.sequence();
            s.send(Pdu::new(
                wire::DELIVER_SM,
                0,
                seq,
                wire::encode_message(&deliver),
            ))
            .await?;
        }
    }
    if let Some(reply) = accepted["reply_text"].as_str() {
        let (coding, payload) = wire::encode_text(reply);
        let deliver = wire::Message {
            source_ton: message.dest_ton,
            source_npi: message.dest_npi,
            source_addr: message.destination_addr.clone(),
            dest_ton: message.source_ton,
            dest_npi: message.source_npi,
            destination_addr: message.source_addr.clone(),
            data_coding: coding,
            payload,
            ..Default::default()
        };
        let seq = s.sequence();
        s.send(Pdu::new(
            wire::DELIVER_SM,
            0,
            seq,
            wire::encode_message(&deliver),
        ))
        .await?;
    }
    Ok(())
}
