//! BFD speaker (RFC 5880 Asynchronous mode over UDP: RFC 5881 single-hop, RFC 5883
//! multihop), in the passive role: a peer that starts sending is a session request for the
//! model; an accepted session is run in Rust (`runner.rs`) and every state change is the
//! model's to react to. **Deliberately silent**: BFD has no refusal, so a peer that is not
//! accepted — or a backend failure — is never answered and its session stays Down.
pub mod actions;
pub mod packet;
pub mod runner;
pub mod session;
pub mod ttl;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use actions::BfdProtocol;
use anyhow::{Context, Result};
use packet::{AuthConfig, ControlPacket};
use runner::{Input, Link, Note};
use serde_json::{json, Value};
use session::{Session, Timers};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// A session that is Down with nothing heard from its peer for this long ends.
pub const SESSION_IDLE: Duration = Duration::from_secs(60);
/// A peer the model did not accept is not asked about again for this long (a Down peer
/// sends once a second, and each packet would otherwise be a model call).
pub const DECLINE_HOLD: Duration = Duration::from_secs(30);
/// How many model turns may follow from one another on one session.
pub const MAX_FOLLOWUP_DEPTH: u32 = 8;
const SESSION_INPUTS: usize = 64;
const NOTES: usize = 64;

struct Entry {
    peer: IpAddr,
    inputs: mpsc::Sender<Input>,
    conn: ConnectionId,
}

#[derive(Default)]
struct Table {
    by_discr: HashMap<u32, Entry>,
    by_peer: HashMap<IpAddr, u32>,
    asking: HashSet<IpAddr>,
    declined: HashMap<IpAddr, Instant>,
}

impl Table {
    fn new_discriminator(&self) -> u32 {
        loop {
            let d: u32 = rand::random();
            if d != 0 && !self.by_discr.contains_key(&d) {
                return d;
            }
        }
    }
}

#[derive(Clone)]
struct Shared {
    ctx: SpawnContext,
    table: Arc<Mutex<Table>>,
    tx: Arc<UdpSocket>,
    port: u16,
    multihop: bool,
    max_sessions: usize,
    timers: Timers,
    auth: Option<AuthConfig>,
}

fn lock(t: &Mutex<Table>) -> std::sync::MutexGuard<'_, Table> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let port = ctx.port.unwrap_or_else(|| ctx.legacy_listen_addr().port());
    let host = ctx.host.clone().unwrap_or_else(|| "127.0.0.1".into());
    let bind = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("cannot resolve host {host}"))?
        .next()
        .with_context(|| format!("host {host} resolved to no address"))?;
    let multihop = params
        .map(|p| p.get_optional_bool("multihop"))
        .transpose()?
        .flatten()
        .unwrap_or(port == packet::MULTIHOP_PORT);
    let max_sessions = params
        .map(|p| p.get_optional_u64("max_sessions"))
        .transpose()?
        .flatten()
        .map(|n| n as usize)
        .unwrap_or(actions::MAX_SESSIONS);
    let timers = actions::timers_from(params)?;
    let auth = actions::auth_from(params)?;
    // SO_REUSEADDR: a routing daemon on the same host binds the BFD port on the wildcard
    // address, and a socket on a specific address still receives what is sent to it.
    let socket = crate::server::socket_helpers::create_reusable_udp_socket(bind)
        .await
        .with_context(|| format!("BFD failed to bind {bind}"))?;
    ttl::enable(&socket).context("asking for the TTL of received packets")?;
    let local = socket.local_addr()?;
    let tx = Arc::new(runner::tx_socket(local.ip()).await?);
    let log = Log::new(Some(&ctx.status_tx));
    log.info(format!(
        "BFD listening on {local} ({}), sending from {}",
        if multihop {
            "multihop, RFC 5883"
        } else {
            "single-hop, RFC 5881: TTL 255 required"
        },
        tx.local_addr()?
    ));
    let shared = Shared {
        ctx: ctx.clone(),
        table: Default::default(),
        tx,
        port: local.port(),
        multihop,
        max_sessions,
        timers,
        auth,
    };
    let task = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let (n, from, ttl) = match ttl::recv(&socket, &mut buf).await {
                Ok(r) => r,
                Err(e) => {
                    Log::new(Some(&shared.ctx.status_tx)).error(format!("BFD receive error: {e}"));
                    break;
                }
            };
            if let Some(asking) = receive(&shared, &buf[..n], from, ttl).await {
                shared
                    .ctx
                    .state
                    .register_server_task(shared.ctx.server_id, asking)
                    .await;
            }
        }
    });
    ctx.state.register_server_task(ctx.server_id, task).await;
    Ok(local)
}

