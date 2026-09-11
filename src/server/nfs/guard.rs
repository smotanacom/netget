//! A record-marking screen that sits between the network and `nfsserve`.
//!
//! # Why this exists
//!
//! `nfsserve` 0.10.2 sizes buffers from numbers a peer supplies and never checks them.
//! `rpcwire::read_fragment` does
//!
//! ```notrust
//! let length = (fragment_header & ((1 << 31) - 1)) as usize;
//! append_to.resize(append_to.len() + length, 0);
//! ```
//!
//! — a 31-bit length, so one four-byte record marker of `0xFFFF_FFFF` asks for a 2 GiB
//! zeroed allocation, and `xdr.rs` does the same with a full 32-bit length for every
//! `dirpath`, `filename`, file handle and WRITE payload. A record whose last-fragment bit is
//! clear appends into the same buffer indefinitely. All of it is reachable **before any
//! authentication** — a `MOUNTPROC3_MNT` is about forty bytes on the wire.
//!
//! None of that can be fixed in the crate from here, and `NFSTcpListener` owns its own
//! `accept()` loop, so there is no seam to hook. NetGet therefore binds the public listener
//! itself, starts `nfsserve` on a loopback-only ephemeral port, and screens the record-marking
//! headers on the way through. Bytes the screen refuses never reach the crate.
//!
//! # What it refuses
//!
//! One record at a time, by the numbers the peer *announces* — never by what it managed to
//! send, because a truncated request is indistinguishable from a complete one to whatever
//! reads it next:
//!
//! - a fragment longer than [`MAX_FRAGMENT_BYTES`],
//! - a record whose fragments total more than [`MAX_RECORD_BYTES`],
//! - a record built from more than [`MAX_FRAGMENTS_PER_RECORD`] fragments (a peer can
//!   otherwise hold a connection open forever with zero-length non-last fragments),
//! - a fragment body that stalls for longer than [`FRAGMENT_BODY_TIMEOUT`] once its length
//!   has been announced.
//!
//! A refusal is a real ONC RPC answer, not a silent close: the peer gets an accepted reply
//! carrying its own xid with `accept_stat = GARBAGE_ARGS` ("procedure can't decode params"),
//! which is the only thing RFC 5531 can say at the record layer, and the connection is then
//! closed. It is deliberately *not* a [`crate::utils::WireFailure`] string — the record layer
//! has no free-text field, and inventing one would be the leak that type exists to prevent.
//! Every refusal logs a stable `decision=fail_closed_*` tag; see [`Refusal::decision_tag`].
//!
//! # What it does not do
//!
//! The screen bounds framing, not semantics. Inside an admitted record the crate's XDR
//! decoder is still the unchecked one — but every length it reads now lives inside a record
//! the screen already sized, so the worst it can ask for is [`MAX_RECORD_BYTES`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Semaphore};

use crate::logging::emit::Log;

/// Largest single RPC fragment the screen will pass through.
///
/// `nfsserve`'s default `fsinfo` advertises `rtmax`/`wtmax` of 1 MiB, so a conforming client
/// may legitimately send a WRITE of that size plus RPC and NFS headers. This is twice that,
/// which leaves the legitimate case comfortable and still refuses the 2 GiB the wire format
/// allows.
pub const MAX_FRAGMENT_BYTES: usize = 2 * 1024 * 1024;

/// Largest complete RPC record (all fragments summed) the screen will pass through.
pub const MAX_RECORD_BYTES: usize = 2 * 1024 * 1024;

/// Most fragments one record may be split into. Real clients send exactly one.
pub const MAX_FRAGMENTS_PER_RECORD: usize = 64;

/// How long a peer may stall part-way through a fragment it has already announced.
pub const FRAGMENT_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Most connections screened at once. Each admitted record costs `nfsserve` up to
/// [`MAX_RECORD_BYTES`], so this is what turns "bounded per connection" into "bounded".
pub const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// Deepest path a MOUNT `dirpath` may have.
///
/// `NFSFileSystem::path_to_id`'s default implementation splits the dirpath on `/` and calls
/// `lookup()` once per component — and in this server every `lookup` is one LLM round-trip.
/// An unauthenticated MOUNT is otherwise an unbounded sequential chain of model calls that
/// saturates `--llm-max-concurrent` for every other server in the process. Real export paths
/// are two or three components deep.
pub const MAX_PATH_COMPONENTS: usize = 32;

/// Bytes read from the peer in one go while streaming an admitted fragment.
const RELAY_CHUNK_BYTES: usize = 64 * 1024;

/// How long a refused record is given to produce the four bytes of its xid, so the refusal
/// can be addressed to it. Nothing else of that record is ever read.
const XID_RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the screen refused a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// One fragment announced more than [`MAX_FRAGMENT_BYTES`].
    FragmentTooLarge { announced: usize },
    /// The fragments of one record summed to more than [`MAX_RECORD_BYTES`].
    RecordTooLarge { announced_total: usize },
    /// More than [`MAX_FRAGMENTS_PER_RECORD`] fragments in one record.
    TooManyFragments,
    /// The peer announced a fragment and then stopped sending it.
    BodyStalled,
}

