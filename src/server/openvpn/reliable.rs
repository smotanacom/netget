//! OpenVPN's control-channel reliability layer.
//!
//! OpenVPN runs its TLS control channel over UDP, so the transport has to
//! supply the guarantees TLS assumes: every control packet carries a monotonic
//! packet id, the peer acknowledges ids in the ACK array of the packets it
//! sends back (or in a standalone `P_ACK_V1`), and anything unacknowledged is
//! retransmitted. Without this, a TLS handshake spread over several datagrams
//! cannot complete: the first lost or reordered fragment desynchronises the
//! record stream permanently.
//!
//! Two halves, both deliberately transport-free — they hold no socket and no
//! session ids, so they can be unit-tested and so the caller decides when bytes
//! actually go out:
//!
//! * [`ReliableSender`] numbers outgoing packets, holds them until the peer
//!   acknowledges them, and reports which are due for a first transmission or a
//!   retransmission.
//! * [`ReliableReceiver`] delivers inbound payloads **in packet-id order**,
//!   buffering anything that arrives early, dropping duplicates and anything
//!   outside the window, and accumulating the ids that still need acknowledging.
//!
//! # Window sizes
//!
//! OpenVPN's own reliability buffer holds `RELIABLE_CAPACITY` (12) packets and
//! its ACK array reader refuses more than `RELIABLE_ACK_SIZE` (8) ids in one
//! frame — a longer array is a parse error at the peer, not merely wasteful, so
//! [`MAX_ACK_ARRAY`] must stay at or below 8. The send window is kept well
//! under the peer's capacity because exceeding it means silent drops.

use super::packet::Opcode;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Most packet ids to name in one ACK array.
///
/// OpenVPN's `reliable_ack_parse` rejects a frame whose ACK count exceeds
/// `RELIABLE_ACK_SIZE` (8). Four leaves headroom and still clears a burst.
pub const MAX_ACK_ARRAY: usize = 4;

/// Most unacknowledged packets in flight at once. The peer's reliability buffer
/// holds 12; staying well below it means a burst is never dropped for capacity.
pub const SEND_WINDOW: usize = 4;

/// How far ahead of `next_expected` an inbound packet id may be before it is
/// dropped rather than buffered.
pub const RECV_WINDOW: u32 = 8;

/// Largest payload carried by a single `P_CONTROL_V1`.
///
/// The peer sizes its control-channel receive buffer from the link MTU, so a
/// larger fragment is silently dropped by a default-configured client. A TLS
/// server flight (ServerHello, certificate, CertificateVerify, Finished) is a
/// few kilobytes and is fragmented across as many packets as this needs.
pub const MAX_CONTROL_PAYLOAD: usize = 1100;

/// First retransmission delay.
const INITIAL_RETRANSMIT: Duration = Duration::from_millis(1000);

/// Ceiling for the exponential backoff.
const MAX_RETRANSMIT: Duration = Duration::from_secs(8);

/// Transmissions of one packet before the session is declared dead. At the
/// backoff above this is roughly a minute of trying.
pub const MAX_ATTEMPTS: u32 = 8;

/// Most ids held awaiting acknowledgement. A peer that floods control packets
/// cannot make this grow without bound.
const MAX_PENDING_ACKS: usize = 64;

/// Most out-of-order payloads buffered at once.
const MAX_BUFFERED: usize = RECV_WINDOW as usize;

/// One outgoing control packet, held until the peer acknowledges it.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub packet_id: u32,
    pub opcode: Opcode,
    /// Ids acknowledged by this packet. Fixed when the packet is queued so a
    /// retransmission is byte-identical to the original — a peer that sees two
    /// different frames with one packet id has to guess which it already
    /// processed.
    pub acks: Vec<u32>,
    pub payload: Vec<u8>,
    sent_at: Option<Instant>,
    attempts: u32,
}

impl Outgoing {
    /// When this packet next needs to go out, or `None` if it never has.
    fn due_at(&self) -> Option<Instant> {
        let sent = self.sent_at?;
        let mut backoff = INITIAL_RETRANSMIT;
        for _ in 1..self.attempts {
            backoff = (backoff * 2).min(MAX_RETRANSMIT);
        }
        Some(sent + backoff)
    }
}

/// Numbers, holds and retransmits outgoing control packets.
pub struct ReliableSender {
    next_packet_id: u32,
    /// In packet-id order. The first [`SEND_WINDOW`] entries are the ones
    /// allowed on the wire.
    outgoing: VecDeque<Outgoing>,
}

impl ReliableSender {
    pub fn new() -> Self {
        ReliableSender {
            next_packet_id: 0,
            outgoing: VecDeque::new(),
        }
    }

