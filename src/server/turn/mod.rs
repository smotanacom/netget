//! TURN relay server (RFC 8656)
//!
//! Split of responsibilities:
//!
//! * **Rust owns the transport and the relay.** Every allocation binds a real
//!   UDP socket; peer traffic arriving on it is forwarded to the client as a
//!   Data indication (or a ChannelData frame when a channel is bound), and Send
//!   indications / ChannelData frames from the client are forwarded to the peer
//!   from that same socket. The data plane never calls the LLM: one model
//!   round-trip per relayed packet would be absurd, and permission policy has
//!   already been decided on the control plane.
//! * **The LLM owns policy.** Allocate, Refresh, CreatePermission and ChannelBind
//!   each raise an event; the model (or a script/static handler) decides whether
//!   to grant, for how long, and for which peers. Nothing is granted by default:
//!   a missing or unusable answer leaves the reserved socket unused and the
//!   client with no allocation.
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant};
use tracing::{debug, error, info, trace, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::executor::ExecutionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType};
use crate::server::TurnProtocol;
use crate::state::app_state::AppState;
use crate::state::ServerId;
use actions::{
    TURN_ALLOCATE_REQUEST_EVENT, TURN_CHANNEL_BIND_REQUEST_EVENT,
    TURN_CREATE_PERMISSION_REQUEST_EVENT, TURN_REFRESH_REQUEST_EVENT,
};

/// STUN magic cookie (RFC 8489 section 5)
const MAGIC_COOKIE: u32 = 0x2112_A442;

/// Largest datagram accepted on either the TURN socket or a relay socket.
const RELAY_MTU: usize = 2048;

/// Hard cap on concurrent allocations. TURN is an amplifier and an open relay
/// by nature; a request that would exceed this is refused with 508 without
/// consulting the model, because resource exhaustion is not a policy question.
const MAX_ALLOCATIONS: usize = 256;

/// Permission lifetime (RFC 8656 section 9: fixed at 5 minutes).
const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);

/// Channel binding lifetime (RFC 8656 section 11: 10 minutes).
const CHANNEL_LIFETIME: Duration = Duration::from_secs(600);

/// Longest allocation lifetime we will grant, whatever the model asks for.
const MAX_LIFETIME_SECONDS: u32 = 3600;

// STUN/TURN attribute types used here.
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;

/// Everything about an allocation that both the request path and the relay task
/// need: when it dies, which peers it may talk to, and its channel bindings.
///
/// The expiry lives here rather than beside the socket so the relay task can
/// enforce it per packet. Keeping it only in the allocation table meant a dead
/// allocation kept relaying until the 30-second cleanup tick noticed.
pub struct AllocationState {
    expires_at: Instant,
    lifetime_seconds: u32,
    /// Permitted peer IPs and their expiry. RFC 8656 matches permissions on the
    /// IP address only, deliberately ignoring the port.
    permissions: HashMap<IpAddr, Instant>,
    /// Channel number -> (peer, expiry)
    channels: HashMap<u16, (SocketAddr, Instant)>,
}

impl AllocationState {
    fn new(lifetime_seconds: u32) -> Self {
        Self {
            expires_at: Instant::now() + Duration::from_secs(lifetime_seconds as u64),
            lifetime_seconds,
            permissions: HashMap::new(),
            channels: HashMap::new(),
        }
    }

    fn is_live(&self) -> bool {
        self.expires_at > Instant::now()
    }

    fn refresh(&mut self, lifetime_seconds: u32) {
        self.expires_at = Instant::now() + Duration::from_secs(lifetime_seconds as u64);
        self.lifetime_seconds = lifetime_seconds;
    }

    fn permit(&mut self, ip: IpAddr) {
        self.permissions
            .insert(ip, Instant::now() + PERMISSION_LIFETIME);
    }

    fn is_permitted(&self, ip: &IpAddr) -> bool {
        self.permissions
            .get(ip)
            .is_some_and(|expiry| *expiry > Instant::now())
    }

    fn bind_channel(&mut self, number: u16, peer: SocketAddr) {
        self.channels
            .insert(number, (peer, Instant::now() + CHANNEL_LIFETIME));
        self.permit(peer.ip());
    }

    /// Peer bound to `number`, if the binding has not expired.
    fn channel_peer(&self, number: u16) -> Option<SocketAddr> {
        self.channels
            .get(&number)
            .filter(|(_, expiry)| *expiry > Instant::now())
            .map(|(peer, _)| *peer)
    }

    /// Channel bound to `peer`, if any binding has not expired.
    fn channel_for_peer(&self, peer: &SocketAddr) -> Option<u16> {
        let now = Instant::now();
        self.channels
            .iter()
            .find(|(_, (bound, expiry))| bound == peer && *expiry > now)
            .map(|(number, _)| *number)
    }

    fn permitted_ips(&self) -> Vec<String> {
        let now = Instant::now();
        let mut ips: Vec<String> = self
            .permissions
            .iter()
            .filter(|(_, expiry)| **expiry > now)
            .map(|(ip, _)| ip.to_string())
            .collect();
        ips.sort();
        ips
    }

    fn bound_channels(&self) -> Vec<serde_json::Value> {
        let now = Instant::now();
        let mut bound: Vec<(u16, SocketAddr)> = self
            .channels
            .iter()
            .filter(|(_, (_, expiry))| *expiry > now)
            .map(|(number, (peer, _))| (*number, *peer))
            .collect();
        bound.sort_by_key(|(number, _)| *number);
        bound
            .into_iter()
            .map(|(number, peer)| {
                serde_json::json!({ "channel_number": number, "peer_address": peer.to_string() })
            })
            .collect()
    }
}