impl Refusal {
    /// A stable tag an operator can grep for, in the shape `src/server/radius/` established.
    pub fn decision_tag(self) -> &'static str {
        match self {
            Self::FragmentTooLarge { .. } => "fail_closed_oversize_fragment",
            Self::RecordTooLarge { .. } => "fail_closed_oversize_record",
            Self::TooManyFragments => "fail_closed_fragment_flood",
            Self::BodyStalled => "fail_closed_fragment_stalled",
        }
    }

    /// Operator-facing detail. Never reaches the wire.
    pub fn describe(self) -> String {
        match self {
            Self::FragmentTooLarge { announced } => format!(
                "fragment announced {} bytes, limit {}",
                announced, MAX_FRAGMENT_BYTES
            ),
            Self::RecordTooLarge { announced_total } => format!(
                "record would reach {} bytes, limit {}",
                announced_total, MAX_RECORD_BYTES
            ),
            Self::TooManyFragments => {
                format!("record exceeded {} fragments", MAX_FRAGMENTS_PER_RECORD)
            }
            Self::BodyStalled => format!(
                "announced fragment stalled for more than {}s",
                FRAGMENT_BODY_TIMEOUT.as_secs()
            ),
        }
    }
}

/// One admitted fragment: how many bytes to relay, and whether it closes the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fragment {
    pub length: usize,
    pub is_last: bool,
}

/// Running state for the record currently being relayed on one connection.
#[derive(Debug, Default)]
pub struct RecordScreen {
    bytes_so_far: usize,
    fragments_so_far: usize,
}

impl RecordScreen {
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no fragment of the current record has been relayed yet, so the next four
    /// bytes on the wire are the call's xid.
    pub fn at_record_start(&self) -> bool {
        self.fragments_so_far == 0
    }

    /// Decide a fragment from its four-byte record marker, **before** anything is read or
    /// allocated for it. RFC 1057 §10: the top bit is "last fragment", the low 31 are the
    /// length.
    pub fn admit_fragment(&mut self, marker: u32) -> Result<Fragment, Refusal> {
        let is_last = marker & 0x8000_0000 != 0;
        let length = (marker & 0x7FFF_FFFF) as usize;

        if length > MAX_FRAGMENT_BYTES {
            return Err(Refusal::FragmentTooLarge { announced: length });
        }
        if self.fragments_so_far >= MAX_FRAGMENTS_PER_RECORD {
            return Err(Refusal::TooManyFragments);
        }
        let total = self.bytes_so_far.saturating_add(length);
        if total > MAX_RECORD_BYTES {
            return Err(Refusal::RecordTooLarge {
                announced_total: total,
            });
        }

        self.bytes_so_far = total;
        self.fragments_so_far += 1;
        if is_last {
            self.bytes_so_far = 0;
            self.fragments_so_far = 0;
        }
        Ok(Fragment { length, is_last })
    }
}

/// An ONC RPC accepted reply with `accept_stat = GARBAGE_ARGS`, record marker included.
///
/// RFC 5531: `xid`, `msg_type = REPLY (1)`, `reply_stat = MSG_ACCEPTED (0)`, an AUTH_NULL
/// verifier (flavor 0, length 0), then `accept_stat = GARBAGE_ARGS (4)`. Twenty-four bytes
/// behind a last-fragment marker.
pub fn garbage_args_reply(xid: u32) -> [u8; 28] {
    let mut out = [0u8; 28];
    out[0..4].copy_from_slice(&(0x8000_0000u32 | 24).to_be_bytes());
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    out[8..12].copy_from_slice(&1u32.to_be_bytes()); // REPLY
    out[12..16].copy_from_slice(&0u32.to_be_bytes()); // MSG_ACCEPTED
    out[16..20].copy_from_slice(&0u32.to_be_bytes()); // verf.flavor = AUTH_NULL
    out[20..24].copy_from_slice(&0u32.to_be_bytes()); // verf.length = 0
    out[24..28].copy_from_slice(&4u32.to_be_bytes()); // accept_stat = GARBAGE_ARGS
    out
}

/// Accept on NetGet's own listener and relay each connection to `nfsserve`, screened.
///
/// Runs until the task is aborted (`stop_server` does that through
/// `AppState::register_server_task`).
pub async fn serve_screened(
    listener: TcpListener,
    backend: SocketAddr,
    app_state: Arc<crate::state::app_state::AppState>,
    server_id: crate::state::ServerId,
    status_tx: mpsc::UnboundedSender<String>,
) {
    let slots = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                Log::new(Some(&status_tx)).error(format!("NFS accept failed: {}", e));
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let permit = match Arc::clone(&slots).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                Log::new(Some(&status_tx)).warn(format!(
                    "NFS connection from {} refused decision=fail_closed_connection_cap \
                     (limit {} concurrent)",
                    peer, MAX_CONCURRENT_CONNECTIONS
                ));
                drop(client);
                continue;
            }
        };

        let status_tx = status_tx.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit;
            screen_connection(client, backend, peer.to_string(), status_tx).await;
        });
        // Registering per-connection tasks is what lets `stop_server` cancel traffic that is
        // already in flight, not just release the listening socket.
        app_state.register_server_task(server_id, handle).await;
    }
}

