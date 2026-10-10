//! KNXnet/IP tunnelling client: one tunnel to a gateway, group writes and reads out, every
//! group telegram on the bus in as an event.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::knx::wire::{self, Apci, Telegram};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::KnxClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const ACK_TIMEOUT: Duration = Duration::from_secs(1);
pub const HEARTBEAT: Duration = Duration::from_secs(60);
pub const MAX_QUEUED: usize = 32;
pub const MAX_FOLLOWUP_DEPTH: usize = 8;

fn hpai_nat() -> Vec<u8> {
    vec![8, 0x01, 0, 0, 0, 0, 0, 0]
}

fn types(ctx: &ConnectContext) -> Result<HashMap<u16, String>> {
    let mut out = HashMap::new();
    if let Some(map) = ctx
        .startup_params
        .as_ref()
        .map(|p| p.get_optional_object("group_types"))
        .transpose()?
        .flatten()
    {
        for (ga, dpt) in map {
            let dpt = dpt.as_str().context("group_types values are DPT names")?;
            wire::dpt_main(dpt)?;
            out.insert(wire::parse_group(ga)?, dpt.to_string());
        }
    }
    Ok(out)
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let types = types(&ctx)?;
    let gateway: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await?
        .find(SocketAddr::is_ipv4)
        .context("the gateway needs an IPv4 address")?;
    let bind: SocketAddr = if gateway.ip().is_loopback() {
        "127.0.0.1:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let socket = UdpSocket::bind(bind).await?;
    let local = socket.local_addr()?;
    let mut body = hpai_nat();
    body.extend(hpai_nat());
    body.extend([4, 0x04, 0x02, 0x00]);
    socket
        .send_to(&wire::frame(wire::CONNECT_REQUEST, &body), gateway)
        .await?;
    let (channel, address) = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut buf = vec![0u8; wire::MAX_FRAME + 1];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await?;
            if from != gateway {
                continue;
            }
            let Ok((wire::CONNECT_RESPONSE, b)) = wire::parse_frame(&buf[..n]) else {
                continue;
            };
            ensure!(b.len() >= 2, "short CONNECT_RESPONSE");
            ensure!(
                b[1] == wire::E_NO_ERROR,
                "the gateway refused the tunnel (status {:#04x})",
                b[1]
            );
            let crd = b.get(10..14).context("CONNECT_RESPONSE without a CRD")?;
            return Ok::<_, anyhow::Error>((b[0], u16::from_be_bytes([crd[2], crd[3]])));
        }
    })
    .await
    .context("no CONNECT_RESPONSE from the gateway")??;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    event_tx.try_send((
        Event::new(
            &actions::CONNECTED_EVENT,
            json!({"remote_addr": gateway.to_string(), "individual_address": wire::format_individual(address)}),
        ),
        0,
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            let instruction = events_ctx
                .state
                .get_instruction_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = events_ctx
                .state
                .get_memory_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &KnxClientProtocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("KNX client handler: {e}"))
                }
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let mut s = Session {
            ctx: &session_ctx,
            socket,
            gateway,
            channel,
            address,
            types,
            seq_out: 0,
            seq_in: 0,
            pending: None,
            queue: VecDeque::new(),
            events: event_tx,
        };
        let result = s.run(external, internal_rx).await;
        let _ = s
            .socket
            .send_to(&disconnect_request(channel), gateway)
            .await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("KNX client ended: {e:#}"));
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

fn disconnect_request(channel: u8) -> Vec<u8> {
    let mut body = vec![channel, 0];
    body.extend(hpai_nat());
    wire::frame(wire::DISCONNECT_REQUEST, &body)
}

struct Pending {
    seq: u8,
    bytes: Vec<u8>,
    deadline: tokio::time::Instant,
    retried: bool,
    caller: Option<ClientCommand>,
}

