//! ISO 7816-4 command APDU parsing for the virtual NFC Forum Type 4 tag.
//!
//! An NFC Type 4 tag speaks ISO 7816-4 APDUs over ISO-DEP, so the same parser
//! that a contact smart card needs applies verbatim to a contactless tag.
//!
//! ## Command APDU layout
//!
//! ```text
//! | CLA | INS | P1 | P2 | [Lc] | [Data] | [Le] |
//! ```
//!
//! Both the short form (1-byte Lc/Le) and the extended form (`00` marker plus a
//! 2-byte Lc/Le) are accepted.
//!
//! ## Hostile input
//!
//! Every field these bytes come from is attacker-controlled: the reader on the
//! other end of the socket writes them. Nothing here indexes without first
//! checking the length, and nothing panics — a malformed APDU is an `Err` that
//! the caller answers with a status word, never an unwind inside a connection
//! task (which would be silent while the server still reported `Running`).

use anyhow::{bail, Result};

/// ISO 7816-4 instruction bytes the tag can name in an event.
pub mod ins {
    pub const SELECT: u8 = 0xA4;
    pub const READ_BINARY: u8 = 0xB0;
    pub const READ_RECORD: u8 = 0xB2;
    pub const UPDATE_BINARY: u8 = 0xD6;
    pub const UPDATE_RECORD: u8 = 0xDC;
    pub const VERIFY: u8 = 0x20;
    pub const GET_RESPONSE: u8 = 0xC0;
    pub const GET_DATA: u8 = 0xCA;
    pub const PUT_DATA: u8 = 0xDA;
    pub const ENVELOPE: u8 = 0xC2;
    pub const INTERNAL_AUTHENTICATE: u8 = 0x88;
    pub const EXTERNAL_AUTHENTICATE: u8 = 0x82;
    pub const GET_CHALLENGE: u8 = 0x84;
}

/// `P1` value of SELECT meaning "select by DF name", i.e. by application ID.
pub const SELECT_P1_BY_AID: u8 = 0x04;

/// Status word returned when the handler produced no usable response.
///
/// `6F00` is ISO 7816-4 "no precise diagnosis": a card-side failure. It is the
/// deliberate fail-*closed* answer, and is structurally distinct from a status
/// word the model chose itself (`6982`, `6A82`, …), so an LLM outage can never
/// be mistaken for the model approving something.
pub const SW_NO_PRECISE_DIAGNOSIS: (u8, u8) = (0x6F, 0x00);

/// Status word for an APDU that could not be parsed at all.
pub const SW_WRONG_LENGTH: (u8, u8) = (0x67, 0x00);

/// Status word returned when the backend is saturated and a retry may succeed.
///
/// `6400` is ISO 7816-4 "execution error, state of non-volatile memory unchanged":
/// the card did not do anything, and the reader may send the command again. It is
/// kept distinct from [`SW_NO_PRECISE_DIAGNOSIS`] so a transient overload is not
/// recorded by the reader as a permanent card fault — the same reason HTTP answers
/// 503 rather than 500.
pub const SW_EXECUTION_ERROR_STATE_UNCHANGED: (u8, u8) = (0x64, 0x00);

/// A parsed ISO 7816-4 command APDU.
#[derive(Debug, Clone)]
pub struct ApduCommand {
    pub cla: u8,
    pub ins: u8,
    pub p1: u8,
    pub p2: u8,
    /// Command data field (`Lc` bytes). Empty for case 1 and case 2 APDUs.
    pub data: Vec<u8>,
    /// Expected response length. `None` when the command carries no `Le`.
    /// A wire value of `00` means the maximum (256 short / 65536 extended).
    pub le: Option<u32>,
}

