//! PFCP client: the control-plane side (an SMF) of one N4 association with one UPF.
pub mod actions;

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event, EventType};
use crate::server::pfcp::wire;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::PfcpClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// TS 29.244 §6.4: retransmit an unanswered request after T1, at most N1 times.
pub const T1: Duration = Duration::from_secs(3);
pub const N1: u32 = 3;
/// Requests awaiting an answer at once; the next is refused.
pub const MAX_PENDING: usize = 64;
/// How many model turns may follow from one another before the chain stops.
pub const MAX_FOLLOWUP_DEPTH: u32 = 8;
const TURNS: usize = 64;

struct Pending {
    request: u8,
    bytes: Vec<u8>,
    sent: u32,
    deadline: Instant,
    depth: u32,
    cp_seid: Option<u64>,
    command: Option<ClientCommand>,
}

struct Smf {
    socket: UdpSocket,
    upf: SocketAddr,
    node_id: Value,
    ip: IpAddr,
    recovery: u64,
    seq: u32,
    next_cp_seid: u64,
    /// cp_seid → the UPF's SEID (0 until the establishment is answered).
    sessions: HashMap<u64, u64>,
    pending: HashMap<u32, Pending>,
    /// Requests from the UPF awaiting the model's pfcp_respond: sequence → (type, cp_seid).
    inbound: HashMap<u32, (u8, Option<u64>)>,
    events: mpsc::Sender<(Event, u32)>,
    client: crate::state::ClientId,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let raw = ctx.remote_addr.trim();
    let upf = match raw.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, wire::PORT),
        Err(_) => tokio::net::lookup_host(raw)
            .await
            .with_context(|| format!("cannot resolve {raw}"))?
            .next()
            .with_context(|| format!("{raw} resolved to no address"))?,
    };
    let local_port = params
        .map(|p| p.get_optional_u64("local_port"))
        .transpose()?
        .flatten()
        .unwrap_or(0);
    anyhow::ensure!(local_port <= 65535, "local_port is a port number");
    let socket = UdpSocket::bind(SocketAddr::new(
        if upf.is_ipv4() {
            [0, 0, 0, 0].into()
        } else {
            [0u16; 8].into()
        },
        local_port as u16,
    ))
    .await?;
    socket
        .connect(upf)
        .await
        .with_context(|| format!("no route to {upf}"))?;
    let local = socket.local_addr()?;
    let node_id = match params
        .map(|p| p.get_optional_string("node_id"))
        .transpose()?
        .flatten()
    {
        Some(n) if n.parse::<std::net::Ipv4Addr>().is_ok() => json!({"ipv4": n}),
        Some(n) => json!({"fqdn": n}),
        None => match local.ip() {
            IpAddr::V4(a) => json!({"ipv4": a.to_string()}),
            IpAddr::V6(a) => json!({"ipv6": a.to_string()}),
        },
    };
    wire::encode(5, None, 0, &json!({"node_id": node_id})).context("node_id")?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "PFCP client {} (SMF {node_id}) towards UPF {upf} from {local}",
        ctx.client_id
    ));
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (event_tx, event_rx) = mpsc::channel::<(Event, u32)>(TURNS);
    let (internal_tx, internal) = mpsc::channel::<(Value, u32)>(64);
    let _ = event_tx.try_send((
        Event::new(
            &actions::READY_EVENT,
            json!({"upf": upf.to_string(), "node_id": node_id}),
        ),
        0,
    ));
    let dispatcher = tokio::spawn(run_turns(ctx.clone(), event_rx, internal_tx));
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let mut smf = Smf {
        socket,
        upf,
        node_id,
        ip: local.ip(),
        recovery: wire::now_unix(),
        seq: rand::random::<u16>() as u32,
        next_cp_seid: 1,
        sessions: HashMap::new(),
        pending: HashMap::new(),
        inbound: HashMap::new(),
        events: event_tx,
        client: ctx.client_id,
    };
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = smf.run(&session_ctx, external, internal).await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("PFCP client ended: {e:#}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, status)
            .await;
        session_ctx
            .state
            .remove_client_handle(session_ctx.client_id)
            .await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

fn reply(command: Option<ClientCommand>, outcome: ClientSendOutcome) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, Ok(outcome));
    }
}

