//! Where CAN frames actually travel — and the one platform on which they can.
//!
//! # `AF_CAN` is a Linux address family and nothing else
//!
//! SocketCAN is not a library that could be ported; it is a socket family implemented in the
//! Linux kernel, alongside a queueing discipline, a per-interface error-confinement state machine
//! and the `can_frame`/`canfd_frame` structs the network stack passes around. macOS has no
//! equivalent — no `AF_CAN`, no `can0` interface, no `cansend`. Neither does Windows. A CAN
//! adapter on those platforms is reached through a vendor's userspace driver over USB, which
//! speaks a proprietary protocol and shares nothing with this code.
//!
//! So the Linux half of this file is written and **has never been compiled or run**, and
//! [`socketcan_unavailable`] is the honest answer everywhere else. Refusing is the point:
//! *"hiding a protocol is not the same as refusing to start it — hidden, the model never learns
//! why; refused, the user gets `ServerStatus::Error` with the reason."*
//!
//! # The UDP test transport
//!
//! [`TransportKind::Udp`] carries the exact octets an `AF_CAN` socket would — a 16-octet
//! `struct can_frame` or a 72-octet `struct canfd_frame` — as a UDP datagram payload. No real CAN
//! node speaks it, and no assertion made over it says anything about SocketCAN. What it does buy
//! is the entire rest of the protocol: the codec, the events, handler and script dispatch, the
//! model call, the action executor and the frame builder all run, unprivileged, on a machine with
//! no CAN stack. It is a declared startup parameter rather than a test-file convention, the same
//! accommodation `lldp` makes for its raw-Ethernet transport.

use crate::server::can::frame::CanFrame;
use anyhow::{anyhow, Result};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::UdpSocket;

/// The single source of the text explaining why SocketCAN cannot run here.
///
/// A `const` so the runtime error, the protocol description, the module documentation and the
/// tests all quote the same words — the arrangement `bluetooth_ble_beacon` uses for the same
/// reason.
pub const UNSUPPORTED_PLATFORM_MESSAGE: &str = concat!(
    "SocketCAN requires the Linux kernel's AF_CAN address family, which does not exist on macOS ",
    "or Windows: there is no can0 interface, no can_frame struct in the network stack and no ",
    "socket family to bind. A CAN adapter on those platforms is reached through a vendor's ",
    "userspace USB driver, which shares nothing with this code. Run the can protocol on Linux ",
    "-- `sudo modprobe vcan && sudo ip link add dev vcan0 type vcan && sudo ip link set up ",
    "vcan0` gives a fully functional virtual CAN bus with no hardware at all -- or start it with ",
    "startup_params {\"transport\": \"udp\"} for the unprivileged test transport, which carries ",
    "SocketCAN frame structs in datagrams and which no real CAN node speaks."
);

/// Which way frames travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// A real `AF_CAN` socket on a Linux CAN interface. The default, and Linux-only.
    SocketCan,
    /// SocketCAN frame structs inside UDP datagrams. Testing only.
    Udp,
}

impl TransportKind {
    /// Parse the `transport` startup parameter.
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value {
            None | Some("socketcan") => Ok(TransportKind::SocketCan),
            Some("udp") => Ok(TransportKind::Udp),
            Some(other) => Err(anyhow!(
                "transport must be \"socketcan\" (a real AF_CAN socket, Linux only) or \"udp\" \
                 (the test transport carrying SocketCAN frame structs in datagrams), got \
                 '{other}'"
            )),
        }
    }
}

/// The refusal, as an error, for every platform without `AF_CAN`.
///
/// Public and separate from `spawn` so a test can assert the exact refusal without starting
/// anything — on macOS this is the only part of the SocketCAN path that is reachable at all.
pub fn socketcan_unavailable<T>() -> Result<T> {
    Err(anyhow!(UNSUPPORTED_PLATFORM_MESSAGE))
}

/// True when this build can open an `AF_CAN` socket.
pub const fn socketcan_supported() -> bool {
    cfg!(target_os = "linux")
}

/// Where a frame goes once something has decided to send it.
///
/// Cloned into each per-frame task rather than shared behind a lock: a channel handle and an
/// `Arc<UdpSocket>` are both cheap to clone, and nothing here may hold a guard across the `.await`
/// that does the I/O.
#[derive(Clone)]
pub enum FrameSink {
    /// Hand the wire bytes to the blocking `AF_CAN` transmit thread.
    SocketCan(std::sync::mpsc::Sender<Vec<u8>>),
    /// Send the wire bytes as a UDP datagram, to the configured peer or to whoever spoke last.
    Udp {
        socket: Arc<UdpSocket>,
        configured_peer: Option<SocketAddr>,
        last_peer: Arc<Mutex<Option<SocketAddr>>>,
    },
}

impl FrameSink {
    /// Transmit one frame. Returns the number of octets handed to the transport.
    pub async fn send(&self, frame: &CanFrame) -> Result<usize> {
        let bytes = frame.to_wire_bytes()?;
        match self {
            FrameSink::SocketCan(tx) => {
                let len = bytes.len();
                tx.send(bytes)
                    .map_err(|_| anyhow!("the CAN transmit thread has gone away"))?;
                Ok(len)
            }
            FrameSink::Udp {
                socket,
                configured_peer,
                last_peer,
            } => {
                // The guard is taken and dropped before the await: never hold a lock across I/O.
                let peer = match configured_peer {
                    Some(peer) => Some(*peer),
                    None => *last_peer.lock().expect("can peer mutex poisoned"),
                };
                let peer = peer.ok_or_else(|| {
                    anyhow!(
                        "no UDP peer to transmit to: nothing has been received yet and no \
                         udp_peer startup parameter was given"
                    )
                })?;
                Ok(socket.send_to(&bytes, peer).await?)
            }
        }
    }
}

