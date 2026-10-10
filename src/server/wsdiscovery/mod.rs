//! WS-Discovery target service: answers Probe and Resolve on UDP 3702, multicast and directed.
//! The model decides which of its services match; matches go back unicast to the asker, and
//! Hello/Bye announcements go to the group. **Deliberately silent:** WS-Discovery has no
//! negative reply, so a probe the model does not answer — or a backend failure — sends
//! nothing, and the log says which it was.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use actions::{RequestContext, WsDiscoveryProtocol};
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;
use wire::Kind;

pub const DEFAULT_JOIN_MULTICAST: bool = true;
/// MessageIDs of messages this server sent, so its own multicast loopback is ignored.
const RECENT_SENT: usize = 64;

/// The AppSequence InstanceId: fixed for the life of the process, as the specification asks
/// for an instance that has not restarted.
pub fn instance_id() -> u64 {
    static ID: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        crate::utils::clock::SystemTime::now()
            .duration_since(crate::utils::clock::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(1)
    })
}

#[derive(Clone)]
struct Config {
    announce_target: SocketAddr,
    numbers: Arc<AtomicU64>,
    sent: Arc<Mutex<VecDeque<String>>>,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let join = params
        .map(|p| p.get_optional_bool("join_multicast"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_JOIN_MULTICAST);
    let interface = params
        .map(|p| p.get_optional_string("multicast_interface"))
        .transpose()?
        .flatten();
    let announce_target = match params
        .map(|p| p.get_optional_string("announce_target"))
        .transpose()?
        .flatten()
    {
        Some(raw) => raw
            .parse::<SocketAddr>()
            .with_context(|| format!("announce_target {raw:?} is not ip:port"))?,
        None => wire::group(),
    };
    // SO_REUSEADDR: discovery daemons share 3702 on a host (wsdd and python WSDiscovery do
    // the same), and every one of them receives each multicast probe.
    // The operator's host, defaulting to loopback as every server does. Multicast probes from
    // the network reach only a socket bound to 0.0.0.0 (or the group), so a discovery
    // responder meant to be found on the LAN is started with host 0.0.0.0.
    let port = ctx.port.unwrap_or_else(|| ctx.legacy_listen_addr().port());
    let host = ctx.host.clone().unwrap_or_else(|| "127.0.0.1".into());
    let bind = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve host {host}"))?
        .next()
        .with_context(|| format!("host {host} resolved to no address"))?;
    let socket = Arc::new(
        crate::server::socket_helpers::create_reusable_udp_socket(bind)
            .await
            .with_context(|| format!("WS-Discovery failed to bind {bind}"))?,
    );
    let local = socket.local_addr()?;
    let log = Log::new(Some(&ctx.status_tx));
    if join && local.ip().is_loopback() {
        log.warn(format!(
            "WS-Discovery is bound to {local}: multicast probes from other interfaces will not arrive; start it with host 0.0.0.0 to be found on the network"
        ));
    }
    if join && local.is_ipv4() {
        let iface = match interface.as_deref() {
            Some(raw) => raw
                .parse::<Ipv4Addr>()
                .with_context(|| format!("multicast_interface {raw:?} is not an IPv4 address"))?,
            None => Ipv4Addr::UNSPECIFIED,
        };
        match socket.join_multicast_v4(wire::GROUP_V4, iface) {
            Ok(()) => log.info(format!("WS-Discovery joined {} on {iface}", wire::GROUP_V4)),
            // Best effort, as SSDP: a directed probe is answered whether or not the join worked.
            Err(e) => log.warn(format!("WS-Discovery could not join {} ({e}); directed probes to {local} are still answered", wire::GROUP_V4)),
        }
    }
    log.info(format!("WS-Discovery target service listening on {local}"));
    let config = Config {
        announce_target,
        numbers: Arc::new(AtomicU64::new(1)),
        sent: Default::default(),
    };
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65_536];
        loop {
            let (n, from) = match socket.recv_from(&mut buf).await {
                Ok(pair) => pair,
                Err(e) => {
                    Log::new(Some(&ctx.status_tx))
                        .error(format!("WS-Discovery receive error: {e}"));
                    break;
                }
            };
            let message = match wire::parse(&buf[..n]) {
                Ok(m) => m,
                Err(e) => {
                    Log::new(Some(&ctx.status_tx)).warn(format!(
                        "WS-Discovery dropped a datagram from {from}: {e:#}"
                    ));
                    continue;
                }
            };
            if config
                .sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&message.message_id)
            {
                continue; // our own announcement, looped back by the group
            }
            if matches!(message.kind, Kind::ProbeMatches | Kind::ResolveMatches) {
                continue; // answers to someone else's probe, overheard on the group
            }
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            record(&ctx, id, local, from, n).await;
            let (child, socket, config) = (ctx.clone(), socket.clone(), config.clone());
            let handle =
                tokio::spawn(async move { answer(child, socket, config, message, from, id).await });
            ctx.state.register_server_task(ctx.server_id, handle).await;
        }
    });
    state.register_server_task(server_id, task).await;
    Ok(local)
}

