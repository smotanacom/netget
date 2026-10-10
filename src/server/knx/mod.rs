//! KNXnet/IP tunnelling gateway. Rust owns the frames, the tunnels (channels, sequence numbers,
//! heartbeats), acknowledgements, confirmations and the routing of group telegrams between
//! tunnels; the handler is every device on the bus, answering reads and reacting to writes.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use wire::{Apci, Data, Telegram};

pub const DEFAULT_ADDRESS: &str = "1.1.250";
pub const DEFAULT_MAX_TUNNELS: u64 = 8;
pub const MAX_TUNNELS_CEILING: u64 = 64;
/// A tunnel that sends nothing (not even a CONNECTIONSTATE heartbeat) for this long is gone.
pub const TUNNEL_TIMEOUT: Duration = Duration::from_secs(120);
/// Telegrams waiting for the handler at once; past this, telegrams are dropped (logged).
pub const TELEGRAM_QUEUE: usize = 64;

struct Tunnel {
    control: SocketAddr,
    data: SocketAddr,
    address: u16,
    /// The sequence number expected next from the client.
    seq_in: u8,
    /// The sequence number of the next request NetGet sends.
    seq_out: u8,
    last_seen: crate::utils::clock::Instant,
    connection: ConnectionId,
}

struct Gateway {
    socket: UdpSocket,
    tunnels: Mutex<BTreeMap<u8, Tunnel>>,
    address: u16,
    types: HashMap<u16, String>,
    max_tunnels: usize,
    local: SocketAddr,
}

impl Gateway {
    /// Send a telegram as L_Data.ind to every tunnel except `except`.
    async fn broadcast(&self, telegram: &Telegram, except: Option<u8>) {
        let cemi = wire::cemi(&Telegram {
            message_code: wire::L_DATA_IND,
            ..telegram.clone()
        });
        let mut sends = Vec::new();
        {
            let mut tunnels = self.tunnels.lock().await;
            for (channel, t) in tunnels.iter_mut() {
                if Some(*channel) == except {
                    continue;
                }
                sends.push((t.data, wire::tunnelling(*channel, t.seq_out, &cemi)));
                t.seq_out = t.seq_out.wrapping_add(1);
            }
        }
        for (to, bytes) in sends {
            let _ = self.socket.send_to(&bytes, to).await;
        }
    }

    fn describe(&self, d: &Data, destination: u16) -> (Option<String>, Value, Value) {
        let dpt = self.types.get(&destination).cloned();
        let value = dpt
            .as_deref()
            .and_then(|t| wire::decode(t, d).ok())
            .unwrap_or(Value::Null);
        (dpt, value, wire::interpretations(d))
    }
}

fn config(ctx: &SpawnContext) -> Result<(u16, HashMap<u16, String>, usize)> {
    let params = ctx.startup_params.as_ref();
    let address = params
        .map(|p| p.get_optional_string("individual_address"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_ADDRESS.into());
    let address = wire::parse_individual(&address)?;
    let max = params
        .map(|p| p.get_optional_u64("max_tunnels"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_MAX_TUNNELS);
    ensure!(
        (1..=MAX_TUNNELS_CEILING).contains(&max),
        "max_tunnels must be 1..={MAX_TUNNELS_CEILING}"
    );
    let mut types = HashMap::new();
    if let Some(map) = params
        .map(|p| p.get_optional_object("group_types"))
        .transpose()?
        .flatten()
    {
        for (ga, dpt) in map {
            let dpt = dpt.as_str().context("group_types values are DPT names")?;
            wire::dpt_main(dpt)?;
            types.insert(wire::parse_group(ga)?, dpt.to_string());
        }
    }
    Ok((address, types, max as usize))
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let (address, types, max_tunnels) = config(&ctx)?;
    let socket = UdpSocket::bind(ctx.legacy_listen_addr()).await?;
    let local = socket.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "KNXnet/IP gateway {} listening on {local}",
        wire::format_individual(address)
    ));
    let gw = Arc::new(Gateway {
        socket,
        tunnels: Mutex::new(BTreeMap::new()),
        address,
        types,
        max_tunnels,
        local,
    });
    let (tx, rx) = mpsc::channel::<(u8, Telegram)>(TELEGRAM_QUEUE);
    let worker_ctx = ctx.clone();
    let worker_gw = gw.clone();
    ctx.state
        .register_server_task(
            ctx.server_id,
            tokio::spawn(worker(worker_ctx, worker_gw, rx)),
        )
        .await;
    let reaper_ctx = ctx.clone();
    let reaper_gw = gw.clone();
    ctx.state
        .register_server_task(
            ctx.server_id,
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    let gone: Vec<(u8, ConnectionId, SocketAddr)> = {
                        let mut tunnels = reaper_gw.tunnels.lock().await;
                        let stale: Vec<u8> = tunnels
                            .iter()
                            .filter(|(_, t)| t.last_seen.elapsed() > TUNNEL_TIMEOUT)
                            .map(|(c, _)| *c)
                            .collect();
                        stale
                            .into_iter()
                            .filter_map(|c| {
                                tunnels.remove(&c).map(|t| (c, t.connection, t.control))
                            })
                            .collect()
                    };
                    for (channel, id, control) in gone {
                        // The gateway closes a tunnel it has given up on, so the client knows.
                        let mut body = vec![channel, 0];
                        body.extend(wire::hpai(reaper_gw.local));
                        let _ = reaper_gw
                            .socket
                            .send_to(&wire::frame(wire::DISCONNECT_REQUEST, &body), control)
                            .await;
                        Log::new(Some(&reaper_ctx.status_tx))
                            .info(format!("KNX tunnel {channel} timed out"));
                        reaper_ctx
                            .state
                            .update_connection_status(
                                reaper_ctx.server_id,
                                id,
                                ConnectionStatus::Closed,
                            )
                            .await;
                    }
                }
            }),
        )
        .await;
    let recv_ctx = ctx.clone();
    ctx.state
        .register_server_task(
            ctx.server_id,
            tokio::spawn(async move {
                let mut buf = vec![0u8; wire::MAX_FRAME + 1];
                loop {
                    let (n, from) = match gw.socket.recv_from(&mut buf).await {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Err(e) = handle(&recv_ctx, &gw, &tx, &buf[..n], from).await {
                        Log::new(Some(&recv_ctx.status_tx))
                            .debug(format!("KNX frame from {from} ignored: {e:#}"));
                    }
                }
            }),
        )
        .await;
    Ok(local)
}

