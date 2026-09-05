//! Tracking for OpenVPN peers seen by the control-channel server.
//!
//! A "peer" here is a source address that sent a session reset. It is transport
//! state only — nothing is persisted, and the table is swept of idle entries so
//! a scan cannot pin every slot forever.

use crate::server::connection::ConnectionId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Whether this server has decided to answer a peer.
///
/// The three states are kept distinct on purpose: `Rejected` and "no decision
/// came back" both stop us answering, but only `Accepted` may ever send bytes,
/// so there is no state in which an absent decision reads as approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAdmission {
    /// A decision is in flight; the peer has not been answered.
    Deciding,
    /// Answered with `P_CONTROL_HARD_RESET_SERVER_V2`.
    Accepted,
    /// Refused, or no usable decision was produced. Never answered.
    Rejected,
}

/// A peer of the OpenVPN control-channel server.
#[derive(Clone)]
pub struct Peer {
    pub connection_id: ConnectionId,
    pub addr: SocketAddr,
    /// The client's OpenVPN session id, from its reset packet.
    pub session_id: u64,
    /// The key id the client used, echoed back in our replies.
    pub key_id: u8,
    pub admission: PeerAdmission,

    /// Whether a non-empty control payload has been seen, so the "feeding it to
    /// the TLS session" notice is logged once rather than per retransmission.
    pub saw_control_payload: bool,
    /// Same, for undecryptable data packets.
    pub saw_data_packet: bool,

    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub last_activity: Instant,
}

impl Peer {
    pub fn new(connection_id: ConnectionId, addr: SocketAddr, session_id: u64, key_id: u8) -> Self {
        Peer {
            connection_id,
            addr,
            session_id,
            key_id,
            admission: PeerAdmission::Deciding,
            saw_control_payload: false,
            saw_data_packet: false,
            bytes_sent: 0,
            bytes_received: 0,
            last_activity: Instant::now(),
        }
    }

    pub fn record_sent(&mut self, bytes: u64) {
        self.bytes_sent = self.bytes_sent.saturating_add(bytes);
        self.last_activity = Instant::now();
    }

    pub fn record_received(&mut self, bytes: u64) {
        self.bytes_received = self.bytes_received.saturating_add(bytes);
        self.last_activity = Instant::now();
    }
}

/// In-memory peer table.
pub struct PeerManager {
    peers: RwLock<HashMap<SocketAddr, Peer>>,
}

impl PeerManager {
    pub fn new() -> Self {
        PeerManager {
            peers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn add_peer(&self, peer: Peer) {
        self.peers.write().await.insert(peer.addr, peer);
    }

    pub async fn get_peer(&self, addr: &SocketAddr) -> Option<Peer> {
        self.peers.read().await.get(addr).cloned()
    }

    pub async fn update_peer<F>(&self, addr: &SocketAddr, f: F)
    where
        F: FnOnce(&mut Peer),
    {
        if let Some(peer) = self.peers.write().await.get_mut(addr) {
            f(peer);
        }
    }

    /// Mutate a peer and return whatever the closure produced, or `None` if the
    /// peer is unknown.
    pub async fn update_peer_returning<F, T>(&self, addr: &SocketAddr, f: F) -> Option<T>
    where
        F: FnOnce(&mut Peer) -> T,
    {
        self.peers.write().await.get_mut(addr).map(f)
    }

    pub async fn set_admission(&self, addr: &SocketAddr, admission: PeerAdmission) {
        self.update_peer(addr, |p| {
            p.admission = admission;
            p.last_activity = Instant::now();
        })
        .await;
    }

    /// Refresh a peer's liveness without changing anything else.
    pub async fn touch(&self, addr: &SocketAddr) {
        self.update_peer(addr, |p| p.last_activity = Instant::now())
            .await;
    }

    pub async fn remove_peer(&self, addr: &SocketAddr) -> Option<Peer> {
        self.peers.write().await.remove(addr)
    }

    /// Drop peers idle for longer than `max_idle` and return them.
    pub async fn remove_idle_peers(&self, max_idle: Duration) -> Vec<Peer> {
        let now = Instant::now();
        let mut peers = self.peers.write().await;
        let expired: Vec<SocketAddr> = peers
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_activity) > max_idle)
            .map(|(addr, _)| *addr)
            .collect();
        expired
            .into_iter()
            .filter_map(|addr| peers.remove(&addr))
            .collect()
    }

    pub async fn get_all_peers(&self) -> Vec<Peer> {
        self.peers.read().await.values().cloned().collect()
    }

    pub async fn count(&self) -> usize {
        self.peers.read().await.len()
    }
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new()
    }
}
