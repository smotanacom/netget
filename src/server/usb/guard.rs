//! A USB/IP message screen that sits between the network and the `usbip` crate.
//!
//! # Why this exists
//!
//! `usbip` 0.9.0 sizes two buffers from numbers the peer supplies and checks neither.
//! `UsbIpCommand::read_from_socket` reads a 48-byte `USBIP_CMD_SUBMIT` header and then does
//!
//! ```notrust
//! let mut data = vec![0; transfer_buffer_length as usize];
//! socket.read_exact(&mut data).await?;
//! ```
//!
//! — so a header declaring `transfer_buffer_length = 0xFFFFFFFF` allocates and zero-fills
//! 4 GiB **before one payload byte arrives**. Immediately after it does the same with the
//! isochronous descriptor array:
//!
//! ```notrust
//! let mut result = vec![0; 16 * number_of_packets as usize];
//! ```
//!
//! which reaches ~68 GiB on a 64-bit host and wraps on a 32-bit one.
//!
//! USB/IP has **no authentication of any kind** — `OP_REQ_IMPORT` carries a bus id and nothing
//! else — and every NetGet USB server spawns its USB/IP session task *before* it makes the
//! attach LLM call, so both allocations are reachable pre-auth and pre-model, from 48 bytes.
//!
//! # The seam
//!
//! `usbip::handler` is generic over `T: AsyncReadExt + AsyncWriteExt + Unpin`, so unlike
//! `nfsserve` (see `src/server/nfs/guard.rs`, the precedent this follows) no second listener
//! is needed: NetGet keeps the real socket, hands the crate an in-memory
//! [`tokio::io::duplex`] pipe, and copies only admitted messages into it. Bytes the screen
//! refuses never reach the crate.
//!
//! # What it refuses
//!
//! One message at a time, by the numbers the peer *announced*, before any arithmetic and
//! before anything is read or allocated for them:
//!
//! - a `USBIP_CMD_SUBMIT` declaring more than [`MAX_TRANSFER_BUFFER_BYTES`],
//! - a `USBIP_CMD_SUBMIT` declaring more than [`MAX_ISO_PACKETS`] isochronous packets,
//! - a command word that is none of the four USB/IP defines,
//! - a `direction` that is not a single bit, or an operation-code message with a non-zero
//!   `status` -- the crate only `debug_assert!`s both, so either is a panic from the wire in a
//!   debug or test build, swallowed by `tokio::spawn` and leaving the peer hung,
//! - a payload the peer announced and then stopped sending ([`URB_BODY_TIMEOUT`]).
//!
//! **A refusal closes the connection rather than answering it, and that is a real limitation
//! rather than the only option.** `USBIP_RET_SUBMIT` has a `status` field that could carry
//! `-EINVAL`, but a well-formed reply also has to echo the `seqnum` and `devid` of a URB the
//! crate never saw, and the crate — not the screen — owns the reply stream it is multiplexed
//! into. Writing one from underneath would interleave with whatever the crate is sending. So
//! the screen closes, and the operator gets the reason from the log: every refusal is an ERROR
//! carrying a stable `decision=fail_closed_*` tag, in the shape `src/server/radius/` set.
//!
//! # What it does not do
//!
//! The screen bounds message framing, not USB semantics. An admitted URB still goes to the
//! protocol's own `UsbInterfaceHandler`, which decides whether the endpoint, the setup packet
//! and the payload make sense.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Duration;

use crate::logging::emit::Log;

/// Largest `transfer_buffer_length` a `USBIP_CMD_SUBMIT` may declare.
///
/// **This is a NetGet policy choice, not a figure from the USB/IP specification** — the
/// protocol's field is a bare `u32` and names no ceiling. It is chosen from what the six
/// devices NetGet emulates actually transfer: HID keyboard and mouse reports are 4-8 bytes,
/// CTAPHID frames are 64, CCID bulk messages are a few hundred, and the largest by far is mass
/// storage, where a Bulk-Only Transport data phase is one SCSI transfer — Linux's `usb-storage`
/// caps those well below a megabyte, and NetGet's own MSC handler serves 512-byte sectors.
///
/// 1 MiB therefore leaves every legitimate transfer these devices can make comfortable, while
/// refusing the 4 GiB the wire format allows. It is also what a protocol declares to the model
/// and the operator through `ProtocolMetadataV2::max_inbound_bytes`.
pub const MAX_TRANSFER_BUFFER_BYTES: usize = 1024 * 1024;