fn device_info(gw: &Gateway) -> Vec<u8> {
    // Device information DIB (54 bytes) then supported service families DIB.
    let mut d = vec![54, 0x01, 0x02, 0x00];
    d.extend(gw.address.to_be_bytes());
    d.extend([0, 0]); // project installation id
    d.extend([0x00, 0x00, 0x4e, 0x47, 0x00, 0x01]); // serial number
    d.extend([0, 0, 0, 0]); // routing multicast address (unused)
    d.extend([0x00, 0x00, 0x4e, 0x47, 0x00, 0x01]); // MAC
    let mut name = b"NetGet KNX/IP".to_vec();
    name.resize(30, 0);
    d.extend(name);
    d.extend([8, 0x02, 0x02, 0x01, 0x03, 0x01, 0x04, 0x01]); // core, device mgmt, tunnelling v1
    d
}

async fn handle(
    ctx: &SpawnContext,
    gw: &Gateway,
    tx: &mpsc::Sender<(u8, Telegram)>,
    frame: &[u8],
    from: SocketAddr,
) -> Result<()> {
    let (service, body) = wire::parse_frame(frame)?;
    match service {
        wire::SEARCH_REQUEST | wire::DESCRIPTION_REQUEST => {
            let (hpai, _) = wire::parse_hpai(body)?;
            let to = wire::endpoint(hpai, from);
            let mut reply = Vec::new();
            if service == wire::SEARCH_REQUEST {
                reply.extend(wire::hpai(gw.local));
            }
            reply.extend(device_info(gw));
            let kind = if service == wire::SEARCH_REQUEST {
                wire::SEARCH_RESPONSE
            } else {
                wire::DESCRIPTION_RESPONSE
            };
            gw.socket.send_to(&wire::frame(kind, &reply), to).await?;
        }
        wire::CONNECT_REQUEST => {
            let (control, n) = wire::parse_hpai(body)?;
            let (data, m) = wire::parse_hpai(&body[n..])?;
            let cri = body.get(n + m..).context("missing CRI")?;
            let control = wire::endpoint(control, from);
            let data = wire::endpoint(data, from);
            let refuse = |status: u8| wire::frame(wire::CONNECT_RESPONSE, &[0, status]);
            if cri.len() < 4 || cri[1] != 0x04 {
                gw.socket
                    .send_to(&refuse(wire::E_CONNECTION_TYPE), control)
                    .await?;
                return Ok(());
            }
            if cri[2] != 0x02 {
                gw.socket
                    .send_to(&refuse(wire::E_CONNECTION_OPTION), control)
                    .await?;
                return Ok(());
            }
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let mut tunnels = gw.tunnels.lock().await;
            let channel = (1..=255u8).find(|c| !tunnels.contains_key(c));
            let address = (1..=255u16)
                .map(|d| (gw.address & 0xff00) | d)
                .find(|a| *a != gw.address && tunnels.values().all(|t| t.address != *a));
            let (Some(channel), Some(address), true) =
                (channel, address, tunnels.len() < gw.max_tunnels)
            else {
                drop(tunnels);
                gw.socket
                    .send_to(&refuse(wire::E_NO_MORE_CONNECTIONS), control)
                    .await?;
                return Ok(());
            };
            let now = crate::utils::clock::Instant::now();
            tunnels.insert(
                channel,
                Tunnel {
                    control,
                    data,
                    address,
                    seq_in: 0,
                    seq_out: 0,
                    last_seen: now,
                    connection: id,
                },
            );
            drop(tunnels);
            ctx.state
                .add_connection_to_server(
                    ctx.server_id,
                    ConnectionState {
                        id,
                        remote_addr: control,
                        local_addr: gw.local,
                        bytes_sent: 0,
                        bytes_received: 0,
                        packets_sent: 0,
                        packets_received: 0,
                        last_activity: now,
                        status: ConnectionStatus::Active,
                        status_changed_at: now,
                        protocol_info: ProtocolConnectionInfo::new(json!({
                            "channel": channel, "individual_address": wire::format_individual(address)
                        })),
                    },
                )
                .await;
            let mut reply = vec![channel, wire::E_NO_ERROR];
            reply.extend(wire::hpai(gw.local));
            reply.extend([4, 0x04]);
            reply.extend(address.to_be_bytes());
            gw.socket
                .send_to(&wire::frame(wire::CONNECT_RESPONSE, &reply), control)
                .await?;
            Log::new(Some(&ctx.status_tx)).info(format!(
                "KNX tunnel {channel} opened for {control} as {}",
                wire::format_individual(address)
            ));
        }
        wire::CONNECTIONSTATE_REQUEST | wire::DISCONNECT_REQUEST => {
            ensure!(body.len() >= 10, "short connection request");
            let channel = body[0];
            let (control, _) = wire::parse_hpai(&body[2..])?;
            let control = wire::endpoint(control, from);
            let (status, closed) = {
                let mut tunnels = gw.tunnels.lock().await;
                let known = tunnels.contains_key(&channel);
                let status = if known {
                    wire::E_NO_ERROR
                } else {
                    wire::E_CONNECTION_ID
                };
                if service == wire::CONNECTIONSTATE_REQUEST {
                    if let Some(t) = tunnels.get_mut(&channel) {
                        t.last_seen = crate::utils::clock::Instant::now();
                    }
                    (status, None)
                } else {
                    (status, tunnels.remove(&channel).map(|t| t.connection))
                }
            };
            let reply = if service == wire::CONNECTIONSTATE_REQUEST {
                wire::frame(wire::CONNECTIONSTATE_RESPONSE, &[channel, status])
            } else {
                wire::frame(wire::DISCONNECT_RESPONSE, &[channel, status])
            };
            gw.socket.send_to(&reply, control).await?;
            if let Some(id) = closed {
                ctx.state
                    .update_connection_status(ctx.server_id, id, ConnectionStatus::Closed)
                    .await;
                Log::new(Some(&ctx.status_tx)).info(format!("KNX tunnel {channel} closed"));
            }
        }
        wire::DISCONNECT_RESPONSE | wire::TUNNELLING_ACK => {
            // Acks for NetGet's own requests: nothing is retransmitted, so nothing to clear.
            if let Some(channel) = body.get(1) {
                if let Some(t) = gw.tunnels.lock().await.get_mut(channel) {
                    t.last_seen = crate::utils::clock::Instant::now();
                }
            }
        }
        wire::TUNNELLING_REQUEST => {
            ensure!(
                body.len() >= 4 && body[0] == 4,
                "malformed connection header"
            );
            let (channel, seq) = (body[1], body[2]);
            let (data_endpoint, fresh, address) = {
                let mut tunnels = gw.tunnels.lock().await;
                let Some(t) = tunnels.get_mut(&channel) else {
                    return Ok(());
                };
                t.last_seen = crate::utils::clock::Instant::now();
                let fresh = seq == t.seq_in;
                // A repeat of the last request is acknowledged again and not processed; any
                // other sequence number is dropped unanswered, as the specification says.
                if !fresh && seq != t.seq_in.wrapping_sub(1) {
                    return Ok(());
                }
                if fresh {
                    t.seq_in = t.seq_in.wrapping_add(1);
                }
                (t.data, fresh, t.address)
            };
            let parsed = wire::parse_cemi(&body[4..]);
            let status = if parsed.is_ok() {
                wire::E_NO_ERROR
            } else {
                0x29
            };
            gw.socket
                .send_to(&wire::tunnelling_ack(channel, seq, status), data_endpoint)
                .await?;
            let Ok(Some(mut telegram)) = parsed else {
                return Ok(());
            };
            if !fresh || telegram.message_code != wire::L_DATA_REQ {
                return Ok(());
            }
            // The tunnel's own address is the source of what it sends.
            telegram.source = address;
            let con = Telegram {
                message_code: wire::L_DATA_CON,
                ..telegram.clone()
            };
            let bytes = {
                let mut tunnels = gw.tunnels.lock().await;
                let Some(t) = tunnels.get_mut(&channel) else {
                    return Ok(());
                };
                let b = wire::tunnelling(channel, t.seq_out, &wire::cemi(&con));
                t.seq_out = t.seq_out.wrapping_add(1);
                b
            };
            gw.socket.send_to(&bytes, data_endpoint).await?;
            // On the bus, every other tunnel hears it.
            gw.broadcast(&telegram, Some(channel)).await;
            if tx.try_send((channel, telegram)).is_err() {
                Log::new(Some(&ctx.status_tx))
                    .warn("KNX telegram queue full: telegram not handed to the handler");
            }
        }
        other => {
            Log::new(Some(&ctx.status_tx))
                .debug(format!("KNX service {other:#06x} from {from} not handled"));
        }
    }
    Ok(())
}