/// Route one datagram. A new peer starts a model turn, whose task is returned to register.
async fn receive(
    shared: &Shared,
    bytes: &[u8],
    from: SocketAddr,
    ttl: Option<u8>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !shared.multihop && ttl.is_some_and(|t| t != 255) {
        tracing::warn!(
            "BFD dropped a packet from {from} with TTL {} (single-hop requires 255) decision=dropped_ttl",
            ttl.unwrap_or_default()
        );
        return None;
    }
    let packet = match packet::decode(bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("BFD dropped a datagram from {from}: {e:#}");
            return None;
        }
    };
    let peer = from.ip();
    let input = || Input::Packet {
        bytes: bytes.to_vec(),
        packet: packet.clone(),
    };
    let target = {
        let mut table = lock(&shared.table);
        let found = if packet.your_discriminator != 0 {
            match table.by_discr.get(&packet.your_discriminator) {
                Some(e) if e.peer == peer => Some((e.inputs.clone(), e.conn)),
                Some(_) => {
                    tracing::warn!(
                        "BFD dropped a packet from {from} naming another peer's session"
                    );
                    return None;
                }
                None => {
                    tracing::debug!(
                        "BFD dropped a packet from {from} for unknown discriminator {}",
                        packet.your_discriminator
                    );
                    return None;
                }
            }
        } else {
            table
                .by_peer
                .get(&peer)
                .and_then(|d| table.by_discr.get(d))
                .map(|e| (e.inputs.clone(), e.conn))
        };
        match found {
            Some(t) => t,
            None => {
                let now = Instant::now();
                table.declined.retain(|_, until| *until > now);
                if table.declined.contains_key(&peer) || table.asking.contains(&peer) {
                    return None;
                }
                // The key is checked before the model hears of the peer: one that cannot
                // authenticate is not a session request.
                if let Err(e) = packet::verify(bytes, &packet, shared.auth.as_ref()) {
                    tracing::warn!("BFD dropped a packet from {from} decision=dropped_auth: {e:#}");
                    return None;
                }
                if table.by_discr.len() + table.asking.len() >= shared.max_sessions {
                    tracing::warn!(
                        "BFD ignored {from}: {} sessions already decision=max_sessions",
                        shared.max_sessions
                    );
                    return None;
                }
                table.asking.insert(peer);
                drop(table);
                let (shared, bytes) = (shared.clone(), bytes.to_vec());
                let handle = tokio::spawn(async move { ask(shared, peer, bytes, packet).await });
                return Some(handle);
            }
        }
    };
    if target.0.try_send(input()).is_err() {
        tracing::warn!("BFD dropped a packet from {from}: its session is {SESSION_INPUTS} behind");
    }
    shared
        .ctx
        .state
        .update_connection_stats(
            shared.ctx.server_id,
            target.1,
            Some(bytes.len() as u64),
            None,
            Some(1),
            None,
        )
        .await;
    None
}

/// A new peer: ask the model whether to bring a session up with it.
async fn ask(shared: Shared, peer: IpAddr, bytes: Vec<u8>, packet: ControlPacket) {
    let ctx = &shared.ctx;
    let event = Event::new(
        &actions::SESSION_REQUEST_EVENT,
        json!({
            "peer": peer.to_string(),
            "peer_discriminator": packet.my_discriminator,
            "peer_state": packet.state.name(),
            "desired_min_tx_ms": packet.desired_min_tx_us / 1000,
            "required_min_rx_ms": packet.required_min_rx_us / 1000,
            "detect_mult": packet.detect_mult,
            "authentication": packet.auth.as_ref().map(|a| a.kind.name()),
            "multihop": shared.multihop,
        }),
    );
    let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
    let summary = format!("BFD session request from {peer}");
    let log = Log::new(Some(&ctx.status_tx));
    let accepted = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &BfdProtocol,
    )
    .await
    {
        Ok(execution) => execution.protocol_results.iter().find_map(|r| match r {
            ActionResult::Custom { name, data } if name == actions::ACCEPT => Some(data.clone()),
            _ => None,
        }),
        Err(e) => {
            log.error(format!(
                "{summary} decision=fail_closed_llm_error (no session: BFD has no refusal): {e}"
            ));
            None
        }
    };
    let Some(answer) = accepted else {
        log.info(format!(
            "{summary} decision=model_silent (not accepted; ignored for {DECLINE_HOLD:?})"
        ));
        let mut table = lock(&shared.table);
        table.asking.remove(&peer);
        table.declined.insert(peer, Instant::now() + DECLINE_HOLD);
        return;
    };
    let timers = match Timers::from_json(&answer, shared.timers) {
        Ok(t) => t,
        Err(e) => {
            log.warn(format!(
                "{summary} decision=fail_closed_action_error: {e:#}"
            ));
            lock(&shared.table).asking.remove(&peer);
            return;
        }
    };
    start_session(shared, peer, id, timers, bytes, packet).await;
}

