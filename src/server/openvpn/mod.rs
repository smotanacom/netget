//! OpenVPN **control-channel server**.
//!
//! # What this is
//!
//! A server that speaks the OpenVPN UDP control channel: the wire format, the
//! reliability layer that runs underneath it, a real TLS session carried inside
//! `P_CONTROL_V1` packets, and the key-method-2 exchange that follows the
//! handshake. A genuine `openvpn` client completes its TLS handshake against
//! this server, sends its options string and `--auth-user-pass` credentials, and
//! is told whether it is admitted — by the model, not by a hardcoded rule.
//!
//! ```text
//! client ──> P_CONTROL_HARD_RESET_CLIENT_V2
//! server ──> P_CONTROL_HARD_RESET_SERVER_V2        (only if the model accepts)
//! client ──> P_CONTROL_V1 * n   TLS ClientHello
//! server ──> P_CONTROL_V1 * n   ServerHello, certificate, Finished
//! client ──> P_CONTROL_V1       key method 2: key material, options, user/pass
//! server ──> P_CONTROL_V1       key method 2 answer (only if the model accepts)
//! client ──> P_CONTROL_V1       PUSH_REQUEST                    <-- stops here
//! ```
//!
//! # What this is not
//!
//! **It is not a VPN and never carries traffic.** `PUSH_REQUEST` is not
//! answered, no data-channel keys are derived from the exchanged key material,
//! there is no TUN device, and every `P_DATA_*` packet is dropped. A real client
//! gets as far as asking for its configuration and then times out. Use
//! `wireguard` for a tunnel.
//!
//! There used to be a `crypto` module here — AES-256-GCM and ChaCha20-Poly1305
//! wrappers plus a `derive_data_keys`. It is deleted rather than kept for a
//! future data channel, because **it was not OpenVPN's key derivation and a real
//! client could never have decrypted anything it produced.** OpenVPN keys the
//! data channel with the TLS 1.0 PRF over the key-method-2 random material from
//! both peers; that function used HKDF-SHA256 with the invented label
//! `"OpenVPN data channel keys"`, and said so in its own doc comment ("For MVP,
//! we use a simplified HKDF-based approach"). Nothing called it, in `src/` or in
//! `tests/`.
//!
//! 222 lines of plausible, unreachable, non-interoperable crypto is the exact
//! shape that cost `wireguard` its Stable rating: it makes the protocol read as
//! nearly finished to anyone skimming, and whoever eventually wires it up
//! inherits a tunnel that silently talks to nobody. A data channel here starts
//! with the PRF from RFC 5246 §5 and the `key method 2` material this server
//! already parses — not with this file.
//!
//! What it *is* good for is what the control channel gives you without a
//! tunnel: it identifies who probes UDP/1194, which OpenVPN build they run
//! (their `IV_*` peer info), what options they expect, and — because the TLS
//! session is real — the username and password they were going to authenticate
//! with.
//!
//! # Client trust
//!
//! The control-channel certificate is self-signed and generated per run. A
//! client trusts it with OpenVPN 2.6+'s `--peer-fingerprint`, whose value this
//! server logs at startup. Nothing is written to disk and there is no shipped
//! key.
//!
//! # Unsupported client options
//!
//! `--tls-auth`, `--tls-crypt` and `--tls-crypt-v2` wrap or displace the
//! reliability fields this server reads. Rather than mis-parse them into
//! plausible-looking nonsense, such frames are detected and refused with an
//! explicit log line (see [`packet::ControlFrame::is_plain_reset`] and
//! [`packet::Opcode::is_tls_crypt_v2`]).

pub mod actions;
pub mod keymethod;
pub mod packet;
pub mod peer;
pub mod reliable;
pub mod session;
pub mod tls_channel;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::wire_failure::WireFailure;
use actions::{OpenvpnProtocol, OPENVPN_KEY_EXCHANGE_EVENT, OPENVPN_PEER_RESET_EVENT};
use anyhow::{Context, Result};
use keymethod::ClientKeyMethod2;
use packet::{ControlFrame, DataFrame};
use peer::{Peer, PeerAdmission, PeerManager};
use session::{ControlSession, SessionEvent, SessionManager};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// Maximum number of peers tracked at once. Beyond this, resets are ignored.
const MAX_PEERS: usize = 100;

/// A peer with no traffic for this long is forgotten.
const PEER_IDLE_TIMEOUT_SECS: u64 = 120;

/// How often the idle sweep runs.
const SWEEP_INTERVAL_SECS: u64 = 30;

/// How often the reliability layer is asked whether anything needs
/// retransmitting. Must be well below the first retransmission delay.
const RETRANSMIT_TICK_MS: u64 = 250;

