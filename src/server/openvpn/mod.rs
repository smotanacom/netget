//! OpenVPN **control-plane responder**.
//!
//! # What this is
//!
//! A server that speaks the unauthenticated front half of the OpenVPN UDP
//! protocol: it decodes the real wire format, answers a client's session reset
//! with a `P_CONTROL_HARD_RESET_SERVER_V2` that a genuine `openvpn` client
//! accepts, acknowledges the control packets the client sends next, and asks the
//! LLM whether each peer should be answered at all.
//!
//! It is useful as a honeypot and as a protocol observatory: it tells you who is
//! probing UDP/1194, what OpenVPN version and options they lead with, and it
//! captures the client's TLS ClientHello.
//!
//! # What this is not
//!
//! **It is not a VPN and never carries traffic.** There is no TLS control
//! channel, so there is no key exchange, so there are no data-channel keys, no
//! TUN device and no tunnel. A real client gets as far as sending its
//! ClientHello, receives ACKs for it, and then times out waiting for a
//! ServerHello that this server cannot produce. Use `wireguard` for a real VPN.
//!
//! This is a deliberate reduction. The previous implementation carried a TUN
//! device and an AES-256-GCM/ChaCha20-Poly1305 data path keyed by HKDF over
//! three string literals committed to this repository — every peer on every
//! installation derived the same key. That path was also unreachable: no peer
//! can ever legitimately be keyed without a key exchange, and no real client
//! ever reached it, because the reset reply it was answering with had its
//! fields in the wrong order. It has been removed rather than left as
//! decorative encryption, which also drops the protocol's root requirement and
//! makes it testable.
//!
//! # Interoperability, and how it was established
//!
//! Verified against OpenVPN 2.7.4 (`aarch64-apple-darwin`, OpenSSL 3.6.2):
//!
//! ```text
//! client -> P_CONTROL_HARD_RESET_CLIENT_V2   (14 bytes)
//! server -> P_CONTROL_HARD_RESET_SERVER_V2   (26 bytes)
//!           client logs: TLS: Initial packet from [AF_INET]127.0.0.1:PORT, sid=...
//! client -> P_CONTROL_V1 x2                  (TLS ClientHello, fragmented)
//! server -> P_ACK_V1 x2
//!           client logs: UDPv4 READ [22] ... P_ACK_V1 kid=0 [ 1 ] DATA len=0
//!           and stops retransmitting
//! ```
//!
//! # Unsupported client options
//!
//! `--tls-auth`, `--tls-crypt` and `--tls-crypt-v2` wrap or displace the
//! reliability fields this server reads. Rather than mis-parse them into
//! plausible-looking nonsense, such frames are detected and refused with an
//! explicit log line (see [`packet::ControlFrame::is_plain_reset`] and
//! [`packet::Opcode::is_tls_crypt_v2`]).

pub mod actions;
pub mod crypto;
pub mod packet;
pub mod peer;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::wire_failure::WireFailure;
use actions::{OpenvpnProtocol, OPENVPN_PEER_RESET_EVENT};
use anyhow::{Context, Result};
use packet::{ControlFrame, DataFrame};
use peer::{Peer, PeerAdmission, PeerManager};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// Maximum number of peers tracked at once. Beyond this, resets are ignored.
const MAX_PEERS: usize = 100;

/// A peer with no traffic for this long is forgotten.
const PEER_IDLE_TIMEOUT_SECS: u64 = 120;

/// How often the idle sweep runs.
const SWEEP_INTERVAL_SECS: u64 = 30;

/// OpenVPN control-plane responder.
pub struct OpenvpnServer {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    peer_manager: Arc<PeerManager>,
    /// This server's OpenVPN session id, freshly random per run.
    server_session_id: u64,
    llm_client: Arc<OllamaClient>,
}