/// Largest `number_of_packets` a `USBIP_CMD_SUBMIT` may declare for an isochronous transfer.
///
/// Unlike the constant above this one **is** the number the other implementations use: the
/// Linux kernel's own USB/IP stub defines `USBIP_MAX_ISO_PACKETS` as 1024 in
/// `drivers/usb/usbip/usbip_common.h` and rejects a larger `number_of_packets` on the submit
/// path in `stub_rx.c`, for the same reason this exists.
///
/// It is generous here to the point of being theoretical: no device NetGet emulates declares an
/// isochronous endpoint at all, so the only values a working session produces are the two
/// exempt ones, `0` and `0xFFFFFFFF`. Matching the kernel rather than refusing every non-exempt
/// value keeps the screen from being the thing that breaks a future isochronous device.
///
/// 1024 packets is 16 KiB of descriptors, so `16 * number_of_packets` cannot overflow once the
/// bound has been applied — it is still computed with `checked_mul`, because the point of the
/// bound is that the arithmetic happens *after* it.
pub const MAX_ISO_PACKETS: u32 = 1024;

/// How long a peer may stall part-way through a payload it has already announced.
///
/// Only the payload is bounded, never the wait for the *next* message: a USB host that has
/// attached a device and is asking nothing of it is idle by design, and closing that connection
/// would be the live-transfer eviction the project `CLAUDE.md` records TFTP learning about.
const URB_BODY_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes read from the peer in one go while streaming an admitted payload.
///
/// The screen never allocates from the length the peer declared — it allocates this, once per
/// connection, and streams.
const RELAY_CHUNK_BYTES: usize = 64 * 1024;

/// Size of the in-memory pipe standing in for the socket the crate thinks it has.
///
/// Both directions are drained concurrently, so this only has to be big enough that a small
/// message does not cost several wakeups; a payload larger than it simply streams through.
const DUPLEX_BUFFER_BYTES: usize = 64 * 1024;

/// How long the crate-side session is given to wind up after the peer is gone.
///
/// Closing the write half of the pipe makes the crate's next read return EOF, which ends
/// `usbip::handler`. A handler parked inside a long `handle_urb` will not see that, so the
/// wait is bounded and then the task is aborted.
const SESSION_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// USB/IP protocol version, as carried by the operation-code messages.
const USBIP_VERSION: u16 = 0x0111;

const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REQ_IMPORT: u16 = 0x8003;
const USBIP_CMD_SUBMIT: u16 = 0x0001;
const USBIP_CMD_UNLINK: u16 = 0x0002;

/// Bytes after the four-byte version/command word, per message kind.
const OP_REQ_DEVLIST_TAIL: usize = 4; // status
const OP_REQ_IMPORT_TAIL: usize = 36; // status + busid[32]
const CMD_TAIL: usize = 44; // both CMD_SUBMIT and CMD_UNLINK are 48 bytes total

/// Offsets within the 48-byte `USBIP_CMD_SUBMIT` header, counted from the command word.
const OFF_DIRECTION: usize = 12;
const OFF_TRANSFER_BUFFER_LENGTH: usize = 24;
const OFF_NUMBER_OF_PACKETS: usize = 32;

/// Offset of `status`, the first field of both operation-code messages.
const OFF_STATUS: usize = 4;

/// `direction` value meaning host-to-device, i.e. the only case carrying a payload.
const DIRECTION_OUT: u32 = 0;

/// `number_of_packets` values that mean "not an isochronous transfer".
///
/// The kernel documentation specifies `0xFFFFFFFF` for every non-ISO URB; the implementation
/// sends `0`. Both are in the wild, so both are exempt — the crate makes the same allowance.
const NON_ISO_PACKET_COUNTS: [u32; 2] = [0, 0xFFFF_FFFF];