async fn record(
    ctx: &SpawnContext,
    id: ConnectionId,
    local: SocketAddr,
    from: SocketAddr,
    n: usize,
) {
    use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
    let now = crate::utils::clock::Instant::now();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id,
                remote_addr: from,
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
}

async fn answer(
    ctx: SpawnContext,
    socket: Arc<UdpSocket>,
    config: Config,
    m: wire::Message,
    from: SocketAddr,
    id: ConnectionId,
) {
    let source = from.to_string();
    let event = match m.kind {
        Kind::Probe => Event::new(
            &actions::PROBE_EVENT,
            json!({
                "version": m.version.name(),
                "message_id": crate::utils::sanitize::line_field(&m.message_id),
                "types": m.types.iter().map(|q| crate::utils::sanitize::line_field(&q.clark())).collect::<Vec<_>>(),
                "scopes": m.scopes.iter().map(|s| crate::utils::sanitize::line_field(s)).collect::<Vec<_>>(),
                "match_by": m.match_by.as_deref().map(crate::utils::sanitize::line_field),
                "source": source,
            }),
        ),
        Kind::Resolve => Event::new(
            &actions::RESOLVE_EVENT,
            json!({
                "version": m.version.name(),
                "endpoint_reference": m.endpoint.as_deref().map(crate::utils::sanitize::line_field),
                "source": source,
            }),
        ),
        Kind::Hello | Kind::Bye => Event::new(
            &actions::ANNOUNCEMENT_EVENT,
            json!({
                "message": m.kind.name(),
                "service": m.targets.first().map(wire::target_json),
                "source": source,
            }),
        ),
        Kind::ProbeMatches | Kind::ResolveMatches => return,
    };
    let protocol = WsDiscoveryProtocol::for_request(RequestContext {
        version: m.version,
        message_id: m.message_id.clone(),
        instance_id: instance_id(),
        numbers: config.numbers.clone(),
    });
    let log = Log::new(Some(&ctx.status_tx));
    let summary = format!("WS-Discovery {} from {from}", m.kind.name());
    let execution = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &protocol,
    )
    .await
    {
        Ok(e) => e,
        Err(e) => {
            let category = if crate::utils::wire_failure::WireFailure::classify(&e).is_overloaded()
            {
                "overloaded"
            } else {
                "unavailable"
            };
            log.error(format!("{summary} decision=fail_closed_llm_error category={category} (nothing sent: WS-Discovery has no negative reply): {e}"));
            return;
        }
    };
    let mut sent = 0usize;
    for result in &execution.protocol_results {
        let items = match result {
            ActionResult::Multiple(all) => all.iter().collect::<Vec<_>>(),
            other => vec![other],
        };
        for item in items {
            let (bytes, to) = match item {
                ActionResult::Output(bytes) => (bytes.clone(), from),
                ActionResult::Custom { name, data } if name == actions::ANNOUNCE => {
                    let xml = data["xml"].as_str().unwrap_or_default().to_string();
                    if let Ok(parsed) = wire::parse(xml.as_bytes()) {
                        let mut recent = config.sent.lock().unwrap_or_else(|e| e.into_inner());
                        recent.push_back(parsed.message_id);
                        while recent.len() > RECENT_SENT {
                            recent.pop_front();
                        }
                    }
                    (xml.into_bytes(), config.announce_target)
                }
                _ => continue,
            };
            match socket.send_to(&bytes, to).await {
                Ok(n) => {
                    sent += 1;
                    ctx.state
                        .update_connection_stats(
                            ctx.server_id,
                            id,
                            None,
                            Some(n as u64),
                            None,
                            Some(1),
                        )
                        .await;
                }
                Err(e) => log.warn(format!("{summary}: sending to {to} failed: {e}")),
            }
        }
    }
    if sent == 0 {
        let reason = if execution.failures.is_empty() {
            "model_silent"
        } else {
            "fail_closed_action_error"
        };
        log.info(format!("{summary} decision={reason} (nothing sent)"));
    } else {
        log.info(format!("{summary} decision=model_answered messages={sent}"));
    }
}