impl ApduCommand {
    /// Parse a command APDU, rejecting every truncated or inconsistent form.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 4 {
            bail!("APDU too short: {} byte(s), need at least 4", bytes.len());
        }

        let cla = bytes[0];
        let ins = bytes[1];
        let p1 = bytes[2];
        let p2 = bytes[3];
        let body = &bytes[4..];

        let (data, le) = match body.len() {
            // Case 1: header only.
            0 => (Vec::new(), None),
            // Case 2S: a single Le byte.
            1 => (Vec::new(), Some(expand_le_short(body[0]))),
            _ if body[0] != 0 => {
                // Short form with a data field.
                let lc = body[0] as usize;
                let data_end = 1 + lc;
                if body.len() < data_end {
                    bail!(
                        "APDU truncated: Lc={} but only {} byte(s) follow",
                        lc,
                        body.len() - 1
                    );
                }
                let data = body[1..data_end].to_vec();
                match body.len() - data_end {
                    0 => (data, None),
                    1 => (data, Some(expand_le_short(body[data_end]))),
                    extra => bail!("APDU has {} trailing byte(s) after Lc/Data/Le", extra),
                }
            }
            // Extended form: leading 00 marker followed by a 2-byte length.
            _ => {
                if body.len() < 3 {
                    bail!(
                        "APDU truncated: extended-length marker needs 3 byte(s), got {}",
                        body.len()
                    );
                }
                let extended = u16::from_be_bytes([body[1], body[2]]);
                if body.len() == 3 {
                    // Case 2E: extended Le only.
                    return Ok(Self {
                        cla,
                        ins,
                        p1,
                        p2,
                        data: Vec::new(),
                        le: Some(expand_le_extended(extended)),
                    });
                }
                let lc = extended as usize;
                if lc == 0 {
                    bail!("APDU has an extended-length marker with Lc=0 and trailing bytes");
                }
                let data_end = 3 + lc;
                if body.len() < data_end {
                    bail!(
                        "APDU truncated: extended Lc={} but only {} byte(s) follow",
                        lc,
                        body.len() - 3
                    );
                }
                let data = body[3..data_end].to_vec();
                match body.len() - data_end {
                    0 => (data, None),
                    2 => (
                        data,
                        Some(expand_le_extended(u16::from_be_bytes([
                            body[data_end],
                            body[data_end + 1],
                        ]))),
                    ),
                    extra => bail!(
                        "APDU has {} trailing byte(s) after extended Lc/Data/Le",
                        extra
                    ),
                }
            }
        };

        Ok(Self {
            cla,
            ins,
            p1,
            p2,
            data,
            le,
        })
    }

    /// Human-readable instruction name, so the model is not asked to memorise
    /// instruction bytes.
    pub fn ins_name(&self) -> &'static str {
        match self.ins {
            ins::SELECT => "SELECT",
            ins::READ_BINARY => "READ_BINARY",
            ins::READ_RECORD => "READ_RECORD",
            ins::UPDATE_BINARY => "UPDATE_BINARY",
            ins::UPDATE_RECORD => "UPDATE_RECORD",
            ins::VERIFY => "VERIFY",
            ins::GET_RESPONSE => "GET_RESPONSE",
            ins::GET_DATA => "GET_DATA",
            ins::PUT_DATA => "PUT_DATA",
            ins::ENVELOPE => "ENVELOPE",
            ins::INTERNAL_AUTHENTICATE => "INTERNAL_AUTHENTICATE",
            ins::EXTERNAL_AUTHENTICATE => "EXTERNAL_AUTHENTICATE",
            ins::GET_CHALLENGE => "GET_CHALLENGE",
            _ => "UNKNOWN",
        }
    }

    /// True for `SELECT` by DF name (AID) — the command a reader uses to pick
    /// an application on the tag. Raised as `nfc_tag_selected`; every other
    /// APDU, including SELECT by file identifier, is `nfc_apdu_received`.
    pub fn is_select_by_aid(&self) -> bool {
        self.ins == ins::SELECT && self.p1 == SELECT_P1_BY_AID && !self.data.is_empty()
    }
}

/// A short `Le` of `00` means 256 bytes, not zero.
fn expand_le_short(value: u8) -> u32 {
    if value == 0 {
        256
    } else {
        value as u32
    }
}

/// An extended `Le` of `0000` means 65536 bytes, not zero.
fn expand_le_extended(value: u16) -> u32 {
    if value == 0 {
        65536
    } else {
        value as u32
    }
}

/// A response APDU: optional data followed by the two status bytes.
#[derive(Debug, Clone)]
pub struct ApduResponse {
    pub data: Vec<u8>,
    pub sw1: u8,
    pub sw2: u8,
}

impl ApduResponse {
    pub fn new(data: Vec<u8>, sw1: u8, sw2: u8) -> Self {
        Self { data, sw1, sw2 }
    }

    /// The fail-closed response used when no handler produced one.
    pub fn card_error() -> Self {
        Self::new(
            Vec::new(),
            SW_NO_PRECISE_DIAGNOSIS.0,
            SW_NO_PRECISE_DIAGNOSIS.1,
        )
    }

    /// The fail-closed response for an internal (backend) failure.
    ///
    /// The two [`crate::utils::WireFailure`] categories map onto two different status
    /// words — `6400` for a saturated backend (retryable), `6F00` for everything else —
    /// so the reader can tell "try again" from "this card is broken". Nothing derived
    /// from the error itself reaches the wire: the status word is a category, and an
    /// APDU response carries no free-text field to leak one into.
    pub fn for_wire_failure(failure: crate::utils::WireFailure) -> Self {
        let (sw1, sw2) = match failure {
            crate::utils::WireFailure::Overloaded => SW_EXECUTION_ERROR_STATE_UNCHANGED,
            crate::utils::WireFailure::Unavailable => SW_NO_PRECISE_DIAGNOSIS,
        };
        Self::new(Vec::new(), sw1, sw2)
    }

    pub fn status_word(&self) -> String {
        format!("{:02X}{:02X}", self.sw1, self.sw2)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        let mut bytes = self.data;
        bytes.push(self.sw1);
        bytes.push(self.sw2);
        bytes
    }
}
