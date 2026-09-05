//! One peer's OpenVPN control channel: reliability layer, TLS session and the
//! key-method-2 exchange that runs inside it.
//!
//! # Why this is separate from [`super::peer`]
//!
//! `Peer` is cloneable transport bookkeeping — addresses, counters, the
//! admission decision — and several call sites take a snapshot of it. A
//! `rustls::ServerConnection` cannot be cloned and must never be snapshotted:
//! two copies of a TLS session are two divergent record streams. So the
//! non-cloneable half lives here, in a table whose only access is a closure that
//! borrows it mutably.
//!
//! # Locking
//!
//! [`SessionManager`] hands out `&mut ControlSession` inside a synchronous
//! closure and nothing else, so the lock is never held across an `await`. Every
//! method here returns the bytes to transmit rather than transmitting them: the
//! socket write happens in `mod.rs` after the guard has been dropped.

use super::keymethod::{self, ClientKeyMethod2};
use super::packet::{ControlFrame, Opcode};
use super::reliable::{fragment, ReliableReceiver, ReliableSender};
use super::tls_channel;
use crate::server::connection::ConnectionId;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

/// Something that happened on a peer's control channel and that the server loop
/// has to act on outside the session lock.
#[derive(Debug)]
pub enum SessionEvent {
    /// The TLS handshake completed.
    TlsEstablished { version: String, cipher: String },
    /// The client sent its key material, options and credentials.
    ClientKeyExchange(Box<ClientKeyMethod2>),
    /// A NUL-terminated control message after the key exchange, e.g.
    /// `PUSH_REQUEST`.
    ControlMessage(String),
    /// The TLS session cannot continue. The session is finished; any alert
    /// rustls produced is still transmitted.
    TlsFailed(String),
}

/// The control channel of one peer.
pub struct ControlSession {
    pub connection_id: ConnectionId,
    pub client_session_id: u64,
    pub server_session_id: u64,
    pub key_id: u8,

    send: ReliableSender,
    recv: ReliableReceiver,

    tls: Option<rustls::ServerConnection>,
    /// Set once, so "handshake done" is reported exactly once.
    tls_reported: bool,
    tls_dead: bool,