impl OpenvpnServer {
    /// Bind the UDP socket and start serving.
    ///
    /// Returns only once the socket is bound, so a bind failure surfaces as
    /// `Err` and `server_startup` can mark the server `Error` instead of
    /// reporting a server that is not listening as `Running`.
    pub async fn spawn_with_llm_actions(
        bind_addr: SocketAddr,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<SocketAddr> {
        let socket = UdpSocket::bind(bind_addr)
            .await
            .with_context(|| format!("Failed to bind OpenVPN UDP socket on {}", bind_addr))?;
        let local_addr = socket.local_addr()?;

        let server_session_id = rand::random::<u64>();

        let log = Log::new(Some(&status_tx));
        log.info(format!(
            "OpenVPN control-plane responder listening on {} (session id {:016x})",
            local_addr, server_session_id
        ));
        log.warn(
            "OpenVPN is a control-plane responder only: it answers the session reset \
             and ACKs control packets, but has no TLS control channel, no key exchange and \
             no data channel, so no tunnel is ever established. Use WireGuard for a real VPN.",
        );

        let server = Arc::new(OpenvpnServer {
            socket: Arc::new(socket),
            local_addr,
            peer_manager: Arc::new(PeerManager::new()),
            server_session_id,
            llm_client,
        });

        // `register_server_task` keeps exactly one handle per server, so the
        // receive loop and the idle sweep run inside a single task joined by
        // `select!`. Registering two would silently drop the first handle and
        // leak that loop past `stop_server`.
        let loop_server = server.clone();
        let loop_state = app_state.clone();
        let loop_status = status_tx.clone();
        let accept_handle = tokio::spawn(async move {
            tokio::select! {
                res = loop_server.clone().recv_loop(loop_state.clone(), server_id, loop_status) => {
                    if let Err(e) = res {
                        error!("OpenVPN receive loop stopped: {}", e);
                    }
                }
                _ = loop_server.sweep_loop(loop_state, server_id) => {}
            }
        });

        app_state
            .register_server_task(server_id, accept_handle)
            .await;

        log.info(format!("OpenVPN server ready on {}", local_addr));
        Ok(local_addr)
    }

    /// Receive and dispatch UDP datagrams.
    async fn recv_loop(
        self: Arc<Self>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let mut buf = vec![0u8; packet::MAX_PACKET_SIZE];

        loop {
            let (len, peer_addr) = self.socket.recv_from(&mut buf).await?;
            let datagram = &buf[..len];

            trace!("OpenVPN: {} bytes from {}", len, peer_addr);

            let (opcode, key_id) = match packet::parse_opcode_byte(datagram) {
                Ok(v) => v,
                Err(e) => {
                    debug!("OpenVPN: undecodable datagram from {}: {}", peer_addr, e);
                    continue;
                }
            };

            if opcode.is_tls_crypt_v2() {
                Log::new(Some(&status_tx)).warn(format!(
                    "OpenVPN: {} uses tls-crypt-v2 ({:?}), which this server cannot decode; ignoring",
                    peer_addr, opcode
                ));
                continue;
            }

            if opcode.is_data() {
                self.handle_data(datagram, peer_addr, &status_tx).await;
                continue;
            }

            let frame = match ControlFrame::parse(datagram) {
                Ok(f) => f,
                Err(e) => {
                    debug!("OpenVPN: malformed {:?} from {}: {}", opcode, peer_addr, e);
                    continue;
                }
            };

            if opcode.is_client_reset() {
                self.clone()
                    .handle_client_reset(
                        frame,
                        peer_addr,
                        key_id,
                        app_state.clone(),
                        server_id,
                        status_tx.clone(),
                    )
                    .await;
            } else if opcode.is_ack() {
                self.handle_ack(frame, peer_addr).await;
            } else {
                self.handle_control(frame, peer_addr, &status_tx).await;
            }
        }
    }

    /// Drop peers that have gone quiet, so a scan cannot grow the peer table
    /// without bound and hold the `MAX_PEERS` slots forever.
    async fn sweep_loop(&self, app_state: Arc<AppState>, server_id: crate::state::ServerId) {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
        ticker.tick().await; // fires immediately; skip it

        loop {
            ticker.tick().await;
            let expired = self
                .peer_manager
                .remove_idle_peers(std::time::Duration::from_secs(PEER_IDLE_TIMEOUT_SECS))
                .await;
            for peer in expired {
                debug!("OpenVPN: forgetting idle peer {}", peer.addr);
                if peer.admission == PeerAdmission::Accepted {
                    app_state
                        .close_connection_on_server(server_id, peer.connection_id)
                        .await;
                }
            }
        }
    }

    /// Handle `P_CONTROL_HARD_RESET_CLIENT_V1/V2`: the first packet of a
    /// handshake, and the only point at which policy is decided.
    async fn handle_client_reset(
        self: Arc<Self>,
        frame: ControlFrame,
        peer_addr: SocketAddr,
        key_id: u8,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        // A plain reset carries nothing after its packet id. Anything else means
        // the client wrapped the control channel with --tls-auth or --tls-crypt,
        // in which case the fields we just read are not the fields it wrote.
        if !frame.is_plain_reset() {
            Log::new(Some(&status_tx)).warn(format!(
                "OpenVPN: reset from {} is not a plain reset ({} payload bytes, {} ACKs); \
                 the client is probably using --tls-auth or --tls-crypt, which is not \
                 supported. Ignoring rather than mis-parsing it.",
                peer_addr,
                frame.payload.len(),
                frame.ack_packet_ids.len()
            ));
            return;
        }

        let client_packet_id = frame.packet_id.unwrap_or(0);

        // Resets are retransmitted every couple of seconds until the client
        // hears back, and an LLM decision takes longer than that. Reuse the
        // existing peer so one handshake costs at most one model call.
        if let Some(existing) = self.peer_manager.get_peer(&peer_addr).await {
            if existing.session_id == frame.session_id {
                match existing.admission {
                    PeerAdmission::Deciding => {
                        trace!(
                            "OpenVPN: reset retransmit from {} while a decision is in flight",
                            peer_addr
                        );
                    }
                    PeerAdmission::Accepted => {
                        // The client missed our reply; send it again verbatim.
                        self.resend_reset_reply(&existing, peer_addr, client_packet_id)
                            .await;
                    }
                    PeerAdmission::Rejected => {
                        trace!(
                            "OpenVPN: ignoring reset retransmit from rejected {}",
                            peer_addr
                        );
                    }
                }
                self.peer_manager.touch(&peer_addr).await;
                return;
            }
            // Different session id from the same address: the client restarted.
            debug!("OpenVPN: {} restarted with a new session id", peer_addr);
            self.peer_manager.remove_peer(&peer_addr).await;
        }

        if self.peer_manager.count().await >= MAX_PEERS {
            Log::new(Some(&status_tx)).warn(format!(
                "OpenVPN: peer table full ({}), ignoring reset from {}",
                MAX_PEERS, peer_addr
            ));
            return;
        }

        let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);
        let peer = Peer::new(connection_id, peer_addr, frame.session_id, key_id);
        self.peer_manager.add_peer(peer).await;

        Log::new(Some(&status_tx)).info(format!(
            "OpenVPN reset from {} (session {:016x}, {:?})",
            peer_addr, frame.session_id, frame.opcode
        ));

        // Ask the model before answering. The decision can take seconds, so it
        // runs off the receive loop.
        let this = self.clone();
        tokio::spawn(async move {
            this.decide_and_answer(
                peer_addr,
                connection_id,
                frame,
                key_id,
                client_packet_id,
                app_state,
                server_id,
                status_tx,
            )
            .await;
        });
    }