/// Why the screen refused a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The version word is neither 0 nor [`USBIP_VERSION`].
    UnknownVersion { version: u16 },
    /// The command word is none of the four USB/IP defines.
    UnknownCommand { command: u16 },
    /// `transfer_buffer_length` exceeded [`MAX_TRANSFER_BUFFER_BYTES`].
    TransferBufferTooLarge { announced: u32 },
    /// `number_of_packets` exceeded [`MAX_ISO_PACKETS`].
    TooManyIsoPackets { announced: u32 },
    /// `16 * number_of_packets` does not fit in a `usize` on this target.
    IsoDescriptorOverflow { announced: u32 },
    /// `direction` is neither 0 (OUT) nor 1 (IN).
    ///
    /// The crate `debug_assert!`s this, so in a debug or test build a peer that sends 2 panics
    /// the session task from the wire, pre-auth — and `tokio::spawn` swallows the panic.
    InvalidDirection { direction: u32 },
    /// An operation-code message declared a non-zero `status`, which the crate also only
    /// `debug_assert!`s. A real host sends zero.
    NonZeroStatus { status: u32 },
    /// The peer announced a payload and then stopped sending it.
    BodyStalled,
}

impl Refusal {
    /// A stable tag an operator can grep for, in the shape `src/server/radius/` established.
    pub fn decision_tag(self) -> &'static str {
        match self {
            Self::UnknownVersion { .. } => "fail_closed_unknown_version",
            Self::UnknownCommand { .. } => "fail_closed_unknown_command",
            Self::TransferBufferTooLarge { .. } => "fail_closed_oversized_urb",
            Self::TooManyIsoPackets { .. } | Self::IsoDescriptorOverflow { .. } => {
                "fail_closed_oversized_iso"
            }
            Self::InvalidDirection { .. } => "fail_closed_invalid_direction",
            Self::NonZeroStatus { .. } => "fail_closed_nonzero_status",
            Self::BodyStalled => "fail_closed_urb_stalled",
        }
    }

    /// Operator-facing detail. Never reaches the wire.
    pub fn describe(self) -> String {
        match self {
            Self::UnknownVersion { version } => {
                format!(
                    "version {:#06X} is neither 0 nor {:#06X}",
                    version, USBIP_VERSION
                )
            }
            Self::UnknownCommand { command } => format!("command {:#06X} is not USB/IP", command),
            Self::TransferBufferTooLarge { announced } => format!(
                "transfer_buffer_length announced {} bytes, limit {}",
                announced, MAX_TRANSFER_BUFFER_BYTES
            ),
            Self::TooManyIsoPackets { announced } => format!(
                "number_of_packets announced {}, limit {}",
                announced, MAX_ISO_PACKETS
            ),
            Self::IsoDescriptorOverflow { announced } => format!(
                "16 * number_of_packets ({}) does not fit in usize",
                announced
            ),
            Self::InvalidDirection { direction } => {
                format!("direction {} is neither OUT (0) nor IN (1)", direction)
            }
            Self::NonZeroStatus { status } => format!("status {} is not zero", status),
            Self::BodyStalled => format!(
                "announced payload stalled for more than {}s",
                URB_BODY_TIMEOUT.as_secs()
            ),
        }
    }
}

/// Aborts the task it holds when dropped.
///
/// `tokio::spawn` does not cascade cancellation, and every caller of
/// [`run_guarded_usbip`] drives it from a `select!` that aborts it when the connection ends.
/// Without this, the crate-side session and the reply relay would outlive that abort still
/// holding the socket — the detached-task defect the project `CLAUDE.md` describes.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How relaying one message ended.
enum Relayed {
    /// The whole message reached the crate.
    Complete,
    /// The peer closed, reset, or the crate-side pipe went away. Ordinary, not a refusal.
    PeerGone,
}

