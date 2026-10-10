//! WS-Discovery client: probes from an ephemeral UDP port (to the multicast group, or to
//! `remote_addr` for a directed probe) and collects every answer whose RelatesTo names the
//! probe until its wait ends, then reports them once. Optionally hears Hello and Bye on 3702.
pub mod actions;

use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event, EventType};
use crate::server::wsdiscovery::wire::{self, Kind, QName, Target, Version};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::WsDiscoveryClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const DEFAULT_LISTEN_ANNOUNCEMENTS: bool = false;
/// Probes and resolves collecting answers at once; the next is refused.
pub const MAX_PENDING: usize = 16;
/// How many model turns may follow from one another before the chain stops.
pub const MAX_FOLLOWUP_DEPTH: u32 = 8;
const TURN_QUEUE: usize = 64;

struct Pending {
    kind: Kind,
    asked: Value,
    deadline: Instant,
    found: Vec<Target>,
    responders: Vec<String>,
    depth: u32,
    command: Option<ClientCommand>,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let version = Version::parse(
        &params
            .map(|p| p.get_optional_string("version"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| actions::DEFAULT_VERSION.into()),
    )?;
    let listen = params
        .map(|p| p.get_optional_bool("listen_announcements"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_LISTEN_ANNOUNCEMENTS);
    let target = if ctx.remote_addr.trim().is_empty() {
        wire::group()
    } else {
        tokio::net::lookup_host(&ctx.remote_addr)
            .await
            .with_context(|| format!("cannot resolve {}", ctx.remote_addr))?
            .next()
            .with_context(|| format!("{} resolved to no address", ctx.remote_addr))?
    };
    let socket = Arc::new(
        UdpSocket::bind(if target.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?,
    );
    let local = socket.local_addr()?;
    let announcements = if listen {
        let s = crate::server::socket_helpers::create_reusable_udp_socket(SocketAddr::from((
            Ipv4Addr::UNSPECIFIED,
            wire::PORT,
        )))
        .await
        .context("cannot listen for announcements on UDP 3702")?;
        s.join_multicast_v4(wire::GROUP_V4, Ipv4Addr::UNSPECIFIED)
            .context("cannot join 239.255.255.250")?;
        Some(Arc::new(s))
    } else {
        None
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "WS-Discovery client {} probing {target} from {local}",
        ctx.client_id
    ));
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;

    let (datagram_tx, datagrams) = mpsc::channel::<(Vec<u8>, SocketAddr)>(256);
    for s in std::iter::once(socket.clone()).chain(announcements.clone()) {
        let tx = datagram_tx.clone();
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            while let Ok((n, from)) = s.recv_from(&mut buf).await {
                if tx.send((buf[..n].to_vec(), from)).await.is_err() {
                    return;
                }
            }
        });
        ctx.state.register_client_task(ctx.client_id, reader).await;
    }
    drop(datagram_tx);

    let (event_tx, event_rx) = mpsc::channel::<(Event, u32)>(TURN_QUEUE);
    let (internal_tx, internal) = mpsc::channel::<(Value, u32)>(64);
    let _ = event_tx.try_send((
        Event::new(
            &actions::READY_EVENT,
            json!({"target": target.to_string(), "version": version.name()}),
        ),
        0,
    ));
    let dispatcher = tokio::spawn(run_turns(ctx.clone(), event_rx, internal_tx));
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let mut session = Session {
        socket,
        target,
        version,
        pending: HashMap::new(),
        events: event_tx,
        client_id: ctx.client_id,
    };
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session
            .run(&session_ctx, datagrams, external, internal)
            .await;
        dispatcher_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx))
                    .warn(format!("WS-Discovery client ended: {e:#}"));
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

async fn run_turns(
    ctx: ConnectContext,
    mut events: mpsc::Receiver<(Event, u32)>,
    internal: mpsc::Sender<(Value, u32)>,
) {
    let protocol = WsDiscoveryClientProtocol;
    while let Some((event, depth)) = events.recv().await {
        if depth >= MAX_FOLLOWUP_DEPTH {
            tracing::warn!("WS-Discovery client {} not asking the model about {}: {depth} turns deep decision=followup_depth", ctx.client_id, event.id());
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
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("WS-Discovery client handler: {e}"))
            }
        }
    }
}

struct Session {
    socket: Arc<UdpSocket>,
    target: SocketAddr,
    version: Version,
    pending: HashMap<String, Pending>,
    events: mpsc::Sender<(Event, u32)>,
    client_id: crate::state::ClientId,
}

fn reply(command: Option<ClientCommand>, outcome: ClientSendOutcome) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, Ok(outcome));
    }
}

impl Session {
    fn emit(&self, t: &'static EventType, data: Value, depth: u32) {
        if self.events.try_send((Event::new(t, data), depth)).is_err() {
            tracing::warn!("WS-Discovery client {} dropped an event: the model is {TURN_QUEUE} events behind decision=turn_queue_full", self.client_id);
        }
    }