    /// Raise `openvpn_peer_reset` and act on the answer.
    ///
    /// Fails closed: only an explicit `accept_peer` produces a reply. A
    /// `reject_peer`, an empty answer, or an LLM error all leave the peer
    /// unanswered, and the three outcomes are logged distinctly so a refusal is
    /// never confused with silence.
    ///
    /// **Silence is the protocol's refusal, not a missing feature.** Before the
    /// TLS control channel exists OpenVPN has exactly one server-to-client
    /// message, `P_CONTROL_HARD_RESET_SERVER_V2`, and sending it *is* admitting
    /// the peer — there is no NAK, no error packet, and `AUTH_FAILED` is a
    /// push message that only exists once TLS is up, which this server never
    /// reaches. A real OpenVPN server drops what it will not admit (that is
    /// what an HMAC failure under `--tls-auth` does). So answering on backend
    /// failure would be the fail-open bug, not a fix for it: there is no reply
    /// that means "try again later", only one that means "you are in".
    ///
    /// What the peer cannot be told, the operator is. Every outcome is logged
    /// with a stable `decision=` token — `model_accept`, `model_reject`,
    /// `fail_closed_no_action`, `fail_closed_llm_error` — mirroring
    /// `src/server/radius/`, so `decision=fail_closed_` greps out every peer
    /// the model did not actually answer for. The LLM-error line additionally
    /// carries the [`WireFailure`] class, which is the distinction a protocol
    /// with two error codes would have put on the wire, and the full error,
    /// which never leaves the log.
    #[allow(clippy::too_many_arguments)]
    async fn decide_and_answer(
        &self,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        frame: ControlFrame,
        key_id: u8,
        client_packet_id: u32,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let event = Event::new(
            &OPENVPN_PEER_RESET_EVENT,
            serde_json::json!({
                "peer_addr": peer_addr.to_string(),
                "client_session_id": format!("{:016x}", frame.session_id),
                "key_id": key_id,
                "reset_type": format!("{:?}", frame.opcode),
                "packet_id": client_packet_id,
                "peer_count": self.peer_manager.count().await,
            }),
        );

        let protocol = OpenvpnProtocol::new();
        let outcome = call_llm(
            &self.llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            &protocol,
        )
        .await;

        let log = Log::new(Some(&status_tx));
        let decision = match outcome {
            Ok(result) => {
                for message in &result.messages {
                    log.info(message);
                }
                extract_decision(&result.protocol_results)
            }
            Err(e) => {
                // The error is rendered here and nowhere else: the peer gets no
                // bytes at all, so nothing can leak, and the operator needs the
                // whole chain. The class is what a protocol with a retryable
                // error code would have signalled instead.
                let class = match WireFailure::classify(&e) {
                    WireFailure::Overloaded => "overloaded",
                    WireFailure::Unavailable => "unavailable",
                };
                log.error(format!(
                    "OpenVPN {} decision=fail_closed_llm_error class={} - leaving it \
                     unanswered (the protocol has no reply that is not an admission): {}",
                    peer_addr, class, e
                ));
                None
            }
        };

        match decision {
            Some(Decision::Accept { reason }) => {
                self.accept_peer(
                    peer_addr,
                    connection_id,
                    frame.session_id,
                    key_id,
                    client_packet_id,
                    reason,
                    &app_state,
                    server_id,
                    &status_tx,
                )
                .await;
            }
            Some(Decision::Reject { reason }) => {
                self.peer_manager
                    .set_admission(&peer_addr, PeerAdmission::Rejected)
                    .await;
                log.info(format!(
                    "OpenVPN {} decision=model_reject - refused, no reply sent ({})",
                    peer_addr,
                    reason.as_deref().unwrap_or("no reason given")
                ));
            }
            None => {
                // Distinct from an explicit rejection on purpose: this is the
                // "nothing usable came back" path, and it must not fall through
                // to answering the peer.
                self.peer_manager
                    .set_admission(&peer_addr, PeerAdmission::Rejected)
                    .await;
                log.warn(format!(
                    "OpenVPN {} decision=fail_closed_no_action - the model produced neither \
                     accept_peer nor reject_peer; leaving it unanswered",
                    peer_addr
                ));
            }
        }
    }