    /// Control-channel plaintext read out of TLS and not yet consumed. The
    /// control channel is a byte stream, so a message can span TLS records and
    /// therefore several `P_CONTROL_V1` packets.
    plaintext: Vec<u8>,
    key_exchange_seen: bool,
    /// Whether this server has written its own key-method-2 answer.
    pub answered_key_exchange: bool,

    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl ControlSession {
    /// Start a session and queue the `P_CONTROL_HARD_RESET_SERVER_V2` that
    /// answers the client's reset.
    ///
    /// The reply goes through the reliability layer like any other control
    /// packet, so it is retransmitted until the client acknowledges it instead
    /// of relying on the client's own reset retransmissions to trigger a
    /// re-send.
    pub fn new(
        connection_id: ConnectionId,
        client_session_id: u64,
        server_session_id: u64,
        key_id: u8,
        client_reset_packet_id: u32,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> Result<Self> {
        let tls = rustls::ServerConnection::new(tls_config)
            .context("Failed to create the OpenVPN control-channel TLS session")?;

        let mut send = ReliableSender::new();
        send.queue(
            Opcode::ControlHardResetServerV2,
            vec![client_reset_packet_id],
            Vec::new(),
        );

        Ok(ControlSession {
            connection_id,
            client_session_id,
            server_session_id,
            key_id,
            send,
            recv: ReliableReceiver::new(client_reset_packet_id.saturating_add(1)),
            tls: Some(tls),
            tls_reported: false,
            tls_dead: false,
            plaintext: Vec::new(),
            key_exchange_seen: false,
            answered_key_exchange: false,
            bytes_sent: 0,
            bytes_received: 0,
        })
    }

    /// The client retransmitted its reset, so it never saw our reply. Put
    /// everything in flight back on the wire.
    pub fn on_client_reset_retransmit(&mut self) {
        self.send.mark_all_due();
    }

    /// Process a `P_ACK_V1` from the peer.
    pub fn on_ack_frame(&mut self, frame: &ControlFrame) {
        self.send.on_ack(&frame.ack_packet_ids);
    }

    /// Process a `P_CONTROL_V1` (or any control frame carrying a packet id).
    pub fn on_control_frame(&mut self, frame: ControlFrame) -> Vec<SessionEvent> {
        self.send.on_ack(&frame.ack_packet_ids);
        self.bytes_received = self
            .bytes_received
            .saturating_add(frame.payload.len() as u64);

        let packet_id = match frame.packet_id {
            Some(id) => id,
            None => return Vec::new(),
        };

        let mut events = Vec::new();
        for payload in self.recv.accept(packet_id, frame.payload) {
            if payload.is_empty() {
                // A control packet with no payload exists only to carry ACKs.
                continue;
            }
            events.extend(self.feed_tls(&payload));
        }
        events
    }

    /// Push one control payload into the TLS session and harvest whatever it
    /// produced.
    fn feed_tls(&mut self, payload: &[u8]) -> Vec<SessionEvent> {
        if self.tls_dead {
            return Vec::new();
        }
        let tls = match self.tls.as_mut() {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut cursor = std::io::Cursor::new(payload);
        while (cursor.position() as usize) < payload.len() {
            match tls.read_tls(&mut cursor) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    self.tls_dead = true;
                    return vec![SessionEvent::TlsFailed(format!(
                        "TLS record could not be buffered: {}",
                        e
                    ))];
                }
            }
            if let Err(e) = tls.process_new_packets() {
                // rustls has queued an alert describing the failure; leave the
                // connection in place so `drain_datagrams` can still send it,
                // but feed it nothing further.
                self.tls_dead = true;
                return vec![SessionEvent::TlsFailed(e.to_string())];
            }
        }

        let mut events = Vec::new();

        if !self.tls_reported && !tls.is_handshaking() {
            self.tls_reported = true;
            events.push(SessionEvent::TlsEstablished {
                version: tls_channel::protocol_version_name(tls),
                cipher: tls_channel::cipher_suite_name(tls),
            });
        }

        // Drain application data. `WouldBlock` is rustls saying "nothing more
        // right now", which is the normal exit, not a failure.
        let mut chunk = [0u8; 4096];
        loop {
            match tls.reader().read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.plaintext.extend_from_slice(&chunk[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    self.tls_dead = true;
                    events.push(SessionEvent::TlsFailed(format!(
                        "control-channel plaintext read failed: {}",
                        e
                    )));
                    return events;
                }
            }
        }

        events.extend(self.consume_plaintext());
        events
    }

    /// Turn accumulated control-channel plaintext into events.
    ///
    /// The first message is always key method 2; everything after it is a
    /// NUL-terminated ASCII control message (`PUSH_REQUEST`, `AUTH_FAILED`,
    /// `RESTART`, …).
    fn consume_plaintext(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        loop {
            if !self.key_exchange_seen {
                match keymethod::parse_client_key_method_2(&self.plaintext) {
                    Ok(Some((msg, consumed))) => {
                        self.plaintext.drain(..consumed);
                        self.key_exchange_seen = true;
                        events.push(SessionEvent::ClientKeyExchange(Box::new(msg)));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        self.tls_dead = true;
                        self.plaintext.clear();
                        events.push(SessionEvent::TlsFailed(e.to_string()));
                        break;
                    }
                }
            } else if let Some(pos) = self.plaintext.iter().position(|b| *b == 0) {
                let msg = String::from_utf8_lossy(&self.plaintext[..pos]).into_owned();
                self.plaintext.drain(..=pos);
                events.push(SessionEvent::ControlMessage(msg));
            } else {
                break;
            }
        }
        events
    }

    /// Write the server's key-method-2 answer into the TLS session.
    ///
    /// The key material is fresh random bytes. Nothing is derived from it: the
    /// data channel that would consume it does not exist, and inventing keys
    /// nothing can use would be worse than not having them.
    pub fn write_key_method_2_answer(&mut self, client_options: &str) -> Result<String> {
        let tls = self
            .tls
            .as_mut()
            .context("no TLS session on this control channel")?;

        let random1: [u8; 32] = rand::random();
        let random2: [u8; 32] = rand::random();
        let options = keymethod::server_options_from_client(client_options);
        let message = keymethod::build_server_key_method_2(&random1, &random2, &options);

        tls.writer()
            .write_all(&message)
            .context("Failed to queue the key-method-2 answer on the control channel")?;
        self.answered_key_exchange = true;
        Ok(options)
    }

    /// Everything this session wants to put on the wire right now.
    ///
    /// Pending acknowledgements go first: a peer that is retransmitting needs to
    /// hear that we have its packets before it needs anything we have to say.
    pub fn drain_datagrams(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.queue_tls_records();

        let mut out = Vec::new();

        while self.recv.has_pending_acks() {
            let acks = self.recv.take_acks();
            if acks.is_empty() {
                break;
            }
            out.push(self.serialize(Opcode::AckV1, &acks, None, &[]));
        }

        for packet in self.send.take_due(now) {
            out.push(self.serialize(
                packet.opcode,
                &packet.acks,
                Some(packet.packet_id),
                &packet.payload,
            ));
        }

        self.bytes_sent = self
            .bytes_sent
            .saturating_add(out.iter().map(|d| d.len() as u64).sum::<u64>());
        out
    }

    /// Move any TLS records rustls has produced into the reliability layer,
    /// fragmented to fit a single `P_CONTROL_V1`.
    fn queue_tls_records(&mut self) {
        let tls = match self.tls.as_mut() {
            Some(t) => t,
            None => return,
        };
        if !tls.wants_write() {
            return;
        }

        let mut records = Vec::new();
        while tls.wants_write() {
            match tls.write_tls(&mut records) {
                Ok(0) => break,
                Ok(_) => {}
                // The sink is a `Vec`, which cannot fail; treat anything else as
                // the session being over rather than looping forever.
                Err(_) => break,
            }
        }

        for chunk in fragment(&records) {
            self.send.queue(Opcode::ControlV1, Vec::new(), chunk);
        }
    }

    fn serialize(
        &self,
        opcode: Opcode,
        acks: &[u32],
        packet_id: Option<u32>,
        payload: &[u8],
    ) -> Vec<u8> {
        ControlFrame {
            opcode,
            key_id: self.key_id,
            session_id: self.server_session_id,
            ack_packet_ids: acks.to_vec(),
            // The peer session id is on the wire only when the ACK array is
            // non-empty; `ControlFrame::serialize` enforces the same rule.
            remote_session_id: if acks.is_empty() {
                None
            } else {
                Some(self.client_session_id)
            },
            packet_id,
            payload: payload.to_vec(),
        }
        .serialize()
        .to_vec()
    }

    /// True once a control packet has been retransmitted to exhaustion: the peer
    /// has stopped listening.
    pub fn is_dead(&self) -> bool {
        self.send.is_exhausted()
    }
}

/// The control sessions of every peer this server is answering.
pub struct SessionManager {
    sessions: Mutex<HashMap<SocketAddr, ControlSession>>,
}

impl SessionManager {
    pub fn new() -> Self {
        SessionManager {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn insert(&self, addr: SocketAddr, session: ControlSession) {
        self.sessions.lock().await.insert(addr, session);
    }

    pub async fn remove(&self, addr: &SocketAddr) -> bool {
        self.sessions.lock().await.remove(addr).is_some()
    }

    /// Borrow one session for the duration of a **synchronous** closure.
    ///
    /// The closure cannot await, which is the point: the guard is dropped before
    /// the caller touches the socket or the LLM.
    pub async fn with<F, T>(&self, addr: &SocketAddr, f: F) -> Option<T>
    where
        F: FnOnce(&mut ControlSession) -> T,
    {
        self.sessions.lock().await.get_mut(addr).map(f)
    }

    /// Collect the datagrams every session wants to send, and the addresses of
    /// sessions that have given up on their peer.
    pub async fn drain_all(&self, now: Instant) -> (Vec<(SocketAddr, Vec<u8>)>, Vec<SocketAddr>) {
        let mut out = Vec::new();
        let mut dead = Vec::new();
        let mut guard = self.sessions.lock().await;
        for (addr, session) in guard.iter_mut() {
            if session.is_dead() {
                dead.push(*addr);
                continue;
            }
            for datagram in session.drain_datagrams(now) {
                out.push((*addr, datagram));
            }
        }
        (out, dead)
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}