    async fn run(
        &mut self,
        ctx: &ConnectContext,
        mut datagrams: mpsc::Receiver<(Vec<u8>, SocketAddr)>,
        mut external: mpsc::Receiver<ClientCommand>,
        mut internal: mpsc::Receiver<(Value, u32)>,
    ) -> Result<()> {
        loop {
            let deadline = self.pending.values().map(|p| p.deadline).min();
            let (action, depth, command) = tokio::select! {
                d = datagrams.recv() => {
                    let (bytes, from) = d.context("the sockets closed")?;
                    self.incoming(&bytes, from);
                    continue;
                }
                _ = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.expire();
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
            if command.is_some() {
                ctx.state
                    .record_access_log(
                        AccessLogOwner::Client(ctx.client_id.as_u32()),
                        "WS-Discovery",
                        None,
                        "injected_action",
                        action.clone(),
                        vec![],
                    )
                    .await;
            }
            match WsDiscoveryClientProtocol.execute_action(action.clone()) {
                Ok(ClientActionResult::Disconnect) => {
                    reply(command, ClientSendOutcome::Disconnected);
                    return Ok(());
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        "WS-Discovery client {} refused an action: {e}",
                        self.client_id
                    );
                    reply(
                        command,
                        ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        },
                    );
                    continue;
                }
            }
            if self.pending.len() >= MAX_PENDING {
                reply(
                    command,
                    ClientSendOutcome::Rejected {
                        error: format!("{MAX_PENDING} probes are already collecting answers"),
                    },
                );
                continue;
            }
            let id = wire::new_message_id();
            let wait = Duration::from_millis(actions::wait_ms(&action)?);
            let (kind, xml, asked) = if action["type"] == "wsd_probe" {
                let types: Vec<QName> = actions::strings(&action, "types")?
                    .iter()
                    .map(|s| QName::parse(s))
                    .collect::<Result<_>>()?;
                let scopes = actions::strings(&action, "scopes")?;
                let xml = wire::probe(
                    self.version,
                    &id,
                    &types,
                    &scopes,
                    action["match_by"].as_str(),
                );
                (
                    Kind::ProbeMatches,
                    xml,
                    json!({"types": types.iter().map(QName::clark).collect::<Vec<_>>(), "scopes": scopes}),
                )
            } else {
                let endpoint = action["endpoint_reference"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                (
                    Kind::ResolveMatches,
                    wire::resolve(self.version, &id, &endpoint),
                    json!({"endpoint_reference": endpoint}),
                )
            };
            self.socket
                .send_to(xml.as_bytes(), self.target)
                .await
                .with_context(|| format!("sending to {}", self.target))?;
            self.pending.insert(
                id,
                Pending {
                    kind,
                    asked,
                    deadline: Instant::now() + wait,
                    found: vec![],
                    responders: vec![],
                    depth,
                    command,
                },
            );
        }
    }

    fn incoming(&mut self, bytes: &[u8], from: SocketAddr) {
        let m = match wire::parse(bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(
                    "WS-Discovery client {} dropped a datagram from {from}: {e:#}",
                    self.client_id
                );
                return;
            }
        };
        match m.kind {
            Kind::ProbeMatches | Kind::ResolveMatches => {
                let Some(p) = m.relates_to.as_ref().and_then(|r| self.pending.get_mut(r)) else {
                    tracing::debug!("WS-Discovery client {} dropped matches from {from} that answer no probe of ours", self.client_id);
                    return;
                };
                if p.kind != m.kind {
                    return;
                }
                for t in m.targets {
                    match p.found.iter_mut().find(|f| f.endpoint == t.endpoint) {
                        Some(existing) => *existing = t,
                        None => p.found.push(t),
                    }
                }
                let who = from.to_string();
                if !p.responders.contains(&who) {
                    p.responders.push(who);
                }
            }
            Kind::Hello | Kind::Bye => {
                let data = json!({"message": m.kind.name(), "service": m.targets.first().map(wire::target_json), "source": from.to_string()});
                self.emit(&actions::ANNOUNCEMENT_EVENT, data, 0);
            }
            Kind::Probe | Kind::Resolve => {} // other clients' questions, overheard on the group
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for id in due {
            let Some(p) = self.pending.remove(&id) else {
                continue;
            };
            let mut data = p.asked;
            data["matches"] = Value::Array(p.found.iter().map(wire::target_json).collect());
            data["count"] = json!(p.found.len());
            data["responders"] = json!(p.responders);
            reply(
                p.command,
                ClientSendOutcome::Executed {
                    detail: data.to_string(),
                },
            );
            let t = if p.kind == Kind::ProbeMatches {
                &*actions::PROBE_MATCHES_EVENT
            } else {
                &*actions::RESOLVE_MATCHES_EVENT
            };
            self.emit(t, data, p.depth);
        }
    }
}
