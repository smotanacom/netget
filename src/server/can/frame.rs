//! The CAN frame: representation, validation and the SocketCAN wire encoding.
//!
//! **This module is pure.** No socket, no `cfg(target_os)`, no configuration, no global state.
//! That is deliberate and it is the whole reason this protocol can be `Experimental` rather than
//! unverifiable: `AF_CAN` exists only in the Linux kernel, so the transport in
//! [`super::transport`] cannot be executed on the machine that wrote it — but everything that
//! *decides what the bytes are* can be, and is, asserted against literal values in
//! `tests/server/can/frame_test.rs`.
//!
//! Keep it pure. The moment it reads a socket or a config value, the only provable part of this
//! protocol stops being provable. (`bluetooth_ble_beacon/payload.rs` and `lldp/codec.rs` are the
//! same arrangement for the same reason.)
//!
//! # Two frame formats, one struct
//!
//! [`CanFrame`] covers classic CAN 2.0 and CAN FD, because a SocketCAN reader gets both off the
//! same socket and the model should not have to think about which struct it is holding.
//!
//! | | Classic CAN 2.0 | CAN FD |
//! |---|---|---|
//! | Identifier | 11-bit (standard) or 29-bit (extended) | same |
//! | Payload | 0-8 bytes | 0-8, 12, 16, 20, 24, 32, 48 or 64 bytes |
//! | DLC | equals the byte count | a 4-bit **length code**, not a byte count, above 8 |
//! | RTR | yes — a request for data, carrying none | **does not exist** |
//! | BRS / ESI | no | yes |
//!
//! ## The DLC trap
//!
//! For classic CAN, DLC and length are the same number and nothing goes wrong. For CAN FD they
//! are not: DLC 9 means **12** bytes, 15 means **64**. Writing `dlc = len` for an FD frame
//! produces a frame that is silently the wrong size, and reading `len = dlc` truncates one. That
//! is the classic implementation error in every CAN stack, so [`dlc_for_len`] and [`len_for_dlc`]
//! are separate named functions with the table written out, and the tests pin all sixteen codes
//! against literal values in both directions.
//!
//! FD also does not permit an arbitrary length: there is no encoding for 9, 10, 11, 13... bytes.
//! [`dlc_for_len`] therefore **rejects** those rather than rounding, because rounding up would
//! append bytes the model did not write and rounding down would drop bytes it did.
//!
//! # The SocketCAN wire layout
//!
//! [`CanFrame::to_wire_bytes`] and [`CanFrame::from_wire_bytes`] implement the kernel's
//! `struct can_frame` (16 octets) and `struct canfd_frame` (72 octets) from `<linux/can.h>`,
//! little-endian, which is what an `AF_CAN` socket reads and writes verbatim. Two things use it:
//! the UDP test transport carries exactly these bytes in a datagram, and the action executor
//! hands them to the server loop as `ActionResult::Output`. Having the layout here rather than
//! in the transport means the format is covered by tests on a machine with no CAN stack at all.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

// =================================================================================================
// SocketCAN identifier flags — <linux/can.h>
// =================================================================================================

/// `CAN_EFF_FLAG`: the identifier is a 29-bit extended one.
pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
/// `CAN_RTR_FLAG`: remote transmission request — a request for data, carrying none.
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
/// `CAN_ERR_FLAG`: this is an error frame; the identifier field is a bitmask of error classes.
pub const CAN_ERR_FLAG: u32 = 0x2000_0000;
/// `CAN_SFF_MASK`: 11 significant bits.
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;
/// `CAN_EFF_MASK`: 29 significant bits.
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;

/// `CANFD_BRS`: bit-rate switch — the data phase ran at the faster bit rate.
pub const CANFD_BRS: u8 = 0x01;
/// `CANFD_ESI`: error state indicator — the transmitting node was error-passive.
pub const CANFD_ESI: u8 = 0x02;
/// `CANFD_FDF`: this is an FD frame. Set by the kernel on `canfd_frame`; carried for fidelity.
pub const CANFD_FDF: u8 = 0x04;