/// How relaying one admitted fragment ended.
enum Relayed {
    /// Every announced byte reached the backend.
    Complete,
    /// The connection closed, reset, or the backend went away. Ordinary, not a refusal.
    PeerGone,
    /// The peer announced bytes and then stopped sending them.
    Stalled,
}

/// Relay one client connection to `nfsserve`, screening every record marker on the way in.
async fn screen_connection(
    client: TcpStream,
    backend: SocketAddr,
    peer: String,
    status_tx: mpsc::UnboundedSender<String>,
) {
    let log = Log::new(Some(&status_tx));
    let _ = client.set_nodelay(true);

    let upstream = match TcpStream::connect(backend).await {
        Ok(s) => s,
        Err(e) => {
            log.error(format!(
                "NFS could not reach its own backend for {}: {}",
                peer, e
            ));
            return;
        }
    };
    let _ = upstream.set_nodelay(true);

    let (mut from_peer, to_peer) = client.into_split();
    let (mut from_backend, mut to_backend) = upstream.into_split();
    // Shared because the screen writes its refusal to the same half the downstream relay
    // uses. The lock is held only for one `write_all`.
    let to_peer = Arc::new(Mutex::new(to_peer));

    let downstream_out = Arc::clone(&to_peer);
    let downstream = tokio::spawn(async move {
        let mut buf = vec![0u8; RELAY_CHUNK_BYTES];
        loop {
            match from_backend.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut out = downstream_out.lock().await;
                    if out.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut screen = RecordScreen::new();
    // The xid of the record in flight, captured from the first four bytes of its first
    // fragment, so a refusal later in the same record is still addressed to the right call.
    let mut record_xid: u32 = 0;
    let mut buf = vec![0u8; RELAY_CHUNK_BYTES];

    let outcome: Option<Refusal> = loop {
        let mut marker = [0u8; 4];
        if from_peer.read_exact(&mut marker).await.is_err() {
            // A closed or reset connection between records is ordinary.
            break None;
        }
        let marker = u32::from_be_bytes(marker);

        let at_start = screen.at_record_start();
        if at_start {
            record_xid = 0;
        }
        let fragment = match screen.admit_fragment(marker) {
            Ok(f) => f,
            Err(refusal) => {
                // Recover the xid so the refusal is addressed to the call that caused it.
                // Four bytes, briefly: the announced body is never read, only the identifier
                // that has to appear in the reply for a client to match it to anything.
                if at_start {
                    let mut xid = [0u8; 4];
                    if let Ok(Ok(_)) =
                        tokio::time::timeout(XID_RECOVERY_TIMEOUT, from_peer.read_exact(&mut xid))
                            .await
                    {
                        record_xid = u32::from_be_bytes(xid);
                    }
                }
                break Some(refusal);
            }
        };

        if to_backend.write_all(&marker.to_be_bytes()).await.is_err() {
            break None;
        }

        let mut remaining = fragment.length;
        let mut captured = 0usize;
        let relayed = loop {
            if remaining == 0 {
                break Relayed::Complete;
            }
            let want = remaining.min(buf.len());
            let read =
                match tokio::time::timeout(FRAGMENT_BODY_TIMEOUT, from_peer.read(&mut buf[..want]))
                    .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) => break Relayed::PeerGone,
                    Ok(Ok(n)) => n,
                    Err(_) => break Relayed::Stalled,
                };

            // Capture the xid out of the first four bytes of the record.
            if at_start && captured < 4 {
                let take = read.min(4 - captured);
                for &b in &buf[..take] {
                    record_xid = (record_xid << 8) | b as u32;
                    captured += 1;
                }
            }

            if to_backend.write_all(&buf[..read]).await.is_err() {
                break Relayed::PeerGone;
            }
            remaining -= read;
        };

        // A first fragment shorter than four bytes leaves the xid partly filled; align what
        // was captured into the high bytes so the reply at least echoes something derived
        // from the call rather than a number shifted by an arbitrary amount.
        if at_start && captured > 0 && captured < 4 {
            record_xid <<= 8 * (4 - captured) as u32;
        }

        match relayed {
            Relayed::Complete => {}
            Relayed::PeerGone => break None,
            Relayed::Stalled => break Some(Refusal::BodyStalled),
        }
    };

    if let Some(refusal) = outcome {
        log.error(format!(
            "NFS record from {} refused decision={} ({}) - answered accept_stat=GARBAGE_ARGS",
            peer,
            refusal.decision_tag(),
            refusal.describe()
        ));
        let reply = garbage_args_reply(record_xid);
        let mut out = to_peer.lock().await;
        let _ = out.write_all(&reply).await;
        let _ = out.flush().await;
        let _ = out.shutdown().await;
    }

    // Dropping the backend write half closes that direction; the relay ends with it.
    drop(to_backend);
    downstream.abort();
}