    /// Send `P_CONTROL_HARD_RESET_SERVER_V2` and start tracking the peer.
    #[allow(clippy::too_many_arguments)]
    async fn accept_peer(
        &self,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        client_session_id: u64,
        key_id: u8,
        client_packet_id: u32,
        reason: Option<String>,
        app_state: &Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        // The server's first control packet is packet id 0, matching the
        // client's numbering.
        let reply = ControlFrame::hard_reset_server_v2(
            key_id,
            self.server_session_id,
            client_session_id,
            client_packet_id,
            0,
        )
        .serialize()
        .to_vec();

        if let Err(e) = self.socket.send_to(&reply, peer_addr).await {
            Log::new(Some(status_tx))
                .error(format!("OpenVPN: reply to {} failed: {}", peer_addr, e));
            return;
        }

        self.peer_manager
            .update_peer(&peer_addr, |p| {
                p.admission = PeerAdmission::Accepted;
                p.reset_reply = Some(reply.clone());
                p.record_sent(reply.len() as u64);
            })
            .await;

        let now = std::time::Instant::now();
        app_state
            .add_connection_to_server(
                server_id,
                ConnectionState {
                    id: connection_id,
                    remote_addr: peer_addr,
                    local_addr: self.local_addr,
                    bytes_sent: reply.len() as u64,
                    bytes_received: 0,
                    packets_sent: 1,
                    packets_received: 1,
                    last_activity: now,
                    status: ConnectionStatus::Active,
                    status_changed_at: now,
                    protocol_info: ProtocolConnectionInfo::empty(),
                },
            )
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        Log::new(Some(status_tx)).info(format!(
            "OpenVPN {} decision=model_accept - answered with HARD_RESET_SERVER_V2 ({} bytes){}",
            peer_addr,
            reply.len(),
            reason.map(|r| format!(" - {}", r)).unwrap_or_default()
        ));
    }