/// Run a USB/IP session for `stream` with every inbound message screened first.
///
/// Drop-in for `usbip::handler(&mut stream, server)`: it owns the socket, returns when the
/// session ends, and reports the crate's own error where there is one. `device` names the
/// protocol for the log (`"USB MSC"`, `"USB keyboard"`, …) and `peer` identifies the
/// connection.
pub async fn run_guarded_usbip(
    stream: TcpStream,
    server: Arc<usbip::UsbIpServer>,
    device: &str,
    peer: String,
    status_tx: UnboundedSender<String>,
) -> std::io::Result<()> {
    let log = Log::new(Some(&status_tx));
    let _ = stream.set_nodelay(true);

    let (guard_side, mut crate_side) = tokio::io::duplex(DUPLEX_BUFFER_BYTES);
    let mut session = AbortOnDrop(tokio::spawn(async move {
        usbip::handler(&mut crate_side, server).await
    }));

    let (mut from_peer, mut to_peer) = stream.into_split();
    let (mut from_crate, mut to_crate) = tokio::io::split(guard_side);

    // The crate's replies have to drain concurrently with the screen's inbound loop: the crate
    // reads the next message only after it has written the previous answer, so a screen that
    // relayed in one direction at a time would deadlock on the first URB.
    let replies = AbortOnDrop(tokio::spawn(async move {
        let _ = tokio::io::copy(&mut from_crate, &mut to_peer).await;
        let _ = to_peer.shutdown().await;
    }));

    let mut chunk = vec![0u8; RELAY_CHUNK_BYTES];
    let refusal = loop {
        match relay_one_message(&mut from_peer, &mut to_crate, &mut chunk).await {
            Ok(Relayed::Complete) => {}
            Ok(Relayed::PeerGone) => break None,
            Err(refusal) => break Some(refusal),
        }
    };

    if let Some(refusal) = refusal {
        log.error(format!(
            "{} refused a USB/IP message from {} decision={} ({}); connection closed",
            device,
            peer,
            refusal.decision_tag(),
            refusal.describe()
        ));
    }

    // Closing the write half is what tells the crate its peer is gone.
    drop(to_crate);

    let outcome = match tokio::time::timeout(SESSION_SHUTDOWN_GRACE, &mut session.0).await {
        Ok(Ok(result)) => result,
        // A panic or an abort inside the session task is not something the caller can act on
        // differently from a closed connection, and `src/panic_log.rs` has already recorded it.
        Ok(Err(_)) => Ok(()),
        Err(_) => Ok(()),
    };

    // Once the crate's end of the pipe is gone the reply relay sees EOF and finishes, having
    // flushed whatever the crate managed to answer. Bounded, because an aborted session task
    // drops its end at a moment tokio chooses rather than immediately.
    let mut replies = replies;
    let _ = tokio::time::timeout(SESSION_SHUTDOWN_GRACE, &mut replies.0).await;

    // `session` and `replies` abort on drop, so a handler still parked in `handle_urb` and a
    // relay still holding the socket both end here rather than outliving the connection.
    drop(session);
    drop(replies);

    if refusal.is_some() {
        return Err(std::io::Error::other(
            "USB/IP message refused by the screen",
        ));
    }
    outcome
}

/// Read one whole USB/IP message from the peer and relay it verbatim to the crate.
///
/// Every bound is applied to the value the peer *declared*, before the bytes it describes are
/// read and before any arithmetic is done on it.
async fn relay_one_message<R, W>(
    from_peer: &mut R,
    to_crate: &mut W,
    chunk: &mut [u8],
) -> Result<Relayed, Refusal>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut head = [0u8; 4];
    if from_peer.read_exact(&mut head).await.is_err() {
        return Ok(Relayed::PeerGone);
    }
    let version = u16::from_be_bytes([head[0], head[1]]);
    let command = u16::from_be_bytes([head[2], head[3]]);

    // Operation-code messages carry the version; the command messages leave it zero, because
    // those four bytes are the top half of a `u32` command field.
    if version != 0 && version != USBIP_VERSION {
        return Err(Refusal::UnknownVersion { version });
    }

    let tail_len = match command {
        OP_REQ_DEVLIST => OP_REQ_DEVLIST_TAIL,
        OP_REQ_IMPORT => OP_REQ_IMPORT_TAIL,
        USBIP_CMD_SUBMIT | USBIP_CMD_UNLINK => CMD_TAIL,
        // The crate would error on this too, but then it is the crate deciding. The screen
        // decides, so nothing unrecognised is forwarded in the first place.
        _ => return Err(Refusal::UnknownCommand { command }),
    };

    let mut message = Vec::with_capacity(4 + tail_len);
    message.extend_from_slice(&head);
    message.resize(4 + tail_len, 0);
    if from_peer.read_exact(&mut message[4..]).await.is_err() {
        return Ok(Relayed::PeerGone);
    }

    // Both command messages carry `direction`, and the crate only `debug_assert!`s that it is
    // a single bit -- a panic from the wire in any debug or test build.
    if command == USBIP_CMD_SUBMIT || command == USBIP_CMD_UNLINK {
        let direction = be_u32(&message, OFF_DIRECTION);
        if direction > 1 {
            return Err(Refusal::InvalidDirection { direction });
        }
    } else {
        // Same reasoning for the operation-code messages, whose `status` the crate reads and
        // then only `debug_assert!`s to be zero.
        let status = be_u32(&message, OFF_STATUS);
        if status != 0 {
            return Err(Refusal::NonZeroStatus { status });
        }
    }

    // How many further bytes this message carries: none, unless it is a SUBMIT.
    let (payload_len, iso_len) = if command == USBIP_CMD_SUBMIT {
        submit_trailer_lengths(&message)?
    } else {
        (0, 0)
    };

    if to_crate.write_all(&message).await.is_err() {
        return Ok(Relayed::PeerGone);
    }

    stream_trailer(from_peer, to_crate, chunk, payload_len + iso_len).await
}