/// A live TURN allocation: a bound relay socket plus the policy attached to it.
pub struct TurnAllocation {
    client_addr: SocketAddr,
    relay_addr: SocketAddr,
    relay_socket: Arc<UdpSocket>,
    #[allow(dead_code)]
    allocated_at: Instant,
    state: Arc<Mutex<AllocationState>>,
    relay_task: JoinHandle<()>,
}

impl Drop for TurnAllocation {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches the task rather than stopping it, so
        // without this an expired (or replaced, or server-stopped) allocation
        // would keep a relay socket bound and keep forwarding peer traffic.
        self.relay_task.abort();
    }
}

/// TURN server: allocation table plus the accept loop.
pub struct TurnServer {
    allocations: Arc<Mutex<HashMap<String, TurnAllocation>>>, // Key: allocation_id
    /// Relay sockets bound for Allocate requests whose policy decision is still
    /// in flight. Counted towards `MAX_ALLOCATIONS` so a flood of requests
    /// cannot exhaust file descriptors while the model thinks.
    reservations: Arc<AtomicUsize>,
}

/// Decrements the in-flight reservation count however the request ends.
struct ReservationGuard(Arc<AtomicUsize>);

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for TurnServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the per-datagram task needs. Passed as one struct because the
/// alternative is a twelve-argument function.
#[derive(Clone)]
struct TurnContext {
    server: Arc<TurnServer>,
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    /// IP the relay sockets are bound to.
    relay_bind_ip: IpAddr,
    /// IP reported to clients in XOR-RELAYED-ADDRESS. Differs from
    /// `relay_bind_ip` when the server sits behind NAT (`relay_ip` parameter).
    advertised_relay_ip: IpAddr,
    llm_client: OllamaClient,
    state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<TurnProtocol>,
    server_id: ServerId,
}

impl TurnServer {
    pub fn new() -> Self {
        Self {
            allocations: Arc::new(Mutex::new(HashMap::new())),
            reservations: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Spawn TURN server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
        relay_ip: Option<String>,
    ) -> Result<SocketAddr> {
        let advertised_relay_ip = match relay_ip {
            Some(ip) => Some(
                ip.parse::<IpAddr>()
                    .with_context(|| format!("Invalid relay_ip '{ip}': expected an IP address"))?,
            ),
            None => None,
        };

        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("TURN server listening on {}", local_addr));

        let relay_bind_ip = local_addr.ip();
        let advertised_relay_ip = advertised_relay_ip.unwrap_or(relay_bind_ip);
        if advertised_relay_ip.is_unspecified() {
            Log::new(Some(&status_tx)).warn(format!(
                "TURN relay address will be advertised as {} because the server is bound to a \
                 wildcard address; set the 'relay_ip' startup parameter to the address clients \
                 should send relayed traffic to",
                advertised_relay_ip
            ));
        }

        let server = Arc::new(Self::new());
        Self::spawn_cleanup_task(&server, status_tx.clone());

        let ctx = TurnContext {
            server,
            socket: socket.clone(),
            local_addr,
            relay_bind_ip,
            advertised_relay_ip,
            llm_client,
            state: app_state.clone(),
            status_tx: status_tx.clone(),
            protocol: Arc::new(TurnProtocol::new()),
            server_id,
        };

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            let mut buffer = vec![0u8; RELAY_MTU];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();

                        if n == buffer.len() {
                            warn!(
                                "TURN datagram from {} filled the {}-byte receive buffer and may \
                                 have been truncated",
                                peer_addr, RELAY_MTU
                            );
                        }

                        Self::register_connection(&ctx, peer_addr, n).await;

                        Log::new(Some(&ctx.status_tx))
                            .debug(format!("TURN received {} bytes from {}", n, peer_addr));
                        trace!("TURN data (hex): {}", hex::encode(&data));

                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            handle_datagram(ctx, data, peer_addr).await;
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("TURN receive error: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    async fn register_connection(ctx: &TurnContext, peer_addr: SocketAddr, bytes: usize) {
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };

        let connection_id = ConnectionId::new(ctx.state.get_next_unified_id().await);
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr: peer_addr,
            local_addr: ctx.local_addr,
            bytes_sent: 0,
            bytes_received: bytes as u64,
            packets_sent: 0,
            packets_received: 1,
            last_activity: now,
            status: ConnectionStatus::Active,
            status_changed_at: now,
            protocol_info: ProtocolConnectionInfo::empty(),
        };
        ctx.state
            .add_connection_to_server(ctx.server_id, conn_state)
            .await;
        let _ = ctx.status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Spawn task to periodically clean up expired allocations.
    ///
    /// Holds only a weak reference: `register_server_task` stores a single handle
    /// per server (the accept loop's), so this task cannot be registered for
    /// abort and must notice on its own when the server has been stopped.
    fn spawn_cleanup_task(server: &Arc<Self>, status_tx: mpsc::UnboundedSender<String>) {
        let server = Arc::downgrade(server);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;

                let Some(server) = server.upgrade() else {
                    debug!("TURN server stopped, ending allocation cleanup task");
                    break;
                };

                let mut allocations = server.allocations.lock().await;

                let mut expired = Vec::new();
                for (id, alloc) in allocations.iter() {
                    if !alloc.state.lock().await.is_live() {
                        expired.push((id.clone(), alloc.client_addr, alloc.relay_addr));
                    }
                }

                let removed = expired.len();
                for (id, client_addr, relay_addr) in expired {
                    // Dropping the allocation aborts its relay task and closes
                    // the relay socket (see `impl Drop for TurnAllocation`).
                    allocations.remove(&id);
                    Log::new(Some(&status_tx)).debug(format!(
                        "TURN expired allocation {} for {} (relay {})",
                        id, client_addr, relay_addr
                    ));
                }

                if removed > 0 {
                    Log::new(Some(&status_tx)).debug(format!(
                        "TURN cleanup removed {} expired allocations",
                        removed
                    ));
                }
            }
        });
    }

    /// Allocation belonging to `client_addr`, if any is still live.
    async fn allocation_for_client(
        &self,
        client_addr: SocketAddr,
    ) -> Option<(String, Arc<UdpSocket>, Arc<Mutex<AllocationState>>)> {
        let allocations = self.allocations.lock().await;
        for (id, alloc) in allocations.iter() {
            if alloc.client_addr != client_addr {
                continue;
            }
            if !alloc.state.lock().await.is_live() {
                continue;
            }
            return Some((id.clone(), alloc.relay_socket.clone(), alloc.state.clone()));
        }
        None
    }

    /// JSON summary of this client's allocations, for the LLM event payload.
    async fn allocation_summary(&self, client_addr: SocketAddr) -> Vec<serde_json::Value> {
        let now = Instant::now();
        let allocations = self.allocations.lock().await;
        let mut summary = Vec::new();
        for (id, alloc) in allocations.iter() {
            if alloc.client_addr != client_addr {
                continue;
            }
            let state = alloc.state.lock().await;
            if !state.is_live() {
                continue;
            }
            summary.push(serde_json::json!({
                "allocation_id": id,
                "relay_address": alloc.relay_addr.to_string(),
                "lifetime_seconds": state.lifetime_seconds,
                "expires_in_seconds": state.expires_at.saturating_duration_since(now).as_secs(),
                "permitted_peers": state.permitted_ips(),
                "channels": state.bound_channels(),
            }));
        }
        summary
    }
}