async fn start_session(
    shared: Shared,
    peer: IpAddr,
    id: ConnectionId,
    timers: Timers,
    bytes: Vec<u8>,
    packet: ControlPacket,
) {
    let ctx = shared.ctx.clone();
    let (inputs, inputs_rx) = mpsc::channel(SESSION_INPUTS);
    let (notes_tx, notes) = mpsc::channel(NOTES);
    let discr = {
        let mut table = lock(&shared.table);
        table.asking.remove(&peer);
        let discr = table.new_discriminator();
        table.by_discr.insert(
            discr,
            Entry {
                peer,
                inputs: inputs.clone(),
                conn: id,
            },
        );
        table.by_peer.insert(peer, discr);
        discr
    };
    let remote = SocketAddr::new(peer, shared.port);
    record(&ctx, id, remote, shared.tx.local_addr().ok()).await;
    let peer_rx =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "BFD session {discr} with {peer} accepted decision=model_answered (tx {} ms, rx {} ms, x{})",
        timers.desired_min_tx_us / 1000,
        timers.required_min_rx_us / 1000,
        timers.detect_mult
    ));
    let session = Session::new(discr, timers, shared.auth.clone(), true);
    let _ = inputs.try_send(Input::Packet { bytes, packet });
    let link = Link {
        socket: shared.tx.clone(),
        dest: remote,
        label: format!("BFD session {discr} with {peer}"),
    };
    let run = tokio::spawn(runner::run(
        session,
        link,
        inputs_rx,
        notes_tx,
        Some(SESSION_IDLE),
    ));
    ctx.state.register_server_task(ctx.server_id, run).await;
    let watch = tokio::spawn(watch(shared, peer, discr, id, inputs, notes, peer_rx));
    ctx.state.register_server_task(ctx.server_id, watch).await;
}

async fn record(
    ctx: &SpawnContext,
    id: ConnectionId,
    remote: SocketAddr,
    local: Option<SocketAddr>,
) {
    use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
    let now = crate::utils::clock::Instant::now();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id,
                remote_addr: remote,
                local_addr: local.unwrap_or(remote),
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: now,
                status: ConnectionStatus::Active,
                status_changed_at: now,
                protocol_info: ProtocolConnectionInfo::new(json!({"state": "Down"})),
            },
        )
        .await;
}

/// One session's model side: state changes become events, answers become session actions,
/// and the dashboard's actions for this peer are forwarded in.
async fn watch(
    shared: Shared,
    peer: IpAddr,
    discr: u32,
    id: ConnectionId,
    inputs: mpsc::Sender<Input>,
    mut notes: mpsc::Receiver<Note>,
    mut peer_rx: mpsc::Receiver<crate::state::client_handles::ClientCommand>,
) {
    let ctx = &shared.ctx;
    loop {
        tokio::select! {
            c = peer_rx.recv() => {
                if let Some(command) = c {
                    let action = command.action.clone();
                    let _ = inputs.send(Input::Action { action, depth: 0, command: Some(command) }).await;
                }
            }
            note = notes.recv() => match note {
                None | Some(Note::Ended { .. }) => break,
                Some(Note::Changed { previous, snapshot, depth }) => {
                    let data = runner::state_event(&peer.to_string(), previous, &snapshot);
                    Log::new(Some(&ctx.status_tx)).info(format!(
                        "BFD session {discr} with {peer}: {} -> {} ({})",
                        previous.name(), data["state"].as_str().unwrap_or_default(), data["diag"].as_str().unwrap_or_default()
                    ));
                    if depth >= MAX_FOLLOWUP_DEPTH {
                        tracing::warn!("BFD session {discr}: not asking the model about this change, {depth} turns deep decision=followup_depth");
                        continue;
                    }
                    for action in react(ctx, id, data).await {
                        let _ = inputs.send(Input::Action { action, depth: depth + 1, command: None }).await;
                    }
                }
            },
        }
    }
    {
        let mut table = lock(&shared.table);
        table.by_discr.remove(&discr);
        if table.by_peer.get(&peer) == Some(&discr) {
            table.by_peer.remove(&peer);
        }
    }
    ctx.state
        .close_connection_on_server(ctx.server_id, id)
        .await;
}

async fn react(ctx: &SpawnContext, id: ConnectionId, data: Value) -> Vec<Value> {
    let event = Event::new(&actions::STATE_EVENT, data);
    match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &BfdProtocol,
    )
    .await
    {
        Ok(execution) => execution
            .protocol_results
            .iter()
            .filter_map(|r| match r {
                ActionResult::Custom { name, data } if name != actions::ACCEPT => {
                    Some(data.clone())
                }
                _ => None,
            })
            .collect(),
        Err(e) => {
            // Nothing to fail closed to: the session keeps running as it is.
            tracing::warn!("BFD state change not answered decision=fail_closed_llm_error: {e}");
            vec![]
        }
    }
}