/// `CAN_MTU`: `sizeof(struct can_frame)`.
pub const CAN_MTU: usize = 16;
/// `CANFD_MTU`: `sizeof(struct canfd_frame)`.
pub const CANFD_MTU: usize = 72;

/// Largest classic CAN payload.
pub const CAN_MAX_DLEN: usize = 8;
/// Largest CAN FD payload.
pub const CANFD_MAX_DLEN: usize = 64;

// =================================================================================================
// Error-class bits — the identifier field of an error frame
// =================================================================================================

/// `CAN_ERR_TX_TIMEOUT`
pub const CAN_ERR_TX_TIMEOUT: u32 = 0x0000_0001;
/// `CAN_ERR_LOSTARB` — arbitration lost
pub const CAN_ERR_LOSTARB: u32 = 0x0000_0002;
/// `CAN_ERR_CRTL` — controller problems; `data[1]` says which
pub const CAN_ERR_CRTL: u32 = 0x0000_0004;
/// `CAN_ERR_PROT` — protocol violation; `data[2]`/`data[3]` say which
pub const CAN_ERR_PROT: u32 = 0x0000_0008;
/// `CAN_ERR_TRX` — transceiver status
pub const CAN_ERR_TRX: u32 = 0x0000_0010;
/// `CAN_ERR_ACK` — no acknowledgement on transmit
pub const CAN_ERR_ACK: u32 = 0x0000_0020;
/// `CAN_ERR_BUSOFF` — the controller took itself off the bus
pub const CAN_ERR_BUSOFF: u32 = 0x0000_0040;
/// `CAN_ERR_BUSERROR` — bus error (the counter, not a state change)
pub const CAN_ERR_BUSERROR: u32 = 0x0000_0080;
/// `CAN_ERR_RESTARTED` — the controller restarted
pub const CAN_ERR_RESTARTED: u32 = 0x0000_0100;
/// `CAN_ERR_CNT` — the error counters in `data[6]`/`data[7]` are valid
pub const CAN_ERR_CNT: u32 = 0x0000_0200;

/// `CAN_ERR_CRTL_RX_OVERFLOW`
pub const CAN_ERR_CRTL_RX_OVERFLOW: u8 = 0x01;
/// `CAN_ERR_CRTL_TX_OVERFLOW`
pub const CAN_ERR_CRTL_TX_OVERFLOW: u8 = 0x02;
/// `CAN_ERR_CRTL_RX_WARNING`
pub const CAN_ERR_CRTL_RX_WARNING: u8 = 0x04;
/// `CAN_ERR_CRTL_TX_WARNING`
pub const CAN_ERR_CRTL_TX_WARNING: u8 = 0x08;
/// `CAN_ERR_CRTL_RX_PASSIVE`
pub const CAN_ERR_CRTL_RX_PASSIVE: u8 = 0x10;
/// `CAN_ERR_CRTL_TX_PASSIVE`
pub const CAN_ERR_CRTL_TX_PASSIVE: u8 = 0x20;
/// `CAN_ERR_CRTL_ACTIVE` — back to error-active
pub const CAN_ERR_CRTL_ACTIVE: u8 = 0x40;

// =================================================================================================
// DLC ⇄ length
// =================================================================================================

/// The CAN FD length for each of the sixteen data length codes.
///
/// Written out rather than computed. The published table (ISO 11898-1:2015) is
/// `0..=8` verbatim, then `12, 16, 20, 24, 32, 48, 64` — a sequence with three different step
/// sizes, which is exactly why implementations get it wrong when they try to be clever.
pub const FD_DLC_TO_LEN: [usize; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64];