// ---------------------------------------------------------------------------
// Message parsing
// ---------------------------------------------------------------------------

/// A decoded STUN/TURN message. Every field is derived with bounds checks: the
/// input is an unauthenticated datagram from anyone who can reach the socket,
/// and a panic in this task would be silent while the server kept reporting
/// itself as Running.
#[derive(Debug)]
struct TurnMessage {
    class: u16,
    method: u16,
    transaction_id: [u8; 12],
    attributes: Vec<(u16, Vec<u8>)>,
}

impl TurnMessage {
    fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 20 {
            return None;
        }

        let raw_type = u16::from_be_bytes([data[0], data[1]]);
        // The two most significant bits of a STUN message type are always zero;
        // anything else is a ChannelData frame or not STUN at all.
        if raw_type & 0xC000 != 0 {
            return None;
        }

        if u32::from_be_bytes([data[4], data[5], data[6], data[7]]) != MAGIC_COOKIE {
            return None;
        }

        // RFC 8489 section 5: C0 is bit 4 and C1 is bit 8, class == C1<<1 | C0.
        let c0 = (raw_type >> 4) & 0x1;
        let c1 = (raw_type >> 8) & 0x1;
        let class = (c1 << 1) | c0;
        let method = (raw_type & 0x000F) | ((raw_type & 0x00E0) >> 1) | ((raw_type & 0x3E00) >> 2);

        let mut transaction_id = [0u8; 12];
        transaction_id.copy_from_slice(&data[8..20]);

        // Trust the shorter of the declared length and what actually arrived.
        let declared = u16::from_be_bytes([data[2], data[3]]) as usize;
        let end = 20usize.saturating_add(declared).min(data.len());

        let mut attributes = Vec::new();
        let mut pos = 20usize;
        while pos + 4 <= end {
            let attr_type = u16::from_be_bytes([data[pos], data[pos + 1]]);
            let attr_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
            let value_start = pos + 4;
            let value_end = match value_start.checked_add(attr_len) {
                Some(v) if v <= end => v,
                // Truncated or lying length: stop, keep what we parsed.
                _ => break,
            };
            attributes.push((attr_type, data[value_start..value_end].to_vec()));

            let padded = attr_len.saturating_add(3) & !3usize;
            pos = match value_start.checked_add(padded) {
                Some(p) => p,
                None => break,
            };
        }

        Some(Self {
            class,
            method,
            transaction_id,
            attributes,
        })
    }

    fn attribute(&self, attr_type: u16) -> Option<&[u8]> {
        self.attributes
            .iter()
            .find(|(t, _)| *t == attr_type)
            .map(|(_, v)| v.as_slice())
    }

    fn lifetime(&self) -> Option<u32> {
        let value = self.attribute(ATTR_LIFETIME)?;
        if value.len() < 4 {
            return None;
        }
        Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
    }

    fn requested_transport(&self) -> Option<u8> {
        self.attribute(ATTR_REQUESTED_TRANSPORT)
            .and_then(|v| v.first().copied())
    }

    fn channel_number(&self) -> Option<u16> {
        let value = self.attribute(ATTR_CHANNEL_NUMBER)?;
        if value.len() < 2 {
            return None;
        }
        Some(u16::from_be_bytes([value[0], value[1]]))
    }

    fn data(&self) -> Option<&[u8]> {
        self.attribute(ATTR_DATA)
    }

    /// All XOR-PEER-ADDRESS attributes. CreatePermission may carry several.
    fn peer_addresses(&self) -> Vec<SocketAddr> {
        self.attributes
            .iter()
            .filter(|(t, _)| *t == ATTR_XOR_PEER_ADDRESS)
            .filter_map(|(_, v)| decode_xor_address(v, &self.transaction_id))
            .collect()
    }

    fn type_name(&self) -> &'static str {
        match (self.class, self.method) {
            (0, 1) => "BindingRequest",
            (0, 3) => "AllocateRequest",
            (0, 4) => "RefreshRequest",
            (0, 8) => "CreatePermissionRequest",
            (0, 9) => "ChannelBindRequest",
            (1, 6) => "SendIndication",
            (1, 7) => "DataIndication",
            (2, 3) => "AllocateResponse",
            (2, 4) => "RefreshResponse",
            (2, 8) => "CreatePermissionResponse",
            (2, 9) => "ChannelBindResponse",
            (3, _) => "ErrorResponse",
            _ => "Unknown",
        }
    }
}