    /// Re-send the reply we already built for a peer whose reset was
    /// retransmitted.
    async fn resend_reset_reply(&self, peer: &Peer, peer_addr: SocketAddr, client_packet_id: u32) {
        let reply = match &peer.reset_reply {
            Some(bytes) => bytes.clone(),
            None => ControlFrame::hard_reset_server_v2(
                peer.key_id,
                self.server_session_id,
                peer.session_id,
                client_packet_id,
                0,
            )
            .serialize()
            .to_vec(),
        };

        if let Err(e) = self.socket.send_to(&reply, peer_addr).await {
            warn!("OpenVPN: reset re-send to {} failed: {}", peer_addr, e);
        } else {
            trace!("OpenVPN: re-sent reset reply to {}", peer_addr);
        }
    }

    /// Handle `P_CONTROL_V1`: acknowledge it, and record what the client sent.
    ///
    /// The payload is the client's TLS stream. This server has no TLS control
    /// channel, so the bytes are logged and dropped; the ACK is still correct
    /// and stops the client retransmitting.
    async fn handle_control(
        &self,
        frame: ControlFrame,
        peer_addr: SocketAddr,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let peer = match self.peer_manager.get_peer(&peer_addr).await {
            Some(p) if p.admission == PeerAdmission::Accepted => p,
            Some(_) => {
                trace!("OpenVPN: control packet from unanswered peer {}", peer_addr);
                return;
            }
            None => {
                trace!("OpenVPN: control packet from unknown peer {}", peer_addr);
                return;
            }
        };

        let packet_id = match frame.packet_id {
            Some(id) => id,
            None => return,
        };

        let hint = describe_tls_payload(&frame.payload);
        debug!(
            "OpenVPN: control packet {} from {} ({} bytes, {})",
            packet_id,
            peer_addr,
            frame.payload.len(),
            hint
        );
        trace!(
            "OpenVPN: control payload from {}: {}",
            peer_addr,
            hex_prefix(&frame.payload, 64)
        );

        if frame.payload.is_empty() {
            // Pure ACK-carrying control packet; nothing new to acknowledge.
            self.peer_manager.touch(&peer_addr).await;
            return;
        }

        let ack = ControlFrame::ack(
            peer.key_id,
            self.server_session_id,
            peer.session_id,
            vec![packet_id],
        )
        .serialize()
        .to_vec();

        if let Err(e) = self.socket.send_to(&ack, peer_addr).await {
            warn!("OpenVPN: ACK to {} failed: {}", peer_addr, e);
            return;
        }

        let first_control = self
            .peer_manager
            .update_peer_returning(&peer_addr, |p| {
                p.record_received(frame.payload.len() as u64);
                p.record_sent(ack.len() as u64);
                let first = !p.saw_control_payload;
                p.saw_control_payload = true;
                first
            })
            .await
            .unwrap_or(false);

        if first_control {
            Log::new(Some(status_tx)).info(format!(
                "OpenVPN: {} sent {} ({} bytes); ACKed. No TLS control channel exists, so \
                 the handshake stops here and no tunnel is established.",
                peer_addr,
                hint,
                frame.payload.len()
            ));
        }
    }