    /// Queue a packet and return the id it was given.
    pub fn queue(&mut self, opcode: Opcode, acks: Vec<u32>, payload: Vec<u8>) -> u32 {
        let packet_id = self.next_packet_id;
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        self.outgoing.push_back(Outgoing {
            packet_id,
            opcode,
            acks,
            payload,
            sent_at: None,
            attempts: 0,
        });
        packet_id
    }

    /// Drop everything the peer has acknowledged.
    pub fn on_ack(&mut self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        self.outgoing.retain(|o| !ids.contains(&o.packet_id));
    }

    /// Packets that should go on the wire now, marked as transmitted.
    ///
    /// Only the first [`SEND_WINDOW`] queued packets are considered, so a large
    /// TLS flight is released a window at a time as the peer acknowledges it.
    pub fn take_due(&mut self, now: Instant) -> Vec<Outgoing> {
        let mut due = Vec::new();
        for entry in self.outgoing.iter_mut().take(SEND_WINDOW) {
            let ready = match entry.due_at() {
                None => true,
                Some(at) => at <= now,
            };
            if ready {
                entry.sent_at = Some(now);
                entry.attempts = entry.attempts.saturating_add(1);
                due.push(entry.clone());
            }
        }
        due
    }

    /// Put every in-flight packet back on the wire at the next opportunity.
    ///
    /// Used when the peer retransmits its session reset, which says it never saw
    /// our reply.
    pub fn mark_all_due(&mut self) {
        for entry in self.outgoing.iter_mut() {
            entry.sent_at = None;
        }
    }

    /// True once some packet has been transmitted [`MAX_ATTEMPTS`] times without
    /// being acknowledged: the peer is gone.
    pub fn is_exhausted(&self) -> bool {
        self.outgoing.iter().any(|o| o.attempts >= MAX_ATTEMPTS)
    }

    pub fn in_flight(&self) -> usize {
        self.outgoing.len()
    }
}

impl Default for ReliableSender {
    fn default() -> Self {
        Self::new()
    }
}

/// Orders inbound control payloads and tracks what still needs acknowledging.
pub struct ReliableReceiver {
    next_expected: u32,
    buffered: BTreeMap<u32, Vec<u8>>,
    pending_acks: VecDeque<u32>,
}

impl ReliableReceiver {
    /// Start expecting `next_expected`, having already accepted everything
    /// below it.
    pub fn new(next_expected: u32) -> Self {
        ReliableReceiver {
            next_expected,
            buffered: BTreeMap::new(),
            pending_acks: VecDeque::new(),
        }
    }

    /// Take a received control packet and return the payloads that are now
    /// deliverable, oldest first.
    ///
    /// A duplicate is acknowledged again — the peer only retransmits because it
    /// missed the first acknowledgement — but delivers nothing. A packet outside
    /// the window is neither acknowledged nor buffered, so the peer keeps it and
    /// retransmits it once the window has moved.
    pub fn accept(&mut self, packet_id: u32, payload: Vec<u8>) -> Vec<Vec<u8>> {
        if packet_id < self.next_expected {
            self.note_ack(packet_id);
            return Vec::new();
        }
        if packet_id.saturating_sub(self.next_expected) >= RECV_WINDOW {
            return Vec::new();
        }

        self.note_ack(packet_id);

        if packet_id > self.next_expected {
            if self.buffered.len() < MAX_BUFFERED {
                self.buffered.entry(packet_id).or_insert(payload);
            }
            return Vec::new();
        }

        let mut delivered = vec![payload];
        self.next_expected = self.next_expected.wrapping_add(1);
        while let Some(next) = self.buffered.remove(&self.next_expected) {
            delivered.push(next);
            self.next_expected = self.next_expected.wrapping_add(1);
        }
        delivered
    }

    /// Note an id that the peer must be told we have.
    fn note_ack(&mut self, packet_id: u32) {
        if self.pending_acks.contains(&packet_id) {
            return;
        }
        if self.pending_acks.len() >= MAX_PENDING_ACKS {
            self.pending_acks.pop_front();
        }
        self.pending_acks.push_back(packet_id);
    }

    /// Remove up to [`MAX_ACK_ARRAY`] ids to put in an outgoing ACK array.
    pub fn take_acks(&mut self) -> Vec<u32> {
        let n = self.pending_acks.len().min(MAX_ACK_ARRAY);
        self.pending_acks.drain(..n).collect()
    }

    pub fn has_pending_acks(&self) -> bool {
        !self.pending_acks.is_empty()
    }

    pub fn next_expected(&self) -> u32 {
        self.next_expected
    }
}

/// Split a TLS record stream into `P_CONTROL_V1`-sized fragments.
pub fn fragment(data: &[u8]) -> Vec<Vec<u8>> {
    data.chunks(MAX_CONTROL_PAYLOAD)
        .map(|c| c.to_vec())
        .collect()
}