/// Decode an XOR-PEER-ADDRESS / XOR-RELAYED-ADDRESS attribute value.
fn decode_xor_address(value: &[u8], transaction_id: &[u8; 12]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]) ^ (MAGIC_COOKIE >> 16) as u16;
    let magic = MAGIC_COOKIE.to_be_bytes();

    match family {
        0x01 => {
            if value.len() < 8 {
                return None;
            }
            let mut octets = [0u8; 4];
            for i in 0..4 {
                octets[i] = value[4 + i] ^ magic[i];
            }
            Some(SocketAddr::new(IpAddr::from(octets), port))
        }
        0x02 => {
            if value.len() < 20 {
                return None;
            }
            let mut octets = [0u8; 16];
            for i in 0..4 {
                octets[i] = value[4 + i] ^ magic[i];
            }
            for i in 0..12 {
                octets[4 + i] = value[8 + i] ^ transaction_id[i];
            }
            Some(SocketAddr::new(IpAddr::from(octets), port))
        }
        _ => None,
    }
}

/// True for a ChannelData frame (RFC 8656 section 12: channel numbers occupy
/// 0x4000-0x7FFF, so the top two bits are 0b01).
fn is_channel_data(data: &[u8]) -> bool {
    data.len() >= 4 && (data[0] & 0xC0) == 0x40
}

fn is_valid_channel_number(number: u16) -> bool {
    (0x4000..=0x7FFF).contains(&number)
}

// ---------------------------------------------------------------------------
// Datagram dispatch
// ---------------------------------------------------------------------------

async fn handle_datagram(ctx: TurnContext, data: Vec<u8>, peer_addr: SocketAddr) {
    // ChannelData frames are not STUN messages and must be tested first.
    if is_channel_data(&data) {
        handle_channel_data(&ctx, &data, peer_addr).await;
        return;
    }

    let Some(msg) = TurnMessage::parse(&data) else {
        Log::new(Some(&ctx.status_tx)).debug(format!("TURN invalid message from {}", peer_addr));
        return;
    };

    match (msg.class, msg.method) {
        // Binding requests carry no policy: the answer is the client's own
        // address, which is a fact and not something for the model to invent.
        (0, 1) => match TurnProtocol::build_binding_response(&msg.transaction_id, peer_addr) {
            Ok(packet) => {
                let _ = ctx.socket.send_to(&packet, peer_addr).await;
                Log::new(Some(&ctx.status_tx))
                    .debug(format!("TURN binding response to {}", peer_addr));
            }
            Err(e) => error!("TURN failed to build binding response: {}", e),
        },
        (0, 3) => handle_allocate_request(&ctx, &msg, &data, peer_addr).await,
        (0, 4) => handle_refresh_request(&ctx, &msg, &data, peer_addr).await,
        (0, 8) => handle_create_permission_request(&ctx, &msg, &data, peer_addr).await,
        (0, 9) => handle_channel_bind_request(&ctx, &msg, &data, peer_addr).await,
        (1, 6) => handle_send_indication(&ctx, &msg, peer_addr).await,
        _ => {
            Log::new(Some(&ctx.status_tx)).debug(format!(
                "TURN unhandled message type {} from {}",
                msg.type_name(),
                peer_addr
            ));
        }
    }
}

/// Build the event payload fields every TURN event carries.
async fn base_event_data(
    ctx: &TurnContext,
    msg: &TurnMessage,
    raw_len: usize,
    peer_addr: SocketAddr,
) -> serde_json::Value {
    serde_json::json!({
        "peer_addr": peer_addr.to_string(),
        "local_addr": ctx.local_addr.to_string(),
        "transaction_id": hex::encode(msg.transaction_id),
        "message_type": msg.type_name(),
        "bytes_received": raw_len,
        "existing_allocations": ctx.server.allocation_summary(peer_addr).await,
    })
}