/// Read a big-endian `u32` field out of a message the caller has already read in full.
fn be_u32(message: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([
        message[off],
        message[off + 1],
        message[off + 2],
        message[off + 3],
    ])
}

/// Decide a `USBIP_CMD_SUBMIT` from its 48-byte header: `(payload bytes, ISO descriptor bytes)`.
fn submit_trailer_lengths(header: &[u8]) -> Result<(usize, usize), Refusal> {
    let direction = be_u32(header, OFF_DIRECTION);
    let transfer_buffer_length = be_u32(header, OFF_TRANSFER_BUFFER_LENGTH);
    let number_of_packets = be_u32(header, OFF_NUMBER_OF_PACKETS);

    // Bound the declared length, whichever direction it is for: the crate allocates from it
    // only for OUT transfers, but an IN transfer's declaration is what sizes the *reply*
    // buffer, and neither number should reach the crate unchecked.
    if u64::from(transfer_buffer_length) > MAX_TRANSFER_BUFFER_BYTES as u64 {
        return Err(Refusal::TransferBufferTooLarge {
            announced: transfer_buffer_length,
        });
    }
    // An IN URB asks the device for data and carries none, so nothing follows the header.
    let payload_len = if direction == DIRECTION_OUT {
        transfer_buffer_length as usize
    } else {
        0
    };

    let iso_len = if NON_ISO_PACKET_COUNTS.contains(&number_of_packets) {
        0
    } else {
        if number_of_packets > MAX_ISO_PACKETS {
            return Err(Refusal::TooManyIsoPackets {
                announced: number_of_packets,
            });
        }
        // Only now, with the count bounded, is it multiplied out — and still checked, because
        // the bound is what makes the arithmetic safe rather than the arithmetic itself.
        usize::try_from(number_of_packets)
            .ok()
            .and_then(|n| n.checked_mul(16))
            .ok_or(Refusal::IsoDescriptorOverflow {
                announced: number_of_packets,
            })?
    };

    Ok((payload_len, iso_len))
}

/// Stream `remaining` admitted bytes from the peer to the crate, `chunk` at a time.
async fn stream_trailer<R, W>(
    from_peer: &mut R,
    to_crate: &mut W,
    chunk: &mut [u8],
    mut remaining: usize,
) -> Result<Relayed, Refusal>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        let read = match tokio::time::timeout(URB_BODY_TIMEOUT, from_peer.read(&mut chunk[..want]))
            .await
        {
            Ok(Ok(0)) | Ok(Err(_)) => return Ok(Relayed::PeerGone),
            Ok(Ok(n)) => n,
            Err(_) => return Err(Refusal::BodyStalled),
        };
        if to_crate.write_all(&chunk[..read]).await.is_err() {
            return Ok(Relayed::PeerGone);
        }
        remaining -= read;
    }
    Ok(Relayed::Complete)
}