/// The `AF_CAN` transport.
///
/// **Never compiled, never executed.** Written against the `socketcan` 3.6 sources rather than
/// from memory — `src/socket.rs` for the blocking `Socket`/`SocketOptions` traits and
/// `src/frame.rs` for `CanAnyFrame` and its constructors — but the machine that wrote it has no
/// Linux target installed and `AF_CAN` cannot be linked here regardless. Treat the first run on
/// Linux as bring-up; `CLAUDE.md` says so too, and says what would move the rating.
#[cfg(target_os = "linux")]
pub mod linux {
    use super::CanFrame;
    use anyhow::{anyhow, Context, Result};
    use socketcan::{
        CanAnyFrame, CanDataFrame, CanFdFrame, CanFdSocket, CanRemoteFrame, EmbeddedFrame,
        ExtendedId, Frame, Id, Socket, SocketOptions, StandardId,
    };

    /// Open one `AF_CAN` socket on `interface`, ready to receive every frame including errors.
    ///
    /// Two socket options matter and neither is the default:
    ///
    /// * **`set_error_filter_accept_all`** — a raw CAN socket's error mask starts at zero, so
    ///   without this the kernel delivers no error frames at all and `can_error_frame` and
    ///   `can_bus_state_changed` could never fire. An event declared and never emitted is a
    ///   defect this repository has shipped in bulk.
    /// * **`recv_own_msgs` stays off** (the default) — with it on, every frame this server
    ///   transmits comes straight back as a received frame, raising an event, producing another
    ///   frame, forever. `lldp` guards the same loop by filtering on its own source address;
    ///   here the kernel does it.
    pub fn open(interface: &str) -> Result<CanFdSocket> {
        let socket = CanFdSocket::open(interface).with_context(|| {
            format!(
                "failed to open an AF_CAN socket on '{interface}'. The interface must exist and \
                 be up: `ip link show {interface}`, then `sudo ip link set up {interface}`. For a \
                 bus with no hardware, `sudo modprobe vcan && sudo ip link add dev vcan0 type \
                 vcan && sudo ip link set up vcan0`."
            )
        })?;

        socket.set_error_filter_accept_all().with_context(|| {
            format!(
                "failed to enable error-frame delivery on '{interface}'; without it the bus \
                 state can never be reported"
            )
        })?;

        Ok(socket)
    }

    /// The identifier in the form `embedded_can` wants.
    fn hal_id(frame: &CanFrame) -> Result<Id> {
        if frame.extended {
            ExtendedId::new(frame.id)
                .map(Id::Extended)
                .ok_or_else(|| anyhow!("0x{:X} is not a valid 29-bit identifier", frame.id))
        } else {
            u16::try_from(frame.id)
                .ok()
                .and_then(StandardId::new)
                .map(Id::Standard)
                .ok_or_else(|| anyhow!("0x{:X} is not a valid 11-bit identifier", frame.id))
        }
    }

    /// Convert a NetGet frame into the one `socketcan` will transmit.
    ///
    /// Deliberately cannot produce a `CanAnyFrame::Error`. NetGet never emits an error frame; see
    /// this protocol's `CLAUDE.md` for why that is a safety property and not an omission.
    pub fn to_socketcan(frame: &CanFrame) -> Result<CanAnyFrame> {
        frame.validate()?;
        let id = hal_id(frame)?;

        if frame.rtr {
            CanRemoteFrame::new_remote(id, frame.rtr_dlc as usize)
                .map(CanAnyFrame::Remote)
                .ok_or_else(|| {
                    anyhow!(
                        "socketcan rejected a remote frame with dlc {}",
                        frame.rtr_dlc
                    )
                })
        } else if frame.fd {
            let mut fd = CanFdFrame::new(id, &frame.data).ok_or_else(|| {
                anyhow!(
                    "socketcan rejected a {}-byte CAN FD payload",
                    frame.data.len()
                )
            })?;
            fd.set_brs(frame.brs);
            Ok(CanAnyFrame::Fd(fd))
        } else {
            CanDataFrame::new(id, &frame.data)
                .map(CanAnyFrame::Normal)
                .ok_or_else(|| {
                    anyhow!(
                        "socketcan rejected a {}-byte classic payload",
                        frame.data.len()
                    )
                })
        }
    }

    /// Convert a received frame into NetGet's representation.
    ///
    /// All four variants go through [`CanFrame::from_id_word`], which is pure and is where the
    /// EFF/RTR/ERR decoding is proven — so the only thing this function contributes is picking
    /// the FD flags out of the FD variant.
    pub fn from_socketcan(any: &CanAnyFrame) -> CanFrame {
        match any {
            CanAnyFrame::Normal(f) => {
                CanFrame::from_id_word(f.id_word(), f.data().to_vec(), false, false, false)
            }
            CanAnyFrame::Remote(f) => {
                let mut frame =
                    CanFrame::from_id_word(f.id_word(), Vec::new(), false, false, false);
                frame.rtr_dlc = f.dlc() as u8;
                frame
            }
            CanAnyFrame::Error(f) => {
                CanFrame::from_id_word(f.id_word(), f.data().to_vec(), false, false, false)
            }
            CanAnyFrame::Fd(f) => {
                CanFrame::from_id_word(f.id_word(), f.data().to_vec(), true, f.is_brs(), f.is_esi())
            }
        }
    }
}