/// Run the policy decision for one control request.
///
/// Three outcomes are kept apart, both in the log (`decision=`) and on the wire, because
/// collapsing them is how a fail-closed protocol turns into a fail-open one:
///
/// * `decision=fail_closed_no_policy` — no operator policy, so no LLM call and, deliberately,
///   **no reply**. Nothing was asked and nothing failed; the request is simply not served.
/// * `decision=fail_closed_llm_error` — the backend errored. Nothing is granted, *and* the
///   client is told so with a STUN error response, so it stops retransmitting for ~39s.
/// * the model answered — the caller inspects the actions; a missing grant action is a
///   model refusal (`decision=model_reject`, logged at the call sites).
///
/// The error response carries a **category only** (`crate::utils::WireFailure`), never the
/// backend's message: the reason phrase goes on the wire to a stranger, the error goes to the
/// log. `Overloaded` maps to 508 Insufficient Capacity and `Unavailable` to 500 Server Error —
/// both in STUN's 5xx "temporarily unable, may retry" class, but distinct so a client can tell
/// saturation from a broken backend.
async fn call_llm_for_event(
    ctx: &TurnContext,
    event_type: &'static EventType,
    event_data: serde_json::Value,
    transaction_id: &[u8; 12],
    method: u16,
    peer_addr: SocketAddr,
) -> Option<ExecutionResult> {
    let event = Event::new(event_type, event_data);

    // Whether to grant an Allocate/Refresh/CreatePermission/ChannelBind, and to which peers, is
    // a policy decision, not something the request determines (the ack packet framing is
    // mechanical, but the grant is not). With no operator policy — no server instruction and no
    // per-event handler — TURN's own fail-closed default applies: grant nothing. So return None
    // WITHOUT an LLM round-trip; the caller drops the reserved relay socket exactly as it does
    // for a refusal or an LLM failure. The model is consulted only when the operator opts in
    // with the grant policy it should apply.
    if !operator_wants_dynamic(&ctx.state, ctx.server_id, &event_type.id).await {
        Log::new(Some(&ctx.status_tx)).debug(format!(
            "TURN {} decision=fail_closed_no_policy: no operator policy configured (no \
             instruction or handler), fail-closed with no LLM call",
            event_type.id
        ));
        return None;
    }

    match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        None, // TURN is UDP: no persistent connection
        &event,
        ctx.protocol.as_ref(),
    )
    .await
    {
        Ok(result) => {
            let log = Log::new(Some(&ctx.status_tx));
            for message in &result.messages {
                log.info(format!("{}", message));
            }
            log.debug(format!("TURN parsed {} actions", result.raw_actions.len()));
            Some(result)
        }
        Err(e) => {
            // The full error goes to the log and the status stream, where an operator looks.
            error!(
                "TURN {} decision=fail_closed_llm_error for {}: {:#}",
                event_type.id, peer_addr, e
            );
            Log::new(Some(&ctx.status_tx)).warn(format!(
                "TURN {} decision=fail_closed_llm_error for {}: {}",
                event_type.id, peer_addr, e
            ));

            // The client gets a category, never the error. Silence here left it retransmitting
            // for the full STUN Rc/Rm schedule (~39s) before giving up.
            let (code, reason) = match crate::utils::WireFailure::classify(&e) {
                crate::utils::WireFailure::Overloaded => {
                    (508u16, crate::utils::WireFailure::Overloaded.text())
                }
                crate::utils::WireFailure::Unavailable => {
                    (500u16, crate::utils::WireFailure::Unavailable.text())
                }
            };
            send_local_error(ctx, transaction_id, method, code, reason, peer_addr).await;
            None
        }
    }
}

/// Write every byte the executed actions produced back to the client.
async fn send_outputs(ctx: &TurnContext, result: &ExecutionResult, peer_addr: SocketAddr) {
    for protocol_result in &result.protocol_results {
        for output in protocol_result.get_all_output() {
            let _ = ctx.socket.send_to(&output, peer_addr).await;
            let log = Log::new(Some(&ctx.status_tx));
            log.debug(format!("TURN sent {} bytes to {}", output.len(), peer_addr));
            log.trace(format!("TURN sent (hex): {}", hex::encode(&output)));
        }
    }
}

/// Send an error response NetGet decided on itself (resource limits, malformed
/// requests, a model answer we cannot honour).
async fn send_local_error(
    ctx: &TurnContext,
    transaction_id: &[u8; 12],
    method: u16,
    code: u16,
    reason: &str,
    peer_addr: SocketAddr,
) {
    match TurnProtocol::build_error_response(transaction_id, method, code, reason) {
        Ok(packet) => {
            let _ = ctx.socket.send_to(&packet, peer_addr).await;
            Log::new(Some(&ctx.status_tx)).debug(format!(
                "TURN sent error {} {} to {}",
                code, reason, peer_addr
            ));
        }
        Err(e) => error!("TURN failed to build error response: {}", e),
    }
}

fn find_action<'a>(result: &'a ExecutionResult, name: &str) -> Option<&'a serde_json::Value> {
    result
        .raw_actions
        .iter()
        .find(|a| a.get("type").and_then(|v| v.as_str()) == Some(name))
}

// ---------------------------------------------------------------------------
// Allocate
// ---------------------------------------------------------------------------