/// OpenVPN control-channel server.
pub struct OpenvpnServer {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    peer_manager: Arc<PeerManager>,
    sessions: Arc<SessionManager>,
    /// This server's OpenVPN session id, freshly random per run.
    server_session_id: u64,
    tls_config: Arc<rustls::ServerConfig>,
    llm_client: Arc<OllamaClient>,
}

impl OpenvpnServer {
    /// Bind the UDP socket and start serving.
    ///
    /// Returns only once the socket is bound and the control-channel TLS
    /// configuration exists, so either failure surfaces as `Err` and
    /// `server_startup` can mark the server `Error` instead of reporting a
    /// server that is not listening as `Running`.
    pub async fn spawn_with_llm_actions(
        bind_addr: SocketAddr,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<SocketAddr> {
        // Certificate generation and root-store work are synchronous and can be
        // slow enough to matter; keep them off the runtime's worker.
        let tls = tokio::task::spawn_blocking(tls_channel::build_control_channel_tls)
            .await
            .context("TLS setup task for the OpenVPN control channel panicked")??;

        let socket = UdpSocket::bind(bind_addr)
            .await
            .with_context(|| format!("Failed to bind OpenVPN UDP socket on {}", bind_addr))?;
        let local_addr = socket.local_addr()?;

        let server_session_id = rand::random::<u64>();

        let log = Log::new(Some(&status_tx));
        log.info(format!(
            "OpenVPN control-channel server listening on {} (session id {:016x})",
            local_addr, server_session_id
        ));
        // Printed so an operator can actually connect: the certificate is
        // self-signed and generated per run, so this is the only value a client
        // can pin it by.
        log.info(format!(
            "OpenVPN control channel peer fingerprint SHA256={}",
            tls.fingerprint
        ));
        log.warn(
            "OpenVPN carries no traffic: the control channel completes a TLS handshake and the \
             key method 2 exchange, but PUSH_REQUEST is not answered, no data channel keys are \
             derived and there is no TUN device, so no tunnel is ever established. Use WireGuard \
             for a real VPN.",
        );

        let server = Arc::new(OpenvpnServer {
            socket: Arc::new(socket),
            local_addr,
            peer_manager: Arc::new(PeerManager::new()),
            sessions: Arc::new(SessionManager::new()),
            server_session_id,
            tls_config: tls.config,
            llm_client,
        });

        // `register_server_task` keeps exactly one handle per server, so the
        // receive loop, the idle sweep and the retransmission timer run inside a
        // single task joined by `select!`. Registering several would silently
        // drop all but the last handle and leak those loops past `stop_server`.
        let loop_server = server.clone();
        let loop_state = app_state.clone();
        let loop_status = status_tx.clone();
        let accept_handle = tokio::spawn(async move {
            tokio::select! {
                res = loop_server.clone().recv_loop(loop_state.clone(), server_id, loop_status.clone()) => {
                    if let Err(e) = res {
                        error!("OpenVPN receive loop stopped: {}", e);
                    }
                }
                _ = loop_server.clone().sweep_loop(loop_state, server_id) => {}
                _ = loop_server.retransmit_loop() => {}
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
                self.clone()
                    .handle_control(
                        frame,
                        peer_addr,
                        app_state.clone(),
                        server_id,
                        status_tx.clone(),
                    )
                    .await;
            }
        }
    }

    /// Drop peers that have gone quiet, so a scan cannot grow the peer table
    /// without bound and hold the `MAX_PEERS` slots forever.
    async fn sweep_loop(
        self: Arc<Self>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
    ) {
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
                self.sessions.remove(&peer.addr).await;
                if peer.admission == PeerAdmission::Accepted {
                    app_state
                        .close_connection_on_server(server_id, peer.connection_id)
                        .await;
                }
            }
        }
    }