/// How many payload bytes a data length code stands for.
///
/// `fd` selects the table: on a classic frame every code above 8 still means 8 bytes (the
/// controller transmits eight and the surplus code is meaningless), while on an FD frame codes
/// 9-15 mean 12, 16, 20, 24, 32, 48 and 64.
///
/// A `dlc` above 15 cannot occur — the field is four bits — and is clamped rather than panicking,
/// because this is on the receive path and a corrupt value must not take the server down.
pub fn len_for_dlc(dlc: u8, fd: bool) -> usize {
    let dlc = (dlc & 0x0F) as usize;
    if fd {
        FD_DLC_TO_LEN[dlc]
    } else {
        dlc.min(CAN_MAX_DLEN)
    }
}

/// The data length code that encodes exactly `len` payload bytes.
///
/// Rejects any length the format cannot express, rather than rounding. For CAN FD the encodable
/// lengths are 0-8, 12, 16, 20, 24, 32, 48 and 64; a 9-byte payload has **no** representation, and
/// silently padding it to 12 would put three bytes on the wire that the model did not write.
pub fn dlc_for_len(len: usize, fd: bool) -> Result<u8> {
    if fd {
        FD_DLC_TO_LEN
            .iter()
            .position(|&candidate| candidate == len)
            .map(|dlc| dlc as u8)
            .ok_or_else(|| {
                anyhow!(
                    "a CAN FD frame carries 0-8, 12, 16, 20, 24, 32, 48 or 64 bytes; {len} is not \
                     an encodable length. Pad the payload to the next encodable size yourself if \
                     that is what you mean - this codec will not invent bytes."
                )
            })
    } else if len <= CAN_MAX_DLEN {
        Ok(len as u8)
    } else {
        Err(anyhow!(
            "a classic CAN frame carries at most 8 bytes; {len} given. Set \"fd\": true for a CAN \
             FD frame, which carries up to 64."
        ))
    }
}

// =================================================================================================
// Bus state
// =================================================================================================

/// The CAN controller's error state, as the error-confinement rules of ISO 11898-1 define it.
///
/// A node starts *error-active* and shouts about every error it sees. As its error counters rise
/// past 127 it becomes *error-passive* and stops interfering. Past 255 it goes *bus-off* and
/// disconnects itself entirely: it transmits nothing and receives nothing until it is restarted.
/// This ladder is why an error frame is never a suitable way to report a NetGet failure — see
/// this protocol's `CLAUDE.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusState {
    /// Normal. Errors are signalled actively.
    ErrorActive,
    /// A counter passed 96: still active, but close to the limit.
    ErrorWarning,
    /// A counter passed 127: the node no longer signals errors actively.
    ErrorPassive,
    /// A counter passed 255: the node has removed itself from the bus.
    BusOff,
}

impl BusState {
    /// The name used in event data and in the log.
    pub fn as_str(self) -> &'static str {
        match self {
            BusState::ErrorActive => "error_active",
            BusState::ErrorWarning => "error_warning",
            BusState::ErrorPassive => "error_passive",
            BusState::BusOff => "bus_off",
        }
    }
}

// =================================================================================================
// The frame
// =================================================================================================

/// One CAN frame, classic or FD, in or out.
///
/// `data` is the payload as bytes; how it is *written* in JSON is the `encoding` field's job
/// ([`CanFrame::from_action`]), never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFrame {
    /// 11-bit or 29-bit identifier, without any of the SocketCAN flag bits.
    pub id: u32,
    /// True for a 29-bit identifier. **Explicit, never inferred from magnitude**: `0x123` is a
    /// perfectly legal extended identifier and is a different frame from standard `0x123`.
    pub extended: bool,
    /// Remote transmission request: a request for data, carrying none. Not valid on FD.
    pub rtr: bool,
    /// This is an error frame. The identifier is then a bitmask of error classes, not an address.
    pub error: bool,
    /// CAN FD rather than classic CAN 2.0.
    pub fd: bool,
    /// Bit-rate switch: the data phase used the faster bit rate. FD only.
    pub brs: bool,
    /// Error state indicator: the transmitter was error-passive. FD only, receive-side only.
    pub esi: bool,
    /// Payload. Empty for an RTR frame, whose length lives in `rtr_dlc`.
    pub data: Vec<u8>,
    /// The number of bytes an RTR frame is *requesting*. Meaningless on any other frame.
    pub rtr_dlc: u8,
}