struct Session<'a> {
    ctx: &'a ConnectContext,
    socket: UdpSocket,
    gateway: SocketAddr,
    channel: u8,
    address: u16,
    types: HashMap<u16, String>,
    seq_out: u8,
    seq_in: u8,
    pending: Option<Pending>,
    queue: VecDeque<(Telegram, Option<ClientCommand>)>,
    events: mpsc::Sender<(Event, usize)>,
}

fn reply(caller: Option<ClientCommand>, outcome: ClientSendOutcome) {
    if let Some(c) = caller {
        crate::client::command_support::reply(c, Ok(outcome));
    }
}

impl Session<'_> {
    async fn run(
        &mut self,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<(Value, usize)>,
    ) -> Result<()> {
        let log = Log::new(Some(&self.ctx.status_tx));
        let mut buf = vec![0u8; wire::MAX_FRAME + 1];
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        loop {
            let deadline = self
                .pending
                .as_ref()
                .map(|p| p.deadline)
                .unwrap_or_else(|| tokio::time::Instant::now() + HEARTBEAT * 10);
            let (action, depth, caller) = tokio::select! {
                r = self.socket.recv_from(&mut buf) => {
                    let (n, from) = r?;
                    if from == self.gateway {
                        let frame = buf[..n].to_vec();
                        if !self.incoming(&frame).await? {
                            return Ok(());
                        }
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(deadline), if self.pending.is_some() => {
                    let mut p = self.pending.take().expect("pending");
                    ensure!(!p.retried, "the gateway did not acknowledge request {} twice", p.seq);
                    self.socket.send_to(&p.bytes, self.gateway).await?;
                    p.retried = true;
                    p.deadline = tokio::time::Instant::now() + ACK_TIMEOUT;
                    self.pending = Some(p);
                    continue;
                }
                _ = heartbeat.tick() => {
                    let mut body = vec![self.channel, 0];
                    body.extend(hpai_nat());
                    self.socket.send_to(&wire::frame(wire::CONNECTIONSTATE_REQUEST, &body), self.gateway).await?;
                    continue;
                }
                c = external.recv() => match c {
                    Some(c) => (c.action.clone(), 0, Some(c)),
                    None => return Ok(()),
                },
                a = internal.recv() => match a {
                    Some((a, d)) => (a, d, None),
                    None => return Ok(()),
                },
            };
            match KnxClientProtocol.execute_action(action.clone()) {
                Ok(ClientActionResult::Disconnect) => {
                    reply(caller, ClientSendOutcome::Disconnected);
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    reply(
                        caller,
                        ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                    );
                    continue;
                }
            }
            if depth > MAX_FOLLOWUP_DEPTH {
                log.warn(format!(
                    "KNX client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
                ));
                continue;
            }
            let telegram = match self.telegram(&action) {
                Ok(t) => t,
                Err(e) => {
                    log.warn(format!("KNX action refused: {e:#}"));
                    reply(
                        caller,
                        ClientSendOutcome::Rejected {
                            error: format!("{e:#}"),
                        },
                    );
                    continue;
                }
            };
            if caller.is_some() {
                self.ctx
                    .state
                    .record_access_log(
                        AccessLogOwner::Client(self.ctx.client_id.as_u32()),
                        "KNX/IP",
                        None,
                        "injected_action",
                        action.clone(),
                        vec![],
                    )
                    .await;
            }
            if self.queue.len() >= MAX_QUEUED {
                reply(
                    caller,
                    ClientSendOutcome::Rejected {
                        error: "too many telegrams queued".into(),
                    },
                );
                continue;
            }
            self.queue.push_back((telegram, caller));
            self.send_next().await?;
        }
    }

    fn telegram(&self, action: &Value) -> Result<Telegram> {
        let ga = wire::parse_group(action["group_address"].as_str().unwrap_or_default())?;
        let (apci, data) = if action["type"] == "knx_group_read" {
            (Apci::Read, wire::Data::Small(0))
        } else {
            let dpt = action["dpt"]
                .as_str()
                .or(self.types.get(&ga).map(String::as_str))
                .context("dpt required: the address has no configured type")?;
            (Apci::Write, wire::encode(dpt, &action["value"])?)
        };
        Ok(Telegram {
            message_code: wire::L_DATA_REQ,
            source: self.address,
            destination: ga,
            apci,
            data,
        })
    }

    /// Send the next queued telegram if nothing awaits an ack.
    async fn send_next(&mut self) -> Result<()> {
        if self.pending.is_some() {
            return Ok(());
        }
        let Some((t, caller)) = self.queue.pop_front() else {
            return Ok(());
        };
        let seq = self.seq_out;
        self.seq_out = self.seq_out.wrapping_add(1);
        let bytes = wire::tunnelling(self.channel, seq, &wire::cemi(&t));
        self.socket.send_to(&bytes, self.gateway).await?;
        self.pending = Some(Pending {
            seq,
            bytes,
            deadline: tokio::time::Instant::now() + ACK_TIMEOUT,
            retried: false,
            caller,
        });
        Ok(())
    }

    /// Handle one frame from the gateway; `false` ends the session.
    async fn incoming(&mut self, frame: &[u8]) -> Result<bool> {
        let Ok((service, body)) = wire::parse_frame(frame) else {
            return Ok(true);
        };
        match service {
            wire::TUNNELLING_ACK if body.len() >= 4 && body[1] == self.channel => {
                if self.pending.as_ref().is_some_and(|p| p.seq == body[2]) {
                    let p = self.pending.take().expect("pending");
                    if body[3] == wire::E_NO_ERROR {
                        reply(
                            p.caller,
                            ClientSendOutcome::Sent {
                                bytes_sent: p.bytes.len(),
                            },
                        );
                    } else {
                        reply(
                            p.caller,
                            ClientSendOutcome::Rejected {
                                error: format!(
                                    "the gateway refused the telegram (status {:#04x})",
                                    body[3]
                                ),
                            },
                        );
                    }
                    self.send_next().await?;
                }
            }
            wire::TUNNELLING_REQUEST if body.len() >= 4 && body[1] == self.channel => {
                let seq = body[2];
                let fresh = seq == self.seq_in;
                if !fresh && seq != self.seq_in.wrapping_sub(1) {
                    return Ok(true);
                }
                self.socket
                    .send_to(
                        &wire::tunnelling_ack(self.channel, seq, wire::E_NO_ERROR),
                        self.gateway,
                    )
                    .await?;
                if !fresh {
                    return Ok(true);
                }
                self.seq_in = self.seq_in.wrapping_add(1);
                if let Ok(Some(t)) = wire::parse_cemi(&body[4..]) {
                    if t.message_code == wire::L_DATA_IND {
                        self.raise(&t)?;
                    }
                }
            }
            wire::CONNECTIONSTATE_RESPONSE if body.len() >= 2 => {
                ensure!(
                    body[1] == wire::E_NO_ERROR,
                    "the gateway lost the tunnel (status {:#04x})",
                    body[1]
                );
            }
            wire::DISCONNECT_REQUEST if body.first() == Some(&self.channel) => {
                self.socket
                    .send_to(
                        &wire::frame(wire::DISCONNECT_RESPONSE, &[self.channel, 0]),
                        self.gateway,
                    )
                    .await?;
                bail!("the gateway closed the tunnel");
            }
            _ => {}
        }
        Ok(true)
    }

    fn raise(&self, t: &Telegram) -> Result<()> {
        let dpt = self.types.get(&t.destination).cloned();
        let value = dpt
            .as_deref()
            .and_then(|d| wire::decode(d, &t.data).ok())
            .unwrap_or(Value::Null);
        let interpretations = if t.apci == Apci::Read {
            json!({})
        } else {
            wire::interpretations(&t.data)
        };
        self.events
            .try_send((
                Event::new(
                    &actions::TELEGRAM_EVENT,
                    json!({"kind": t.apci.name(), "source": wire::format_individual(t.source),
                           "destination": wire::format_group(t.destination), "dpt": dpt,
                           "value": value, "interpretations": interpretations}),
                ),
                0,
            ))
            .context("KNX event queue full; consumer stalled")
    }
}