    /// Drive the reliability layer.
    ///
    /// The control channel is a reliable layer over UDP: a control packet that
    /// is not acknowledged has to go out again, or a TLS flight spread over
    /// several datagrams stalls forever on the first loss. This is the only
    /// thing that makes multi-packet control messages work at all.
    async fn retransmit_loop(self: Arc<Self>) {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_millis(RETRANSMIT_TICK_MS));
        loop {
            ticker.tick().await;
            let (datagrams, dead) = self.sessions.drain_all(Instant::now()).await;
            for (addr, datagram) in datagrams {
                if let Err(e) = self.socket.send_to(&datagram, addr).await {
                    warn!("OpenVPN: retransmission to {} failed: {}", addr, e);
                }
            }
            for addr in dead {
                debug!(
                    "OpenVPN: {} stopped acknowledging control packets; dropping its session",
                    addr
                );
                self.sessions.remove(&addr).await;
            }
        }
    }

    /// Handle `P_CONTROL_HARD_RESET_CLIENT_V1/V2`: the first packet of a
    /// handshake, and the first of the two points at which policy is decided.
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
                        // The client missed our reply; put it back on the wire.
                        self.sessions
                            .with(&peer_addr, |s| s.on_client_reset_retransmit())
                            .await;
                        self.flush_peer(peer_addr).await;
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
            self.sessions.remove(&peer_addr).await;
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
    /// the peer — there is no NAK and no error packet at this point in the
    /// handshake. A real OpenVPN server drops what it will not admit (that is
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
                extract_decision(&result.protocol_results, actions::PEER_DECISION_RESULT)
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

    /// Open a control session and send `P_CONTROL_HARD_RESET_SERVER_V2`.
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
        let control = match ControlSession::new(
            connection_id,
            client_session_id,
            self.server_session_id,
            key_id,
            client_packet_id,
            self.tls_config.clone(),
        ) {
            Ok(s) => s,
            Err(e) => {
                // No session means no reply, which is the fail-closed outcome
                // again: better an unanswered peer than an admitted one whose
                // control channel we cannot actually run.
                self.peer_manager
                    .set_admission(&peer_addr, PeerAdmission::Rejected)
                    .await;
                Log::new(Some(status_tx)).error(format!(
                    "OpenVPN {} decision=fail_closed_no_control_channel - could not start a \
                     control session, so nothing was sent: {}",
                    peer_addr, e
                ));
                return;
            }
        };

        self.sessions.insert(peer_addr, control).await;
        self.peer_manager
            .update_peer(&peer_addr, |p| {
                p.admission = PeerAdmission::Accepted;
            })
            .await;

        let sent = self.flush_peer(peer_addr).await;
        if sent == 0 {
            Log::new(Some(status_tx)).error(format!(
                "OpenVPN: reply to {} could not be sent; the peer is left unanswered",
                peer_addr
            ));
            self.sessions.remove(&peer_addr).await;
            self.peer_manager
                .set_admission(&peer_addr, PeerAdmission::Rejected)
                .await;
            return;
        }

        let now = std::time::Instant::now();
        app_state
            .add_connection_to_server(
                server_id,
                ConnectionState {
                    id: connection_id,
                    remote_addr: peer_addr,
                    local_addr: self.local_addr,
                    bytes_sent: sent,
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
            sent,
            reason.map(|r| format!(" - {}", r)).unwrap_or_default()
        ));
    }

    /// Send whatever this peer's control session has queued. Returns the number
    /// of bytes that actually went out.
    ///
    /// The session lock is taken, the datagrams are copied out, and the guard is
    /// dropped before a single byte touches the socket — the lock is never held
    /// across the `await`.
    async fn flush_peer(&self, peer_addr: SocketAddr) -> u64 {
        let datagrams = self
            .sessions
            .with(&peer_addr, |s| s.drain_datagrams(Instant::now()))
            .await
            .unwrap_or_default();

        let mut sent = 0u64;
        for datagram in &datagrams {
            match self.socket.send_to(datagram, peer_addr).await {
                Ok(n) => sent = sent.saturating_add(n as u64),
                Err(e) => {
                    warn!("OpenVPN: send to {} failed: {}", peer_addr, e);
                    break;
                }
            }
        }
        if sent > 0 {
            self.peer_manager
                .update_peer(&peer_addr, |p| p.record_sent(sent))
                .await;
        }
        sent
    }

    /// Handle `P_CONTROL_V1`: acknowledge it, feed its payload to the peer's TLS
    /// session, and act on whatever that produces.
    async fn handle_control(
        self: Arc<Self>,
        frame: ControlFrame,
        peer_addr: SocketAddr,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        match self.peer_manager.get_peer(&peer_addr).await {
            Some(p) if p.admission == PeerAdmission::Accepted => {}
            Some(_) => {
                trace!("OpenVPN: control packet from unanswered peer {}", peer_addr);
                return;
            }
            None => {
                trace!("OpenVPN: control packet from unknown peer {}", peer_addr);
                return;
            }
        }

        let payload_len = frame.payload.len();
        let hint = describe_tls_payload(&frame.payload);
        trace!(
            "OpenVPN: control payload from {}: {}",
            peer_addr,
            hex_prefix(&frame.payload, 64)
        );

        let events = match self
            .sessions
            .with(&peer_addr, |s| s.on_control_frame(frame))
            .await
        {
            Some(events) => events,
            None => {
                trace!("OpenVPN: control packet from {} has no session", peer_addr);
                return;
            }
        };

        let (first_control, connection_id) = self
            .peer_manager
            .update_peer_returning(&peer_addr, |p| {
                p.record_received(payload_len as u64);
                let first = !p.saw_control_payload;
                p.saw_control_payload = true;
                (first, Some(p.connection_id))
            })
            .await
            .unwrap_or((false, None));

        // Keep the rail's counters and `last_activity` moving for as long as the peer is
        // really there. Nothing else updated them, so an accepted peer was drawn as 0B in
        // both directions for its whole handshake.
        if let Some(connection_id) = connection_id {
            app_state
                .update_connection_stats(
                    server_id,
                    connection_id,
                    Some(payload_len as u64),
                    None,
                    Some(1),
                    None,
                )
                .await;
        }

        if first_control && payload_len > 0 {
            Log::new(Some(&status_tx)).info(format!(
                "OpenVPN: {} sent {} ({} bytes); feeding it to the control-channel TLS session",
                peer_addr, hint, payload_len
            ));
        }

        // Acknowledge and answer before doing anything that can block.
        self.flush_peer(peer_addr).await;

        for event in events {
            self.clone()
                .handle_session_event(event, peer_addr, &app_state, server_id, &status_tx)
                .await;
        }
    }

    /// React to something the control session produced.
    async fn handle_session_event(
        self: Arc<Self>,
        event: SessionEvent,
        peer_addr: SocketAddr,
        app_state: &Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let log = Log::new(Some(status_tx));
        match event {
            SessionEvent::TlsEstablished { version, cipher } => {
                log.info(format!(
                    "OpenVPN {} control channel TLS handshake completed ({}, {})",
                    peer_addr, version, cipher
                ));
            }
            SessionEvent::ClientKeyExchange(km2) => {
                let connection_id = self
                    .sessions
                    .with(&peer_addr, |s| s.connection_id)
                    .await
                    .unwrap_or_else(|| ConnectionId::new(0));

                log.info(format!(
                    "OpenVPN {} sent key method 2: username {:?}, {} bytes of peer info",
                    peer_addr,
                    km2.username,
                    km2.peer_info.len()
                ));
                // The password is what a honeypot is here to capture, but it is
                // still a credential: it goes to the model and to DEBUG, never
                // to INFO or to the wire.
                debug!(
                    "OpenVPN {} credentials: username={:?} password={:?} options={:?}",
                    peer_addr, km2.username, km2.password, km2.options
                );

                let this = self.clone();
                let state = app_state.clone();
                let status = status_tx.clone();
                tokio::spawn(async move {
                    this.decide_key_exchange(
                        peer_addr,
                        connection_id,
                        km2,
                        state,
                        server_id,
                        status,
                    )
                    .await;
                });
            }
            SessionEvent::ControlMessage(message) => {
                if message.starts_with("PUSH_REQUEST") {
                    log.warn(format!(
                        "OpenVPN {} sent PUSH_REQUEST, which this server does not answer: it \
                         derives no data channel keys and has no TUN device, so the client will \
                         time out here rather than build a tunnel",
                        peer_addr
                    ));
                } else {
                    log.info(format!(
                        "OpenVPN {} control message: {}",
                        peer_addr,
                        crate::utils::truncate::truncate_for_log(&message, 200)
                    ));
                }
            }
            SessionEvent::TlsFailed(reason) => {
                log.warn(format!(
                    "OpenVPN {} control-channel TLS session failed: {}",
                    peer_addr, reason
                ));
                // rustls may have queued an alert; send it, then let the idle
                // sweep collect the session.
                self.flush_peer(peer_addr).await;
            }
        }
    }

    /// Raise `openvpn_client_key_exchange` and act on the answer.
    ///
    /// Fails closed exactly like the reset decision: only `accept_key_exchange`
    /// causes the server's key-method-2 answer to be written. A refusal, an
    /// empty answer and an LLM error all leave the client with nothing, and are
    /// logged under distinct `decision=` tokens.
    ///
    /// Silence is again the refusal. OpenVPN's `AUTH_FAILED` is a control
    /// message the client only looks for **after** it has read the server's
    /// key-method-2 answer; sent before it, the client parses it as that answer
    /// and reports a protocol error rather than a rejection. So there is no
    /// refusal message available at this point that the peer would understand,
    /// and sending our key material to say "no" would be the fail-open bug in a
    /// new place.
    async fn decide_key_exchange(
        self: Arc<Self>,
        peer_addr: SocketAddr,
        connection_id: ConnectionId,
        km2: Box<ClientKeyMethod2>,
        app_state: Arc<AppState>,
        server_id: crate::state::ServerId,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        // `pre_master`, `random1` and `random2` are key material and are
        // deliberately absent from the event: they are secrets, and no decision
        // can be made from 112 random bytes.
        let event = Event::new(
            &OPENVPN_KEY_EXCHANGE_EVENT,
            serde_json::json!({
                "peer_addr": peer_addr.to_string(),
                "username": km2.username,
                "password": km2.password,
                "has_credentials": !km2.username.is_empty(),
                "options": km2.options,
                "peer_info": keymethod::peer_info_map(&km2.peer_info),
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
                extract_decision(
                    &result.protocol_results,
                    actions::KEY_EXCHANGE_DECISION_RESULT,
                )
            }
            Err(e) => {
                let class = match WireFailure::classify(&e) {
                    WireFailure::Overloaded => "overloaded",
                    WireFailure::Unavailable => "unavailable",
                };
                log.error(format!(
                    "OpenVPN {} decision=fail_closed_llm_error class={} - key exchange left \
                     unanswered (no message at this point in the handshake means anything but \
                     'admitted'): {}",
                    peer_addr, class, e
                ));
                None
            }
        };

        match decision {
            Some(Decision::Accept { reason }) => {
                let written = self
                    .sessions
                    .with(&peer_addr, |s| s.write_key_method_2_answer(&km2.options))
                    .await;
                match written {
                    Some(Ok(options)) => {
                        self.flush_peer(peer_addr).await;
                        log.info(format!(
                            "OpenVPN {} decision=model_accept - answered key method 2 with \
                             options {:?}{}",
                            peer_addr,
                            crate::utils::truncate::truncate_for_log(&options, 160),
                            reason.map(|r| format!(" - {}", r)).unwrap_or_default()
                        ));
                    }
                    Some(Err(e)) => log.error(format!(
                        "OpenVPN {} decision=fail_closed_write_error - the key method 2 answer \
                         could not be written: {}",
                        peer_addr, e
                    )),
                    None => log.warn(format!(
                        "OpenVPN {} decision=fail_closed_no_session - the control session was \
                         gone before the key method 2 answer could be written",
                        peer_addr
                    )),
                }
            }
            Some(Decision::Reject { reason }) => {
                log.info(format!(
                    "OpenVPN {} decision=model_reject - key exchange refused, nothing sent ({})",
                    peer_addr,
                    reason.as_deref().unwrap_or("no reason given")
                ));
                self.sessions.remove(&peer_addr).await;
            }
            None => {
                log.warn(format!(
                    "OpenVPN {} decision=fail_closed_no_action - the model produced neither \
                     accept_key_exchange nor reject_key_exchange; the key exchange is left \
                     unanswered",
                    peer_addr
                ));
                self.sessions.remove(&peer_addr).await;
            }
        }
    }

    /// Handle `P_ACK_V1` from a client: clear what it acknowledges from the
    /// retransmission queue.
    async fn handle_ack(&self, frame: ControlFrame, peer_addr: SocketAddr) {
        trace!(
            "OpenVPN: ACK from {} for {:?}",
            peer_addr,
            frame.ack_packet_ids
        );
        self.sessions
            .with(&peer_addr, |s| s.on_ack_frame(&frame))
            .await;
        self.peer_manager.touch(&peer_addr).await;
    }

    /// Handle `P_DATA_V1/V2`.
    ///
    /// The key-method-2 exchange carries the material a real server would expand
    /// into data-channel keys, but this server derives none: there is no data
    /// channel and no TUN device to carry the plaintext to. Every data packet is
    /// therefore unopenable by construction. Say so once per peer instead of
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
                "OpenVPN: {} sent a data packet, but this server derives no data channel keys, \
                 so it cannot be decrypted and is dropped",
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

/// What the model decided.
enum Decision {
    Accept { reason: Option<String> },
    Reject { reason: Option<String> },
}

/// Pull the first accept/reject decision with the given result name out of the
/// executed action results.
///
/// Returns `None` when the model produced neither, which callers must treat as
/// a refusal rather than a default.
fn extract_decision(results: &[ActionResult], wanted: &str) -> Option<Decision> {
    fn walk(results: &[ActionResult], wanted: &str, out: &mut Option<Decision>) {
        for result in results {
            if out.is_some() {
                return;
            }
            match result {
                ActionResult::Custom { name, data } if name == wanted => {
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
                ActionResult::Multiple(inner) => walk(inner, wanted, out),
                _ => {}
            }
        }
    }

    let mut out = None;
    walk(results, wanted, &mut out);
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