    /// Handle `P_ACK_V1` from a client. Nothing is retransmitted by this server,
    /// so an ACK only refreshes liveness.
    async fn handle_ack(&self, frame: ControlFrame, peer_addr: SocketAddr) {
        trace!(
            "OpenVPN: ACK from {} for {:?}",
            peer_addr,
            frame.ack_packet_ids
        );
        self.peer_manager.touch(&peer_addr).await;
    }

    /// Handle `P_DATA_V1/V2`.
    ///
    /// No peer can be keyed, because no key exchange happens, so every data
    /// packet is unopenable by construction. Say so once per peer instead of
    /// pretending to decrypt.
    async fn handle_data(
        &self,
        datagram: &[u8],
        peer_addr: SocketAddr,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let frame = match DataFrame::parse(datagram) {
            Ok(f) => f,
            Err(e) => {
                debug!("OpenVPN: malformed data packet from {}: {}", peer_addr, e);
                return;
            }
        };

        let first = self
            .peer_manager
            .update_peer_returning(&peer_addr, |p| {
                p.record_received(frame.payload.len() as u64);
                let first = !p.saw_data_packet;
                p.saw_data_packet = true;
                first
            })
            .await
            .unwrap_or(false);

        debug!(
            "OpenVPN: dropping {:?} from {} ({} ciphertext bytes, peer id {:?}): no data \
             channel keys exist",
            frame.opcode,
            peer_addr,
            frame.payload.len(),
            frame.peer_id
        );

        if first {
            Log::new(Some(status_tx)).warn(format!(
                "OpenVPN: {} sent a data packet, but no key exchange has taken place, so it \
                 cannot be decrypted and is dropped",
                peer_addr
            ));
        }
    }

    /// Peers currently answered by this server.
    pub async fn accepted_peers(&self) -> Vec<SocketAddr> {
        self.peer_manager
            .get_all_peers()
            .await
            .into_iter()
            .filter(|p| p.admission == PeerAdmission::Accepted)
            .map(|p| p.addr)
            .collect()
    }
}

/// What the model decided about a peer.
enum Decision {
    Accept { reason: Option<String> },
    Reject { reason: Option<String> },
}

/// Pull the first accept/reject decision out of the executed action results.
///
/// Returns `None` when the model produced neither, which callers must treat as
/// a refusal rather than a default.
fn extract_decision(results: &[ActionResult]) -> Option<Decision> {
    fn walk(results: &[ActionResult], out: &mut Option<Decision>) {
        for result in results {
            if out.is_some() {
                return;
            }
            match result {
                ActionResult::Custom { name, data } if name == actions::PEER_DECISION_RESULT => {
                    let reason = data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    match data.get("accept").and_then(|v| v.as_bool()) {
                        Some(true) => *out = Some(Decision::Accept { reason }),
                        Some(false) => *out = Some(Decision::Reject { reason }),
                        None => {}
                    }
                }
                ActionResult::Multiple(inner) => walk(inner, out),
                _ => {}
            }
        }
    }

    let mut out = None;
    walk(results, &mut out);
    out
}

/// Best-effort description of a TLS record, for logs only.
fn describe_tls_payload(payload: &[u8]) -> &'static str {
    match payload.first() {
        Some(0x16) => "a TLS handshake record (ClientHello)",
        Some(0x14) => "a TLS change-cipher-spec record",
        Some(0x15) => "a TLS alert record",
        Some(0x17) => "a TLS application-data record",
        Some(_) => "a non-TLS control payload",
        None => "an empty control payload",
    }
}

/// Hex-dump at most `limit` bytes, for TRACE logging.
fn hex_prefix(data: &[u8], limit: usize) -> String {
    let shown = &data[..data.len().min(limit)];
    let mut s = String::with_capacity(shown.len() * 2 + 16);
    for b in shown {
        s.push_str(&format!("{:02x}", b));
    }
    if data.len() > shown.len() {
        s.push_str(&format!("... ({} bytes total)", data.len()));
    }
    s
}