impl CanFrame {
    /// A classic data frame.
    pub fn classic(id: u32, extended: bool, data: Vec<u8>) -> Result<Self> {
        let frame = Self {
            id,
            extended,
            rtr: false,
            error: false,
            fd: false,
            brs: false,
            esi: false,
            data,
            rtr_dlc: 0,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// A CAN FD data frame.
    pub fn fd(id: u32, extended: bool, data: Vec<u8>, brs: bool) -> Result<Self> {
        let frame = Self {
            id,
            extended,
            rtr: false,
            error: false,
            fd: true,
            brs,
            esi: false,
            data,
            rtr_dlc: 0,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// A remote transmission request for `dlc` bytes.
    pub fn remote(id: u32, extended: bool, dlc: u8) -> Result<Self> {
        let frame = Self {
            id,
            extended,
            rtr: true,
            error: false,
            fd: false,
            brs: false,
            esi: false,
            data: Vec::new(),
            rtr_dlc: dlc,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Everything that must hold before a frame can be encoded.
    ///
    /// Called by every constructor and again by [`Self::to_wire_bytes`], so a frame built by
    /// hand cannot skip it.
    pub fn validate(&self) -> Result<()> {
        if self.error {
            // An error frame's "identifier" is a class bitmask, so the 11/29-bit rules do not
            // apply to it. NetGet never *builds* one; this branch exists for frames read off the
            // bus, which are already whatever the controller said they were.
            return Ok(());
        }

        let limit = if self.extended {
            CAN_EFF_MASK
        } else {
            CAN_SFF_MASK
        };
        if self.id > limit {
            bail!(
                "identifier 0x{:X} does not fit in {} bits (maximum 0x{:X}). {}",
                self.id,
                if self.extended { 29 } else { 11 },
                limit,
                if self.extended {
                    "29 bits is the largest CAN identifier there is."
                } else {
                    "Set \"extended\": true for a 29-bit identifier."
                }
            );
        }

        if self.rtr {
            if self.fd {
                bail!(
                    "CAN FD has no remote frames: the RTR bit was reused as RRS and is always \
                     dominant. Send an RTR request as a classic frame (\"fd\": false), or send a \
                     data frame."
                );
            }
            if !self.data.is_empty() {
                bail!(
                    "a remote frame carries no data by definition; {} byte(s) were given. Its DLC \
                     names how many bytes are being *requested*.",
                    self.data.len()
                );
            }
            if self.rtr_dlc as usize > CAN_MAX_DLEN {
                bail!(
                    "a remote frame can request at most 8 bytes; dlc {} given",
                    self.rtr_dlc
                );
            }
            return Ok(());
        }

        if self.brs && !self.fd {
            bail!(
                "the bit-rate switch is a CAN FD feature; a classic frame has one bit rate. Set \
                 \"fd\": true, or drop \"brs\"."
            );
        }

        // The real check: this is what rejects an over-length payload instead of truncating it.
        dlc_for_len(self.data.len(), self.fd)?;
        Ok(())
    }

    /// The data length code this frame will carry on the wire.
    pub fn dlc(&self) -> u8 {
        if self.rtr {
            self.rtr_dlc
        } else {
            dlc_for_len(self.data.len(), self.fd).unwrap_or(0)
        }
    }

    /// The composite SocketCAN identifier word: the identifier plus the EFF/RTR/ERR flag bits.
    pub fn id_word(&self) -> u32 {
        let mut word = if self.error {
            self.id
        } else if self.extended {
            self.id & CAN_EFF_MASK
        } else {
            self.id & CAN_SFF_MASK
        };
        if self.extended {
            word |= CAN_EFF_FLAG;
        }
        if self.rtr {
            word |= CAN_RTR_FLAG;
        }
        if self.error {
            word |= CAN_ERR_FLAG;
        }
        word
    }

    /// Rebuild a frame from a composite identifier word and a payload.
    ///
    /// This is the receive-side entry point for both transports: the Linux one gets the word from
    /// `socketcan`'s `Frame::id_word()`, and the UDP one from the first four octets of the
    /// datagram. Keeping the flag decoding here — rather than once per transport — is what lets
    /// `tests/server/can/frame_test.rs` prove it on a machine with no CAN stack.
    pub fn from_id_word(id_word: u32, data: Vec<u8>, fd: bool, brs: bool, esi: bool) -> Self {
        let extended = id_word & CAN_EFF_FLAG != 0;
        let rtr = id_word & CAN_RTR_FLAG != 0;
        let error = id_word & CAN_ERR_FLAG != 0;

        let id = if error {
            // The class bitmask occupies the same 29 bits.
            id_word & CAN_EFF_MASK
        } else if extended {
            id_word & CAN_EFF_MASK
        } else {
            id_word & CAN_SFF_MASK
        };

        Self {
            id,
            extended,
            rtr,
            error,
            fd,
            brs,
            esi,
            // A remote frame's payload octets are undefined; the DLC is the message.
            data: if rtr { Vec::new() } else { data },
            rtr_dlc: 0,
        }
    }

    // ---------------------------------------------------------------------------------------------
    // The SocketCAN wire layout
    // ---------------------------------------------------------------------------------------------

    /// Encode as `struct can_frame` (16 octets) or `struct canfd_frame` (72 octets).
    ///
    /// Little-endian, matching `<linux/can.h>` on every architecture Linux supports CAN on. The
    /// two layouts differ in more than length, so the tests pin both:
    ///
    /// ```text
    /// struct can_frame {          struct canfd_frame {
    ///   canid_t can_id;   // 4      canid_t can_id;   // 4
    ///   __u8    len;      // 1      __u8    len;      // 1
    ///   __u8    __pad;    // 1      __u8    flags;    // 1   <- BRS/ESI/FDF live here
    ///   __u8    __res0;   // 1      __u8    __res0;   // 1
    ///   __u8    len8_dlc; // 1      __u8    __res1;   // 1
    ///   __u8    data[8];  // 8      __u8    data[64]; // 64
    /// };                          };
    /// ```
    ///
    /// Note `len` is a **byte count** in both structs — the kernel does the DLC encoding itself on
    /// the way to the controller. The DLC still matters and is still reported to the model,
    /// because it is what appears on the physical bus and what a DBC file is written against; for
    /// a remote frame it is the only content there is, so it is written into `len`, which is where
    /// the kernel reads an RTR request length from.
    pub fn to_wire_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;

        let id_word = self.id_word().to_le_bytes();

        if self.fd {
            let mut out = vec![0u8; CANFD_MTU];
            out[0..4].copy_from_slice(&id_word);
            out[4] = self.data.len() as u8;
            let mut flags = CANFD_FDF;
            if self.brs {
                flags |= CANFD_BRS;
            }
            if self.esi {
                flags |= CANFD_ESI;
            }
            out[5] = flags;
            out[8..8 + self.data.len()].copy_from_slice(&self.data);
            Ok(out)
        } else {
            let mut out = vec![0u8; CAN_MTU];
            out[0..4].copy_from_slice(&id_word);
            out[4] = if self.rtr {
                self.rtr_dlc
            } else {
                self.data.len() as u8
            };
            // `len8_dlc` carries a raw DLC above 8 for classic frames on controllers that allow
            // it; NetGet never produces one, so it stays zero.
            out[8..8 + self.data.len()].copy_from_slice(&self.data);
            Ok(out)
        }
    }

    /// Decode a `struct can_frame` or `struct canfd_frame`.
    ///
    /// The length picks the layout, which is exactly how `read()` on an `AF_CAN` socket
    /// distinguishes them: an FD-enabled socket returns 72 octets for an FD frame and 16 for a
    /// classic one.
    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self> {
        match bytes.len() {
            CAN_MTU => {
                let id_word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let rtr = id_word & CAN_RTR_FLAG != 0;
                let len = (bytes[4] as usize).min(CAN_MAX_DLEN);
                let mut frame =
                    Self::from_id_word(id_word, bytes[8..8 + len].to_vec(), false, false, false);
                if rtr {
                    // For an RTR frame `len` is the requested byte count, not a payload length.
                    frame.rtr_dlc = bytes[4] & 0x0F;
                }
                Ok(frame)
            }
            CANFD_MTU => {
                let id_word = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                let len = (bytes[4] as usize).min(CANFD_MAX_DLEN);
                let flags = bytes[5];
                Ok(Self::from_id_word(
                    id_word,
                    bytes[8..8 + len].to_vec(),
                    true,
                    flags & CANFD_BRS != 0,
                    flags & CANFD_ESI != 0,
                ))
            }
            other => Err(anyhow!(
                "a SocketCAN frame is {CAN_MTU} octets (struct can_frame) or {CANFD_MTU} octets \
                 (struct canfd_frame); got {other}"
            )),
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Error frames
    // ---------------------------------------------------------------------------------------------

    /// The error classes this frame reports, as names, or `None` if it is not an error frame.
    ///
    /// The model is given names rather than the bitmask because a mask is exactly the kind of
    /// thing it cannot reliably decode, and because these names are what `candump` prints and
    /// what every CAN document calls them.
    pub fn error_classes(&self) -> Option<Vec<&'static str>> {
        if !self.error {
            return None;
        }
        let mut classes = Vec::new();
        for (bit, name) in [
            (CAN_ERR_TX_TIMEOUT, "tx_timeout"),
            (CAN_ERR_LOSTARB, "arbitration_lost"),
            (CAN_ERR_CRTL, "controller_problem"),
            (CAN_ERR_PROT, "protocol_violation"),
            (CAN_ERR_TRX, "transceiver_status"),
            (CAN_ERR_ACK, "no_acknowledgement"),
            (CAN_ERR_BUSOFF, "bus_off"),
            (CAN_ERR_BUSERROR, "bus_error"),
            (CAN_ERR_RESTARTED, "controller_restarted"),
            (CAN_ERR_CNT, "error_counters"),
        ] {
            if self.id & bit != 0 {
                classes.push(name);
            }
        }
        if classes.is_empty() {
            classes.push("unspecified");
        }
        Some(classes)
    }

    /// The controller state this error frame reports, if it reports one.
    ///
    /// `None` for a data frame, and for an error frame that says nothing about confinement — a
    /// lost arbitration is not a state change. This is the emit condition for
    /// `can_bus_state_changed`, so getting it wrong would either flood the model with
    /// non-transitions or hide a bus-off.
    pub fn bus_state(&self) -> Option<BusState> {
        if !self.error {
            return None;
        }
        if self.id & CAN_ERR_BUSOFF != 0 {
            return Some(BusState::BusOff);
        }
        if self.id & CAN_ERR_RESTARTED != 0 {
            return Some(BusState::ErrorActive);
        }
        if self.id & CAN_ERR_CRTL == 0 {
            return None;
        }
        // data[1] carries the controller status for CAN_ERR_CRTL. A frame too short to hold it
        // is malformed; report nothing rather than guessing.
        let status = *self.data.get(1)?;
        if status & (CAN_ERR_CRTL_RX_PASSIVE | CAN_ERR_CRTL_TX_PASSIVE) != 0 {
            Some(BusState::ErrorPassive)
        } else if status & (CAN_ERR_CRTL_RX_WARNING | CAN_ERR_CRTL_TX_WARNING) != 0 {
            Some(BusState::ErrorWarning)
        } else if status & CAN_ERR_CRTL_ACTIVE != 0 {
            Some(BusState::ErrorActive)
        } else {
            // Overflows are real problems but not confinement states.
            None
        }
    }

    // ---------------------------------------------------------------------------------------------
    // JSON
    // ---------------------------------------------------------------------------------------------

    /// The identifier written the way automotive documentation writes it.
    pub fn id_hex(&self) -> String {
        if self.extended {
            format!("0x{:08X}", self.id)
        } else {
            format!("0x{:03X}", self.id)
        }
    }

    /// A one-line description for the log.
    pub fn describe(&self) -> String {
        if self.error {
            return format!(
                "error frame [{}]",
                self.error_classes().unwrap_or_default().join(",")
            );
        }
        let kind = match (self.fd, self.rtr) {
            (true, _) => {
                if self.brs {
                    "FD/BRS"
                } else {
                    "FD"
                }
            }
            (false, true) => "RTR",
            (false, false) => "CAN",
        };
        format!(
            "{} {} dlc={} {}",
            kind,
            self.id_hex(),
            self.dlc(),
            if self.rtr {
                "(no data)".to_string()
            } else {
                hex::encode(&self.data)
            }
        )
    }

    /// Event data for `can_frame_received` and friends.
    ///
    /// `data` is hex and `data_encoding` says so, every time. See [`Self::from_action`] for why
    /// hex is the default here and not in most of this codebase.
    pub fn to_event_data(&self) -> serde_json::Map<String, Value> {
        let mut map = serde_json::Map::new();
        map.insert("id".into(), json!(self.id_hex()));
        map.insert("id_decimal".into(), json!(self.id));
        map.insert("extended".into(), json!(self.extended));
        map.insert("rtr".into(), json!(self.rtr));
        map.insert("error".into(), json!(self.error));
        map.insert("fd".into(), json!(self.fd));
        map.insert("brs".into(), json!(self.brs));
        map.insert("esi".into(), json!(self.esi));
        map.insert("dlc".into(), json!(self.dlc()));
        map.insert("data_length".into(), json!(self.data.len()));
        map.insert("data".into(), json!(hex::encode(&self.data)));
        map.insert("data_encoding".into(), json!("hex"));
        if let Some(classes) = self.error_classes() {
            map.insert("error_classes".into(), json!(classes));
        }
        if let Some(state) = self.bus_state() {
            map.insert("bus_state".into(), json!(state.as_str()));
        }
        map
    }

    /// Build a frame from a `send_can_frame` action object.
    ///
    /// Pure, so the whole action vocabulary is tested on a machine with no CAN stack.
    ///
    /// # `encoding` is read, never sniffed
    ///
    /// `"hex"` is the default, which is unusual in this codebase and is right here: a CAN payload
    /// is eight bytes whose meaning is defined by a DBC file NetGet does not have. There is no
    /// text to default to. `"text"` is offered because an ASCII payload does occur (diagnostic
    /// VIN responses, some telematics gateways) and writing `"484921"` for `"HI!"` helps nobody.
    ///
    /// The executor **really decodes** what the field says. It never guesses: `"48656c6c6f"` is
    /// simultaneously valid text and valid hex, and only the sender knows which it meant. This is
    /// the `send_tcp_data` defect the root `CLAUDE.md` records, where the documentation promised
    /// hex and the executor called `as_bytes()`, putting literal ASCII on the wire.
    pub fn from_action(action: &Value) -> Result<Self> {
        let extended = action
            .get("extended")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let rtr = action.get("rtr").and_then(Value::as_bool).unwrap_or(false);
        let fd = action.get("fd").and_then(Value::as_bool).unwrap_or(false);
        let brs = action.get("brs").and_then(Value::as_bool).unwrap_or(false);

        let id = parse_id(
            action
                .get("id")
                .context("send_can_frame requires 'id' (e.g. \"0x7DF\" or 2015)")?,
        )?;

        if rtr {
            let dlc = action
                .get("dlc")
                .and_then(Value::as_u64)
                .or_else(|| action.get("data_length").and_then(Value::as_u64))
                .unwrap_or(0);
            if dlc > CAN_MAX_DLEN as u64 {
                bail!("a remote frame can request at most 8 bytes; dlc {dlc} given");
            }
            let mut frame = Self::remote(id, extended, dlc as u8)?;
            frame.fd = fd;
            frame.brs = brs;
            // Re-validated so an `"rtr": true, "fd": true` combination is rejected by name rather
            // than encoded into something the bus cannot carry.
            frame.validate()?;
            return Ok(frame);
        }

        let data = decode_payload(action)?;
        let frame = Self {
            id,
            extended,
            rtr: false,
            error: false,
            fd,
            brs,
            esi: false,
            data,
            rtr_dlc: 0,
        };
        frame.validate()?;
        Ok(frame)
    }
}

/// Decode a `send_can_frame` payload according to its declared `encoding`.
///
/// Absent `data` is an empty payload — a zero-length frame is legal CAN and is how a node
/// announces presence without content.
fn decode_payload(action: &Value) -> Result<Vec<u8>> {
    let Some(data) = action.get("data") else {
        return Ok(Vec::new());
    };

    // A JSON array of byte values is accepted because a script handler naturally produces one,
    // and refusing it would push script authors into hex-encoding by hand.
    if let Some(array) = data.as_array() {
        return array
            .iter()
            .map(|v| {
                v.as_u64()
                    .filter(|n| *n <= 0xFF)
                    .map(|n| n as u8)
                    .ok_or_else(|| {
                        anyhow!("every element of a 'data' array must be 0-255, got {v}")
                    })
            })
            .collect();
    }

    let text = data
        .as_str()
        .ok_or_else(|| anyhow!("'data' must be a string (see 'encoding') or an array of bytes"))?;

    match action.get("encoding").and_then(Value::as_str) {
        None | Some("hex") => {
            let cleaned: String = text
                .chars()
                .filter(|c| !c.is_whitespace() && *c != ':' && *c != '-')
                .collect();
            let cleaned = cleaned
                .strip_prefix("0x")
                .or_else(|| cleaned.strip_prefix("0X"))
                .unwrap_or(&cleaned)
                .to_string();
            hex::decode(&cleaned).with_context(|| {
                format!(
                    "'data' is hex-encoded (the default for CAN) but {text:?} is not valid hex. \
                     Pass \"encoding\": \"text\" if you meant these characters literally."
                )
            })
        }
        Some("text") => Ok(text.as_bytes().to_vec()),
        Some(other) => Err(anyhow!(
            "unknown encoding {other:?}: expected \"hex\" (the default) or \"text\""
        )),
    }
}

/// Parse an identifier written as a JSON number or as a hex string.
///
/// `"0x7DF"`, `"7DF"` and `2015` are the same identifier. Hex is accepted — and is how every CAN
/// document, DBC file and `candump` line writes an identifier — but it is a fixed-width numeric
/// identifier in its published notation, not a byte blob, which is the same reasoning that lets
/// `bluetooth_ble_beacon` take an Eddystone namespace as hex.
pub fn parse_id(value: &Value) -> Result<u32> {
    if let Some(n) = value.as_u64() {
        return u32::try_from(n)
            .map_err(|_| anyhow!("identifier {n} does not fit in 32 bits, let alone 29"));
    }
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("'id' must be a number or a hex string such as \"0x7DF\""))?
        .trim();
    let (body, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(body) => (body, 16),
        // A bare string is hex: that is the universal notation for a CAN identifier, and reading
        // "0700" as seven hundred decimal would silently address a different ECU.
        None => (text, 16),
    };
    u32::from_str_radix(body, radix)
        .with_context(|| format!("'id' {text:?} is not a valid CAN identifier"))
}