fn outcome(ctx: &SpawnContext, op: &str, decision: &str) {
    let line = format!("KNX operation={op} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed") {
        log.error(line)
    } else {
        log.info(line)
    }
}

/// The handler, one telegram at a time.
async fn worker(ctx: SpawnContext, gw: Arc<Gateway>, mut rx: mpsc::Receiver<(u8, Telegram)>) {
    while let Some((channel, telegram)) = rx.recv().await {
        let connection = gw.tunnels.lock().await.get(&channel).map(|t| t.connection);
        let source = wire::format_individual(telegram.source);
        let destination = wire::format_group(telegram.destination);
        let (dpt, value, interpretations) = gw.describe(&telegram.data, telegram.destination);
        let (event, op) = match telegram.apci {
            Apci::Read => (
                Event::new(
                    &actions::READ_EVENT,
                    json!({"source": source, "destination": destination, "dpt": dpt}),
                ),
                "group_read",
            ),
            kind => (
                Event::new(
                    &actions::TELEGRAM_EVENT,
                    json!({"kind": kind.name(), "source": source, "destination": destination,
                           "dpt": dpt, "value": value, "interpretations": interpretations}),
                ),
                "group_telegram",
            ),
        };
        let result = match call_llm(
            &ctx.llm_client,
            &ctx.state,
            ctx.server_id,
            connection,
            &event,
            &actions::KnxProtocol,
        )
        .await
        {
            Ok(r) if r.failures.is_empty() => r,
            Ok(_) => {
                outcome(&ctx, op, "fail_closed_invalid_reply");
                continue;
            }
            Err(_) => {
                outcome(&ctx, op, "fail_closed_llm_error");
                continue;
            }
        };
        let mut answers = Vec::new();
        let mut stack = result.protocol_results;
        while let Some(r) = stack.pop() {
            match r {
                ActionResult::Custom { name, data } if name.starts_with("knx_") => {
                    answers.push((name, data))
                }
                ActionResult::Multiple(items) => stack.extend(items),
                _ => {}
            }
        }
        answers.reverse();
        if answers.is_empty() {
            outcome(&ctx, op, "model_silent");
        }
        for (name, data) in answers {
            let sent = match name.as_str() {
                "knx_group_response" if telegram.apci == Apci::Read => {
                    actions::value_of(&data, dpt.as_deref()).map(|d| Telegram {
                        message_code: wire::L_DATA_IND,
                        source: gw.address,
                        destination: telegram.destination,
                        apci: Apci::Response,
                        data: d,
                    })
                }
                "knx_group_write" => (|| {
                    let ga = wire::parse_group(data["group_address"].as_str().unwrap_or_default())?;
                    let d = actions::value_of(&data, gw.types.get(&ga).map(String::as_str))?;
                    Ok(Telegram {
                        message_code: wire::L_DATA_IND,
                        source: gw.address,
                        destination: ga,
                        apci: Apci::Write,
                        data: d,
                    })
                })(),
                "knx_ignore" => {
                    outcome(&ctx, op, "model_ignore");
                    continue;
                }
                _ => continue,
            };
            match sent {
                Ok(t) => {
                    outcome(&ctx, op, "model_answer");
                    gw.broadcast(&t, None).await;
                }
                Err(e) => {
                    Log::new(Some(&ctx.status_tx)).error(format!("KNX answer refused: {e:#}"));
                    outcome(&ctx, op, "fail_closed_invalid_reply");
                }
            }
        }
    }
}