fn merge(base: Value, extra: &Value) -> Value {
    let mut m: Map<String, Value> = match base {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Value::Object(e) = extra {
        for (k, v) in e {
            m.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    Value::Object(m)
}

impl Smf {
    fn emit(&self, t: &'static EventType, data: Value, depth: u32) {
        if self.events.try_send((Event::new(t, data), depth)).is_err() {
            tracing::warn!("PFCP client {} dropped an event: the model is {TURNS} behind decision=turn_queue_full", self.client);
        }
    }

    fn fseid(&self, seid: u64) -> Value {
        let mut f = json!({"seid": seid});
        match self.ip {
            IpAddr::V4(a) => f["ipv4"] = json!(a.to_string()),
            IpAddr::V6(a) => f["ipv6"] = json!(a.to_string()),
        }
        f
    }

    async fn run(
        &mut self,
        ctx: &ConnectContext,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<(Value, u32)>,
    ) -> Result<()> {
        let mut buf = vec![0u8; 65_536];
        loop {
            let deadline = self.pending.values().map(|p| p.deadline).min();
            let (action, depth, command) = tokio::select! {
                r = self.socket.recv(&mut buf) => {
                    match r {
                        Ok(n) => {
                            let datagram = buf[..n].to_vec();
                            self.incoming(&datagram).await;
                        }
                        // ICMP port unreachable surfaces here on a connected socket; the
                        // retransmission timer reports the request.
                        Err(e) => tracing::debug!("PFCP client {}: {e}", self.client),
                    }
                    continue;
                }
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.retransmit().await;
                    continue;
                }
                c = external.recv() => match c {
                    Some(c) => {
                        ctx.state.record_access_log(AccessLogOwner::Client(ctx.client_id.as_u32()), "PFCP", None,
                            "injected_action", c.action.clone(), vec![]).await;
                        (c.action.clone(), 0, Some(c))
                    }
                    None => return Ok(()),
                },
                a = internal.recv() => match a {
                    Some((a, d)) => (a, d, None),
                    None => return Ok(()),
                },
            };
            match PfcpClientProtocol.execute_action(action.clone()) {
                Ok(ClientActionResult::Disconnect) => {
                    reply(command, ClientSendOutcome::Disconnected);
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    reply(
                        command,
                        ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                    );
                    continue;
                }
            }
            if let Err(e) = self.perform(&action, depth, command).await {
                tracing::warn!("PFCP client {}: {e:#}", self.client);
            }
        }
    }

    async fn perform(
        &mut self,
        action: &Value,
        depth: u32,
        command: Option<ClientCommand>,
    ) -> Result<()> {
        let extra = action.get("ies").cloned().unwrap_or(Value::Null);
        let kind = action["type"].as_str().unwrap_or_default();
        if kind == "pfcp_respond" {
            let seq = action["sequence"].as_u64().unwrap_or(0) as u32;
            let Some((t, cp_seid)) = self.inbound.remove(&seq) else {
                reply(
                    command,
                    ClientSendOutcome::Rejected {
                        error: format!("no request from the UPF with sequence {seq} is waiting"),
                    },
                );
                return Ok(());
            };
            let up = cp_seid.and_then(|c| self.sessions.get(&c).copied());
            let ies = merge(
                json!({"cause": action["cause"].as_str().unwrap_or("request_accepted")}),
                &extra,
            );
            let bytes = wire::encode(
                t + 1,
                wire::has_seid(t + 1).then_some(up.unwrap_or(0)),
                seq,
                &ies,
            )?;
            self.socket.send(&bytes).await?;
            reply(
                command,
                ClientSendOutcome::Sent {
                    bytes_sent: bytes.len(),
                },
            );
            return Ok(());
        }
        if self.pending.len() >= MAX_PENDING {
            reply(
                command,
                ClientSendOutcome::Rejected {
                    error: format!("{MAX_PENDING} requests are already waiting for the UPF"),
                },
            );
            return Ok(());
        }
        let (t, seid, ies, cp_seid) = match kind {
            "pfcp_associate" => (
                5,
                None,
                merge(
                    json!({"node_id": self.node_id, "recovery_time_stamp": self.recovery}),
                    &extra,
                ),
                None,
            ),
            "pfcp_heartbeat" => (1, None, json!({"recovery_time_stamp": self.recovery}), None),
            "pfcp_release_association" => (9, None, json!({"node_id": self.node_id}), None),
            "pfcp_establish_session" => {
                let cp = self.next_cp_seid;
                self.next_cp_seid += 1;
                self.sessions.insert(cp, 0);
                (
                    50,
                    Some(0),
                    merge(
                        json!({"node_id": self.node_id, "f_seid": self.fseid(cp)}),
                        &extra,
                    ),
                    Some(cp),
                )
            }
            "pfcp_modify_session" | "pfcp_delete_session" => {
                let cp = actions::cp_seid(action)?;
                let Some(&up) = self.sessions.get(&cp).filter(|up| **up != 0) else {
                    reply(
                        command,
                        ClientSendOutcome::Rejected {
                            error: format!("no established session with cp_seid {cp}"),
                        },
                    );
                    return Ok(());
                };
                let t = if kind == "pfcp_modify_session" {
                    52
                } else {
                    54
                };
                (
                    t,
                    Some(up),
                    if t == 52 { extra } else { Value::Null },
                    Some(cp),
                )
            }
            other => anyhow::bail!("unexpected action {other}"),
        };
        self.seq = (self.seq + 1) & 0x00ff_ffff;
        let bytes = wire::encode(t, seid, self.seq, &ies)?;
        self.socket
            .send(&bytes)
            .await
            .with_context(|| format!("sending to {}", self.upf))?;
        self.pending.insert(
            self.seq,
            Pending {
                request: t,
                bytes,
                sent: 1,
                deadline: Instant::now() + T1,
                depth,
                cp_seid,
                command,
            },
        );
        Ok(())
    }

    async fn retransmit(&mut self) {
        let now = Instant::now();
        let due: Vec<u32> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(s, _)| *s)
            .collect();
        for seq in due {
            let Some(p) = self.pending.get_mut(&seq) else {
                continue;
            };
            if p.sent > N1 {
                let p = self.pending.remove(&seq).expect("present");
                let data = json!({"request": wire::message_name(p.request), "sequence": seq});
                reply(
                    p.command,
                    ClientSendOutcome::Executed {
                        detail: json!({"timeout": data}).to_string(),
                    },
                );
                self.emit(&actions::TIMEOUT_EVENT, data, p.depth);
                continue;
            }
            p.sent += 1;
            p.deadline = now + T1;
            let bytes = p.bytes.clone();
            let _ = self.socket.send(&bytes).await;
        }
    }

    async fn incoming(&mut self, datagram: &[u8]) {
        let msg = match wire::parse(datagram) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("PFCP client {} dropped a datagram: {e:#}", self.client);
                return;
            }
        };
        let t = msg.header.message_type;
        let seq = msg.header.sequence;
        if wire::is_request(t) {
            let cp_seid = msg.header.seid.filter(|s| *s != 0);
            if t == 1 {
                // Heartbeats are answered as any node would, with our recovery time.
                if let Ok(b) =
                    wire::encode(2, None, seq, &json!({"recovery_time_stamp": self.recovery}))
                {
                    let _ = self.socket.send(&b).await;
                }
                return;
            }
            self.inbound.insert(seq, (t, cp_seid));
            if self.inbound.len() > MAX_PENDING {
                if let Some(oldest) = self.inbound.keys().min().copied() {
                    self.inbound.remove(&oldest);
                }
            }
            let data = json!({"message": wire::message_name(t), "sequence": seq, "cp_seid": cp_seid, "ies": msg.ies});
            self.emit(&actions::REQUEST_EVENT, data, 0);
            return;
        }
        let Some(p) = self.pending.remove(&seq) else {
            tracing::debug!(
                "PFCP client {} ignored a {} with sequence {seq} it did not ask for",
                self.client,
                wire::message_name(t)
            );
            return;
        };
        if t == 11 {
            let data = json!({"request": wire::message_name(p.request), "message": "version_not_supported_response", "ies": {}});
            reply(
                p.command,
                ClientSendOutcome::Executed {
                    detail: data.to_string(),
                },
            );
            self.emit(&actions::RESPONSE_EVENT, data, p.depth);
            return;
        }
        let cause = msg
            .ies
            .get("cause")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut data = json!({"request": wire::message_name(p.request), "message": wire::message_name(t), "cause": cause, "ies": msg.ies});
        if let Some(cp) = p.cp_seid {
            data["cp_seid"] = json!(cp);
            let accepted = cause.as_deref() == Some("request_accepted");
            match p.request {
                50 if accepted => {
                    if let Some(up) = msg
                        .ies
                        .get("f_seid")
                        .and_then(|f| f.get("seid"))
                        .and_then(Value::as_u64)
                    {
                        self.sessions.insert(cp, up);
                    }
                }
                50 => {
                    self.sessions.remove(&cp);
                }
                54 if accepted => {
                    self.sessions.remove(&cp);
                }
                _ => {}
            }
            if let Some(up) = self.sessions.get(&cp).filter(|u| **u != 0) {
                data["up_seid"] = json!(up);
            }
        }
        reply(
            p.command,
            ClientSendOutcome::Executed {
                detail: data.to_string(),
            },
        );
        self.emit(&actions::RESPONSE_EVENT, data, p.depth);
    }
}

async fn run_turns(
    ctx: ConnectContext,
    mut events: mpsc::Receiver<(Event, u32)>,
    internal: mpsc::Sender<(Value, u32)>,
) {
    let protocol = PfcpClientProtocol;
    while let Some((event, depth)) = events.recv().await {
        if depth >= MAX_FOLLOWUP_DEPTH {
            tracing::warn!("PFCP client {} not asking the model about {}: {depth} turns deep decision=followup_depth", ctx.client_id, event.id());
            continue;
        }
        let instruction = ctx
            .state
            .get_instruction_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        let memory = ctx
            .state
            .get_memory_for_client(ctx.client_id)
            .await
            .unwrap_or_default();
        match call_llm_for_client(
            &ctx.llm_client,
            &ctx.state,
            ctx.client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &protocol,
            &ctx.status_tx,
        )
        .await
        {
            Ok(result) => {
                if let Some(memory) = result.memory_updates {
                    ctx.state.set_memory_for_client(ctx.client_id, memory).await;
                }
                for action in result.actions {
                    if internal.send((action, depth + 1)).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => Log::new(Some(&ctx.status_tx)).warn(format!("PFCP client handler: {e}")),
        }
    }
}