async fn handle_allocate_request(
    ctx: &TurnContext,
    msg: &TurnMessage,
    raw: &[u8],
    peer_addr: SocketAddr,
) {
    // RFC 8656 section 6.2 step 3: UDP is the only transport we relay.
    if let Some(transport) = msg.requested_transport() {
        if transport != 17 {
            warn!(
                "TURN allocate from {} requested transport {} (only UDP/17 is relayed)",
                peer_addr, transport
            );
            send_local_error(
                ctx,
                &msg.transaction_id,
                3,
                442,
                "Unsupported Transport Protocol",
                peer_addr,
            )
            .await;
            return;
        }
    }

    let in_flight = ctx.server.reservations.fetch_add(1, Ordering::Relaxed) + 1;
    let _reservation = ReservationGuard(ctx.server.reservations.clone());

    if ctx.server.allocations.lock().await.len() + in_flight > MAX_ALLOCATIONS {
        warn!(
            "TURN refusing allocate from {}: at the {}-allocation cap",
            peer_addr, MAX_ALLOCATIONS
        );
        send_local_error(
            ctx,
            &msg.transaction_id,
            3,
            508,
            "Insufficient Capacity",
            peer_addr,
        )
        .await;
        return;
    }

    // Reserve a real relay socket *before* asking the model, so the address it
    // is told to hand out is one NetGet is actually listening on. If the model
    // refuses, the socket is dropped at the end of this function and closed.
    let relay_socket = match UdpSocket::bind(SocketAddr::new(ctx.relay_bind_ip, 0)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            Log::new(Some(&ctx.status_tx))
                .error(format!("TURN could not bind a relay socket: {}", e));
            send_local_error(
                ctx,
                &msg.transaction_id,
                3,
                508,
                "Insufficient Capacity",
                peer_addr,
            )
            .await;
            return;
        }
    };
    let bound_addr = match relay_socket.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            error!("TURN could not read relay socket address: {}", e);
            return;
        }
    };
    let relay_addr = SocketAddr::new(ctx.advertised_relay_ip, bound_addr.port());

    let mut event_data = base_event_data(ctx, msg, raw.len(), peer_addr).await;
    if let Some(obj) = event_data.as_object_mut() {
        obj.insert(
            "relay_address".to_string(),
            serde_json::json!(relay_addr.to_string()),
        );
        obj.insert(
            "requested_lifetime_seconds".to_string(),
            match msg.lifetime() {
                Some(l) => serde_json::json!(l),
                None => serde_json::Value::Null,
            },
        );
        obj.insert(
            "requested_transport".to_string(),
            match msg.requested_transport() {
                Some(17) => serde_json::json!("udp"),
                Some(6) => serde_json::json!("tcp"),
                Some(other) => serde_json::json!(other),
                None => serde_json::Value::Null,
            },
        );
    }

    let Some(result) = call_llm_for_event(
        ctx,
        &TURN_ALLOCATE_REQUEST_EVENT,
        event_data,
        &msg.transaction_id,
        3,
        peer_addr,
    )
    .await
    else {
        return;
    };

    let Some(action) = find_action(&result, "send_turn_allocate_response") else {
        // No grant: refusal/error/ignore all land here. The reserved socket is
        // dropped, so nothing is ever relayed. This is the model answering, so it
        // is a policy decision — distinct from a backend failure.
        Log::new(Some(&ctx.status_tx)).info(format!(
            "TURN allocate decision=model_reject for {}: no send_turn_allocate_response action",
            peer_addr
        ));
        send_outputs(ctx, &result, peer_addr).await;
        return;
    };

    // The relay address is a fact about a socket NetGet bound, not a value the
    // model may choose: handing the client an address nobody listens on is the
    // exact failure this protocol used to ship. Refuse rather than confirm it.
    let claimed = action
        .get("relay_address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if claimed != relay_addr.to_string() {
        Log::new(Some(&ctx.status_tx)).warn(format!(
            "TURN refusing allocation for {}: action relay_address {:?} is not the reserved \
             address {} (use {{{{event.relay_address}}}})",
            peer_addr, claimed, relay_addr
        ));
        send_local_error(
            ctx,
            &msg.transaction_id,
            3,
            508,
            "Insufficient Capacity",
            peer_addr,
        )
        .await;
        return;
    }

    let lifetime = action
        .get("lifetime_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(600)
        .min(MAX_LIFETIME_SECONDS as u64) as u32;
    let allocation_id = action
        .get("allocation_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| hex::encode(msg.transaction_id));

    let state = Arc::new(Mutex::new(AllocationState::new(lifetime)));
    let relay_task = spawn_relay_task(
        relay_socket.clone(),
        ctx.socket.clone(),
        peer_addr,
        relay_addr,
        state.clone(),
    );

    let allocation = TurnAllocation {
        client_addr: peer_addr,
        relay_addr,
        relay_socket,
        allocated_at: Instant::now(),
        state,
        relay_task,
    };

    {
        let mut allocations = ctx.server.allocations.lock().await;
        // One allocation per client 5-tuple: replacing drops the old one, which
        // aborts its relay task and closes its socket.
        let stale: Vec<String> = allocations
            .iter()
            .filter(|(id, a)| a.client_addr == peer_addr && *id != &allocation_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale {
            warn!(
                "TURN replacing existing allocation {} for {}",
                id, peer_addr
            );
            allocations.remove(&id);
        }
        allocations.insert(allocation_id.clone(), allocation);
    }

    Log::new(Some(&ctx.status_tx)).info(format!(
        "TURN allocation {} for {} relaying on {} for {}s",
        allocation_id, peer_addr, relay_addr, lifetime
    ));

    send_outputs(ctx, &result, peer_addr).await;
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

async fn handle_refresh_request(
    ctx: &TurnContext,
    msg: &TurnMessage,
    raw: &[u8],
    peer_addr: SocketAddr,
) {
    let mut event_data = base_event_data(ctx, msg, raw.len(), peer_addr).await;
    if let Some(obj) = event_data.as_object_mut() {
        obj.insert(
            "requested_lifetime_seconds".to_string(),
            match msg.lifetime() {
                Some(l) => serde_json::json!(l),
                None => serde_json::Value::Null,
            },
        );
    }

    let Some(result) = call_llm_for_event(
        ctx,
        &TURN_REFRESH_REQUEST_EVENT,
        event_data,
        &msg.transaction_id,
        4,
        peer_addr,
    )
    .await
    else {
        return;
    };

    if find_action(&result, "send_turn_refresh_response").is_none() {
        Log::new(Some(&ctx.status_tx)).info(format!(
            "TURN refresh decision=model_reject for {}: no send_turn_refresh_response action",
            peer_addr
        ));
    }

    if let Some(action) = find_action(&result, "send_turn_refresh_response") {
        let lifetime = action
            .get("lifetime_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(600)
            .min(MAX_LIFETIME_SECONDS as u64) as u32;

        let mut allocations = ctx.server.allocations.lock().await;
        if lifetime == 0 {
            // RFC 8656 section 7: lifetime 0 deletes the allocation.
            let doomed: Vec<String> = allocations
                .iter()
                .filter(|(_, a)| a.client_addr == peer_addr)
                .map(|(id, _)| id.clone())
                .collect();
            for id in doomed {
                allocations.remove(&id);
                info!("TURN deleted allocation {} for {}", id, peer_addr);
            }
        } else {
            let mut refreshed = 0usize;
            for alloc in allocations.values() {
                if alloc.client_addr == peer_addr {
                    alloc.state.lock().await.refresh(lifetime);
                    refreshed += 1;
                }
            }
            if refreshed == 0 {
                warn!(
                    "TURN refresh response sent to {} which holds no allocation",
                    peer_addr
                );
            }
        }
    }

    send_outputs(ctx, &result, peer_addr).await;
}

// ---------------------------------------------------------------------------
// CreatePermission
// ---------------------------------------------------------------------------

async fn handle_create_permission_request(
    ctx: &TurnContext,
    msg: &TurnMessage,
    raw: &[u8],
    peer_addr: SocketAddr,
) {
    let requested_peers = msg.peer_addresses();

    let mut event_data = base_event_data(ctx, msg, raw.len(), peer_addr).await;
    if let Some(obj) = event_data.as_object_mut() {
        obj.insert(
            "peer_addresses".to_string(),
            serde_json::json!(requested_peers
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()),
        );
    }

    let Some(result) = call_llm_for_event(
        ctx,
        &TURN_CREATE_PERMISSION_REQUEST_EVENT,
        event_data,
        &msg.transaction_id,
        8,
        peer_addr,
    )
    .await
    else {
        return;
    };

    if find_action(&result, "send_turn_create_permission_response").is_none() {
        Log::new(Some(&ctx.status_tx)).info(format!(
            "TURN create_permission decision=model_reject for {}: no \
             send_turn_create_permission_response action",
            peer_addr
        ));
    }

    if let Some(action) = find_action(&result, "send_turn_create_permission_response") {
        // An explicit list lets the model permit a subset; omitting it permits
        // every peer the request named. Peers the request did *not* name are
        // never permitted, so a hallucinated address cannot open a hole.
        let chosen: Vec<IpAddr> = match action.get("peer_addresses").and_then(|v| v.as_array()) {
            Some(list) => list
                .iter()
                .filter_map(|v| v.as_str())
                .filter_map(parse_peer_ip)
                .filter(|ip| {
                    let known = requested_peers.iter().any(|p| p.ip() == *ip);
                    if !known {
                        warn!(
                            "TURN ignoring permission for {} which {} did not ask for",
                            ip, peer_addr
                        );
                    }
                    known
                })
                .collect(),
            None => requested_peers.iter().map(|p| p.ip()).collect(),
        };

        if let Some((id, _, state)) = ctx.server.allocation_for_client(peer_addr).await {
            let mut state = state.lock().await;
            for ip in &chosen {
                state.permit(*ip);
            }
            Log::new(Some(&ctx.status_tx)).info(format!(
                "TURN allocation {} permits {:?} for {} ({} peer(s))",
                id,
                chosen,
                peer_addr,
                chosen.len()
            ));
        } else {
            warn!(
                "TURN permission response sent to {} which holds no allocation",
                peer_addr
            );
        }
    }

    send_outputs(ctx, &result, peer_addr).await;
}

fn parse_peer_ip(value: &str) -> Option<IpAddr> {
    if let Ok(addr) = value.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    value.parse::<IpAddr>().ok()
}

// ---------------------------------------------------------------------------
// ChannelBind
// ---------------------------------------------------------------------------

async fn handle_channel_bind_request(
    ctx: &TurnContext,
    msg: &TurnMessage,
    raw: &[u8],
    peer_addr: SocketAddr,
) {
    let channel_number = msg.channel_number();
    let peer = msg.peer_addresses().into_iter().next();

    let (Some(channel_number), Some(peer)) = (channel_number, peer) else {
        warn!(
            "TURN channel bind from {} missing CHANNEL-NUMBER or XOR-PEER-ADDRESS",
            peer_addr
        );
        send_local_error(ctx, &msg.transaction_id, 9, 400, "Bad Request", peer_addr).await;
        return;
    };

    if !is_valid_channel_number(channel_number) {
        warn!(
            "TURN channel bind from {} used out-of-range channel {}",
            peer_addr, channel_number
        );
        send_local_error(ctx, &msg.transaction_id, 9, 400, "Bad Request", peer_addr).await;
        return;
    }

    let mut event_data = base_event_data(ctx, msg, raw.len(), peer_addr).await;
    if let Some(obj) = event_data.as_object_mut() {
        obj.insert(
            "channel_number".to_string(),
            serde_json::json!(channel_number),
        );
        obj.insert(
            "peer_address".to_string(),
            serde_json::json!(peer.to_string()),
        );
    }

    let Some(result) = call_llm_for_event(
        ctx,
        &TURN_CHANNEL_BIND_REQUEST_EVENT,
        event_data,
        &msg.transaction_id,
        9,
        peer_addr,
    )
    .await
    else {
        return;
    };

    if find_action(&result, "send_turn_channel_bind_response").is_none() {
        Log::new(Some(&ctx.status_tx)).info(format!(
            "TURN channel_bind decision=model_reject for {}: no send_turn_channel_bind_response \
             action",
            peer_addr
        ));
    }

    if find_action(&result, "send_turn_channel_bind_response").is_some() {
        if let Some((id, _, state)) = ctx.server.allocation_for_client(peer_addr).await {
            state.lock().await.bind_channel(channel_number, peer);
            Log::new(Some(&ctx.status_tx)).info(format!(
                "TURN allocation {} bound channel {} to {}",
                id, channel_number, peer
            ));
        } else {
            warn!(
                "TURN channel bind response sent to {} which holds no allocation",
                peer_addr
            );
        }
    }

    send_outputs(ctx, &result, peer_addr).await;
}

// ---------------------------------------------------------------------------
// Data plane: client -> peer
// ---------------------------------------------------------------------------

async fn handle_send_indication(ctx: &TurnContext, msg: &TurnMessage, peer_addr: SocketAddr) {
    let Some(peer) = msg.peer_addresses().into_iter().next() else {
        debug!(
            "TURN send indication from {} has no peer address",
            peer_addr
        );
        return;
    };
    let Some(payload) = msg.data() else {
        debug!("TURN send indication from {} has no DATA", peer_addr);
        return;
    };

    // RFC 8656 section 10.2: indications are never answered with an error;
    // anything we cannot relay is silently discarded.
    let Some((id, relay_socket, state)) = ctx.server.allocation_for_client(peer_addr).await else {
        debug!(
            "TURN send indication from {} which holds no allocation, discarded",
            peer_addr
        );
        return;
    };

    if !state.lock().await.is_permitted(&peer.ip()) {
        debug!(
            "TURN send indication from {} to unpermitted peer {}, discarded",
            peer_addr, peer
        );
        return;
    }

    match relay_socket.send_to(payload, peer).await {
        Ok(n) => trace!(
            "TURN allocation {} relayed {} bytes from {} to {}",
            id,
            n,
            peer_addr,
            peer
        ),
        Err(e) => warn!("TURN relay to {} failed: {}", peer, e),
    }
}

async fn handle_channel_data(ctx: &TurnContext, data: &[u8], peer_addr: SocketAddr) {
    if data.len() < 4 {
        return;
    }
    let channel_number = u16::from_be_bytes([data[0], data[1]]);
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    let end = match 4usize.checked_add(length) {
        Some(end) if end <= data.len() => end,
        _ => {
            debug!(
                "TURN channel data from {} declares {} bytes but carries {}",
                peer_addr,
                length,
                data.len().saturating_sub(4)
            );
            return;
        }
    };
    let payload = &data[4..end];

    let Some((id, relay_socket, state)) = ctx.server.allocation_for_client(peer_addr).await else {
        debug!(
            "TURN channel data from {} which holds no allocation, discarded",
            peer_addr
        );
        return;
    };

    let peer = {
        let state = state.lock().await;
        match state.channel_peer(channel_number) {
            Some(peer) if state.is_permitted(&peer.ip()) => peer,
            _ => {
                debug!(
                    "TURN channel data from {} on unbound channel {}, discarded",
                    peer_addr, channel_number
                );
                return;
            }
        }
    };

    match relay_socket.send_to(payload, peer).await {
        Ok(n) => trace!(
            "TURN allocation {} relayed {} bytes from {} to {} (channel {})",
            id,
            n,
            peer_addr,
            peer,
            channel_number
        ),
        Err(e) => warn!("TURN relay to {} failed: {}", peer, e),
    }
}

// ---------------------------------------------------------------------------
// Data plane: peer -> client
// ---------------------------------------------------------------------------

/// Forward everything arriving on the relay socket to the client, as a Data
/// indication or a ChannelData frame. Per-packet logging stays on `trace!`
/// (file only) because the status channel is unbounded and has no backpressure.
fn spawn_relay_task(
    relay_socket: Arc<UdpSocket>,
    turn_socket: Arc<UdpSocket>,
    client_addr: SocketAddr,
    relay_addr: SocketAddr,
    state: Arc<Mutex<AllocationState>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0u8; RELAY_MTU];
        loop {
            let (n, src_addr) = match relay_socket.recv_from(&mut buffer).await {
                Ok(v) => v,
                Err(e) => {
                    debug!("TURN relay socket {} closed: {}", relay_addr, e);
                    break;
                }
            };

            let (live, permitted, channel) = {
                let state = state.lock().await;
                (
                    state.is_live(),
                    state.is_permitted(&src_addr.ip()),
                    state.channel_for_peer(&src_addr),
                )
            };

            if !live {
                // The allocation expired; the cleanup tick has not removed it
                // yet. Relaying now would outlive the lifetime the client was
                // promised.
                debug!(
                    "TURN relay {} dropped {} bytes from {}: allocation expired",
                    relay_addr, n, src_addr
                );
                continue;
            }

            if !permitted {
                debug!(
                    "TURN relay {} dropped {} bytes from unpermitted peer {}",
                    relay_addr, n, src_addr
                );
                continue;
            }

            let packet = match channel {
                Some(number) => TurnProtocol::build_channel_data(number, &buffer[..n]),
                None => match TurnProtocol::build_data_indication(src_addr, &buffer[..n]) {
                    Ok(packet) => packet,
                    Err(e) => {
                        warn!("TURN could not build data indication: {}", e);
                        continue;
                    }
                },
            };

            match turn_socket.send_to(&packet, client_addr).await {
                Ok(_) => trace!(
                    "TURN relay {} forwarded {} bytes from {} to client {}",
                    relay_addr,
                    n,
                    src_addr,
                    client_addr
                ),
                Err(e) => warn!("TURN could not reach client {}: {}", client_addr, e),
            }
        }
    })
}

/// Returns true if the operator opted into dynamic (LLM- or handler-driven) responses for this
/// server: either a non-empty server instruction was given, or an event handler is configured
/// for `event_id`. When false the protocol applies its static default and never consults the
/// model — for TURN that default is to grant nothing (fail closed), because whether to grant an
/// allocation/permission and to which peers is a policy decision with no configured policy to
/// apply.
async fn operator_wants_dynamic(state: &AppState, server_id: ServerId, event_id: &str) -> bool {
    state
        .with_server_mut(server_id, |server| {
            let has_instruction = !server.instruction.trim().is_empty();
            let has_handler = server
                .event_handler_config
                .as_ref()
                .map(|c| c.find_handler(event_id).is_some())
                .unwrap_or(false);
            has_instruction || has_handler
        })
        .await
        .unwrap_or(false)
}
