//! GTP codec — GTPv1-C/GTPv1-U (3GPP TS 29.060) and GTPv2-C (3GPP TS 29.274).
//!
//! Pure: no I/O, no state, no LLM. `mod.rs` owns the sockets and `actions.rs` owns the
//! model's vocabulary; everything that turns bytes into structure and back lives here so the
//! literal-byte tests in `tests/server/gtp/codec_test.rs` can pin it without a running server.
//!
//! # The two rules this file exists to get right
//!
//! 1. **The GTPv1 E/S/PN flags are all-or-nothing.** If *any* of the extension-header,
//!    sequence-number or N-PDU-number flags is set, **all four optional octets are present**
//!    (2 sequence, 1 N-PDU, 1 next-extension-header) and each field is only *meaningful* when
//!    its own flag is set. Implementations routinely assume only the flagged field appears,
//!    which desynchronises every subsequent byte. See [`GtpV1Header::optional_present`].
//! 2. **GTPv1 information elements below type 128 are fixed-length (TV), 128 and above are
//!    variable-length (TLV) with a two-octet length.** A decoder that assumes TLV throughout
//!    misreads a Cause IE as a length prefix. See [`v1_fixed_ie_len`].
//!
//! GTPv2-C has neither problem: its header length is decided by one flag (T) and every IE is
//! TLIV. It is implemented here because both versions share UDP 2123 and a node has to tell
//! them apart from the first three bits anyway.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

// ===========================================================================
// Ports
// ===========================================================================

/// GTP-C signalling port (TS 29.060 §4.1, TS 29.274 §4.1). Above 1023.
pub const GTPC_PORT: u16 = 2123;
/// GTP-U user-plane port (TS 29.060 §4.1). Above 1023.
pub const GTPU_PORT: u16 = 2152;

// ===========================================================================
// Message types
// ===========================================================================

pub const V1_ECHO_REQUEST: u8 = 1;
pub const V1_ECHO_RESPONSE: u8 = 2;
pub const V1_VERSION_NOT_SUPPORTED: u8 = 3;
pub const V1_CREATE_PDP_CONTEXT_REQUEST: u8 = 16;
pub const V1_CREATE_PDP_CONTEXT_RESPONSE: u8 = 17;
pub const V1_UPDATE_PDP_CONTEXT_REQUEST: u8 = 18;
pub const V1_UPDATE_PDP_CONTEXT_RESPONSE: u8 = 19;
pub const V1_DELETE_PDP_CONTEXT_REQUEST: u8 = 20;
pub const V1_DELETE_PDP_CONTEXT_RESPONSE: u8 = 21;
pub const V1_ERROR_INDICATION: u8 = 26;
pub const V1_SUPPORTED_EXTENSION_HEADERS_NOTIFICATION: u8 = 31;
pub const V1_END_MARKER: u8 = 254;
pub const V1_G_PDU: u8 = 255;

pub const V2_ECHO_REQUEST: u8 = 1;
pub const V2_ECHO_RESPONSE: u8 = 2;
pub const V2_CREATE_SESSION_REQUEST: u8 = 32;
pub const V2_CREATE_SESSION_RESPONSE: u8 = 33;
pub const V2_MODIFY_BEARER_REQUEST: u8 = 34;
pub const V2_MODIFY_BEARER_RESPONSE: u8 = 35;
pub const V2_DELETE_SESSION_REQUEST: u8 = 36;
pub const V2_DELETE_SESSION_RESPONSE: u8 = 37;

/// Human name for a GTPv1 message type, for logs and event data.
pub fn v1_message_name(t: u8) -> &'static str {
    match t {
        V1_ECHO_REQUEST => "Echo Request",
        V1_ECHO_RESPONSE => "Echo Response",
        V1_VERSION_NOT_SUPPORTED => "Version Not Supported",
        V1_CREATE_PDP_CONTEXT_REQUEST => "Create PDP Context Request",
        V1_CREATE_PDP_CONTEXT_RESPONSE => "Create PDP Context Response",
        V1_UPDATE_PDP_CONTEXT_REQUEST => "Update PDP Context Request",
        V1_UPDATE_PDP_CONTEXT_RESPONSE => "Update PDP Context Response",
        V1_DELETE_PDP_CONTEXT_REQUEST => "Delete PDP Context Request",
        V1_DELETE_PDP_CONTEXT_RESPONSE => "Delete PDP Context Response",
        V1_ERROR_INDICATION => "Error Indication",
        V1_SUPPORTED_EXTENSION_HEADERS_NOTIFICATION => "Supported Extension Headers Notification",
        V1_END_MARKER => "End Marker",
        V1_G_PDU => "G-PDU",
        _ => "Unknown",
    }
}

/// Human name for a GTPv2-C message type.
pub fn v2_message_name(t: u8) -> &'static str {
    match t {
        V2_ECHO_REQUEST => "Echo Request",
        V2_ECHO_RESPONSE => "Echo Response",
        V2_CREATE_SESSION_REQUEST => "Create Session Request",
        V2_CREATE_SESSION_RESPONSE => "Create Session Response",
        V2_MODIFY_BEARER_REQUEST => "Modify Bearer Request",
        V2_MODIFY_BEARER_RESPONSE => "Modify Bearer Response",
        V2_DELETE_SESSION_REQUEST => "Delete Session Request",
        V2_DELETE_SESSION_RESPONSE => "Delete Session Response",
        _ => "Unknown",
    }
}

// ===========================================================================
// GTPv1 information element types (TS 29.060 §7.7)
// ===========================================================================

pub const V1_IE_CAUSE: u8 = 1;
pub const V1_IE_IMSI: u8 = 2;
pub const V1_IE_REORDERING_REQUIRED: u8 = 8;
pub const V1_IE_RECOVERY: u8 = 14;
pub const V1_IE_SELECTION_MODE: u8 = 15;
pub const V1_IE_TEID_DATA_I: u8 = 16;
pub const V1_IE_TEID_CONTROL_PLANE: u8 = 17;
pub const V1_IE_TEARDOWN_IND: u8 = 19;
pub const V1_IE_NSAPI: u8 = 20;
pub const V1_IE_CHARGING_ID: u8 = 127;
pub const V1_IE_END_USER_ADDRESS: u8 = 128;
pub const V1_IE_ACCESS_POINT_NAME: u8 = 131;
pub const V1_IE_PROTOCOL_CONFIG_OPTIONS: u8 = 132;
pub const V1_IE_GSN_ADDRESS: u8 = 133;
pub const V1_IE_MSISDN: u8 = 134;
pub const V1_IE_QOS_PROFILE: u8 = 135;
pub const V1_IE_RAT_TYPE: u8 = 151;
pub const V1_IE_USER_LOCATION_INFO: u8 = 152;
pub const V1_IE_MS_TIME_ZONE: u8 = 153;
pub const V1_IE_IMEISV: u8 = 154;

/// Length in octets of the *value* of a GTPv1 fixed-length (TV) information element.
///
/// Types 1..=127 are fixed-length and carry **no length field on the wire**, so a decoder
/// that does not know the type cannot skip it — [`parse_v1_ies`] therefore stops with
/// [`DecodeError::UnknownFixedIe`] rather than guessing, which is the only safe answer.
///
/// Source: TS 29.060 Table 37 ("Information Elements") and §7.7.
pub fn v1_fixed_ie_len(ie_type: u8) -> Option<usize> {
    Some(match ie_type {
        1 => 1,   // Cause
        2 => 8,   // IMSI (TBCD)
        3 => 6,   // Routeing Area Identity
        4 => 4,   // TLLI
        5 => 4,   // P-TMSI
        8 => 1,   // Reordering Required
        9 => 28,  // Authentication Triplet
        11 => 1,  // MAP Cause
        12 => 3,  // P-TMSI Signature
        13 => 1,  // MS Validated
        14 => 1,  // Recovery
        15 => 1,  // Selection Mode
        16 => 4,  // TEID Data I
        17 => 4,  // TEID Control Plane
        18 => 5,  // TEID Data II
        19 => 1,  // Teardown Ind
        20 => 1,  // NSAPI
        21 => 1,  // RANAP Cause
        22 => 9,  // RAB Context
        23 => 1,  // Radio Priority SMS
        24 => 1,  // Radio Priority
        25 => 2,  // Packet Flow Id
        26 => 2,  // Charging Characteristics
        27 => 2,  // Trace Reference
        28 => 2,  // Trace Type
        29 => 1,  // MS Not Reachable Reason
        127 => 4, // Charging ID
        _ => return None,
    })
}

/// Human name for the GTPv1 IEs this server surfaces. Unknown types are reported by number.
pub fn v1_ie_name(ie_type: u8) -> &'static str {
    match ie_type {
        1 => "Cause",
        2 => "IMSI",
        3 => "Routeing Area Identity",
        8 => "Reordering Required",
        14 => "Recovery",
        15 => "Selection Mode",
        16 => "TEID Data I",
        17 => "TEID Control Plane",
        19 => "Teardown Ind",
        20 => "NSAPI",
        127 => "Charging ID",
        128 => "End User Address",
        129 => "MM Context",
        130 => "PDP Context",
        131 => "Access Point Name",
        132 => "Protocol Configuration Options",
        133 => "GSN Address",
        134 => "MSISDN",
        135 => "QoS Profile",
        148 => "Common Flags",
        149 => "APN Restriction",
        151 => "RAT Type",
        152 => "User Location Information",
        153 => "MS Time Zone",
        154 => "IMEI(SV)",
        251 => "Charging Gateway Address",
        255 => "Private Extension",
        _ => "Unknown",
    }
}

// ===========================================================================
// GTPv2-C information element types (TS 29.274 §8)
// ===========================================================================

pub const V2_IE_IMSI: u8 = 1;
pub const V2_IE_CAUSE: u8 = 2;
pub const V2_IE_RECOVERY: u8 = 3;
pub const V2_IE_APN: u8 = 71;
pub const V2_IE_EBI: u8 = 73;
pub const V2_IE_MSISDN: u8 = 76;
pub const V2_IE_PCO: u8 = 78;
pub const V2_IE_PAA: u8 = 79;
pub const V2_IE_RAT_TYPE: u8 = 82;
pub const V2_IE_FTEID: u8 = 87;
pub const V2_IE_BEARER_CONTEXT: u8 = 93;
pub const V2_IE_ULI: u8 = 86;
pub const V2_IE_APN_RESTRICTION: u8 = 127;

/// Human name for the GTPv2-C IEs this server surfaces.
pub fn v2_ie_name(ie_type: u8) -> &'static str {
    match ie_type {
        1 => "IMSI",
        2 => "Cause",
        3 => "Recovery",
        71 => "APN",
        72 => "AMBR",
        73 => "EPS Bearer ID",
        74 => "IPv4 Configuration Parameters",
        75 => "MEI",
        76 => "MSISDN",
        77 => "Indication",
        78 => "Protocol Configuration Options",
        79 => "PDN Address Allocation",
        80 => "Bearer QoS",
        82 => "RAT Type",
        83 => "Serving Network",
        86 => "User Location Information",
        87 => "Fully Qualified TEID",
        93 => "Bearer Context",
        94 => "Charging ID",
        99 => "PDN Type",
        127 => "APN Restriction",
        255 => "Private Extension",
        _ => "Unknown",
    }
}

// ===========================================================================
// Decode errors
// ===========================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer octets than the mandatory header needs.
    TooShort { len: usize, needed: usize },
    /// The three version bits named a version this server does not speak.
    BadVersion { version: u8 },
    /// The header's length field disagrees with the datagram.
    LengthMismatch { declared: usize, available: usize },
    /// An extension header's length octet was zero, or ran past the datagram.
    BadExtensionHeader { ext_type: u8 },
    /// An information element ran past the end of the message body.
    TruncatedIe { ie_type: u8 },
    /// A GTPv1 IE below 128 whose fixed length is not in [`v1_fixed_ie_len`]. It carries no
    /// length on the wire, so there is no way to skip it; guessing would misread the rest.
    UnknownFixedIe { ie_type: u8 },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooShort { len, needed } => {
                write!(f, "datagram is {len} octets, needs at least {needed}")
            }
            DecodeError::BadVersion { version } => {
                write!(
                    f,
                    "GTP version {version} is not supported (expected 1 or 2)"
                )
            }
            DecodeError::LengthMismatch {
                declared,
                available,
            } => write!(
                f,
                "header declares {declared} octets of payload but {available} are present"
            ),
            DecodeError::BadExtensionHeader { ext_type } => {
                write!(f, "malformed extension header of type {ext_type}")
            }
            DecodeError::TruncatedIe { ie_type } => {
                write!(f, "information element {ie_type} runs past the message")
            }
            DecodeError::UnknownFixedIe { ie_type } => write!(
                f,
                "information element {ie_type} is below 128 (fixed length) but its length is \
                 not known, so the rest of the message cannot be parsed"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Which GTP version a datagram announced in its first three bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GtpVersion {
    V1,
    V2,
}

impl GtpVersion {
    pub fn as_number(self) -> u8 {
        match self {
            GtpVersion::V1 => 1,
            GtpVersion::V2 => 2,
        }
    }
}

/// The version bits of a datagram, without decoding anything else.
///
/// Returns `None` for an empty datagram. A value other than 1 or 2 is returned as-is so the
/// caller can answer "Version Not Supported" rather than dropping silently.
pub fn peek_version(data: &[u8]) -> Option<u8> {
    data.first().map(|b| b >> 5)
}

// ===========================================================================
// GTPv1 header
// ===========================================================================

/// One GTPv1 extension header (TS 29.060 §6.1).
///
/// On the wire an extension header is `length | content | next-type`, where `length` counts
/// **4-octet units** and covers all three parts. `content` here is those middle octets, so
/// `content.len() + 2` must be a multiple of 4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionHeader {
    pub ext_type: u8,
    pub content: Vec<u8>,
}

/// GTPv1 header (TS 29.060 §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GtpV1Header {
    /// PT bit: `true` = GTP (this specification), `false` = GTP' (charging, TS 32.295).
    pub protocol_type: bool,
    pub message_type: u8,
    pub teid: u32,
    /// Present exactly when the S flag is set.
    pub sequence: Option<u16>,
    /// Present exactly when the PN flag is set.
    pub npdu: Option<u8>,
    /// Non-empty exactly when the E flag is set.
    pub extension_headers: Vec<ExtensionHeader>,
}

impl GtpV1Header {
    /// A minimal header: no sequence, no N-PDU number, no extension headers.
    pub fn new(message_type: u8, teid: u32) -> Self {
        Self {
            protocol_type: true,
            message_type,
            teid,
            sequence: None,
            npdu: None,
            extension_headers: Vec::new(),
        }
    }

    /// The same, with a sequence number — which every GTP-C signalling message carries.
    pub fn with_sequence(message_type: u8, teid: u32, sequence: u16) -> Self {
        Self {
            sequence: Some(sequence),
            ..Self::new(message_type, teid)
        }
    }

    /// **The all-or-nothing rule.** The four optional octets are present whenever *any* of
    /// E, S or PN is set — not only the flagged one.
    pub fn optional_present(&self) -> bool {
        self.sequence.is_some() || self.npdu.is_some() || !self.extension_headers.is_empty()
    }

    /// The flags octet: `version(3) | PT(1) | reserved(1) | E(1) | S(1) | PN(1)`.
    pub fn flags(&self) -> u8 {
        let mut flags = 0b0010_0000; // version 1
        if self.protocol_type {
            flags |= 0b0001_0000;
        }
        if !self.extension_headers.is_empty() {
            flags |= 0b0000_0100;
        }
        if self.sequence.is_some() {
            flags |= 0b0000_0010;
        }
        if self.npdu.is_some() {
            flags |= 0b0000_0001;
        }
        flags
    }
}

/// A decoded GTPv1 datagram.
///
/// `body` is everything after the header and its extension headers: the information elements
/// of a control-plane message, or the encapsulated user IP packet of a G-PDU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GtpV1Message {
    pub header: GtpV1Header,
    pub body: Vec<u8>,
}

impl GtpV1Message {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        if data.len() < 8 {
            return Err(DecodeError::TooShort {
                len: data.len(),
                needed: 8,
            });
        }
        let flags = data[0];
        let version = flags >> 5;
        if version != 1 {
            return Err(DecodeError::BadVersion { version });
        }
        let protocol_type = flags & 0b0001_0000 != 0;
        let e = flags & 0b0000_0100 != 0;
        let s = flags & 0b0000_0010 != 0;
        let pn = flags & 0b0000_0001 != 0;

        let message_type = data[1];
        let declared = u16::from_be_bytes([data[2], data[3]]) as usize;
        let teid = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

        // The length field counts everything after the mandatory 8 octets, so the optional
        // block and every extension header are inside it.
        let available = data.len() - 8;
        if declared > available {
            return Err(DecodeError::LengthMismatch {
                declared,
                available,
            });
        }
        // Trailing octets past the declared length are not ours (UDP padding, a coalesced
        // read); cut them off rather than feeding them to the IE parser.
        let payload = &data[8..8 + declared];

        let mut pos = 0usize;
        let (sequence, npdu, mut next_ext) = if e || s || pn {
            // *** The all-or-nothing rule: four octets, whatever the individual flags say. ***
            if payload.len() < 4 {
                return Err(DecodeError::TooShort {
                    len: data.len(),
                    needed: 12,
                });
            }
            let seq = u16::from_be_bytes([payload[0], payload[1]]);
            let np = payload[2];
            let nx = payload[3];
            pos = 4;
            (
                if s { Some(seq) } else { None },
                if pn { Some(np) } else { None },
                if e { nx } else { 0 },
            )
        } else {
            (None, None, 0)
        };

        let mut extension_headers = Vec::new();
        while next_ext != 0 {
            if pos >= payload.len() {
                return Err(DecodeError::BadExtensionHeader { ext_type: next_ext });
            }
            let units = payload[pos] as usize;
            if units == 0 {
                return Err(DecodeError::BadExtensionHeader { ext_type: next_ext });
            }
            let total = units * 4;
            if pos + total > payload.len() {
                return Err(DecodeError::BadExtensionHeader { ext_type: next_ext });
            }
            let content = payload[pos + 1..pos + total - 1].to_vec();
            extension_headers.push(ExtensionHeader {
                ext_type: next_ext,
                content,
            });
            next_ext = payload[pos + total - 1];
            pos += total;
        }

        Ok(GtpV1Message {
            header: GtpV1Header {
                protocol_type,
                message_type,
                teid,
                sequence,
                npdu,
                extension_headers,
            },
            body: payload[pos..].to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let mut tail = Vec::new();

        if h.optional_present() {
            // *** The all-or-nothing rule again, on the encode side. A field whose flag is
            // clear is still written, as zero. ***
            tail.extend_from_slice(&h.sequence.unwrap_or(0).to_be_bytes());
            tail.push(h.npdu.unwrap_or(0));
            tail.push(h.extension_headers.first().map(|x| x.ext_type).unwrap_or(0));
        }

        for (i, ext) in h.extension_headers.iter().enumerate() {
            let total = ext.content.len() + 2;
            debug_assert_eq!(total % 4, 0, "extension header must be a multiple of 4");
            tail.push((total / 4) as u8);
            tail.extend_from_slice(&ext.content);
            tail.push(
                h.extension_headers
                    .get(i + 1)
                    .map(|x| x.ext_type)
                    .unwrap_or(0),
            );
        }

        tail.extend_from_slice(&self.body);

        let mut out = Vec::with_capacity(8 + tail.len());
        out.push(h.flags());
        out.push(h.message_type);
        out.extend_from_slice(&(tail.len() as u16).to_be_bytes());
        out.extend_from_slice(&h.teid.to_be_bytes());
        out.extend_from_slice(&tail);
        out
    }
}

// ===========================================================================
// GTPv1 information elements
// ===========================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GtpV1Ie {
    pub ie_type: u8,
    pub value: Vec<u8>,
}

/// Parse a GTPv1-C message body into information elements.
///
/// **The fixed-vs-TLV split is the whole point.** Types 1..=127 carry their value immediately
/// after the type octet with a length known only from the specification; types 128..=255 carry
/// a two-octet big-endian length.
pub fn parse_v1_ies(body: &[u8]) -> Result<Vec<GtpV1Ie>, DecodeError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < body.len() {
        let ie_type = body[pos];
        if ie_type < 128 {
            let len = v1_fixed_ie_len(ie_type).ok_or(DecodeError::UnknownFixedIe { ie_type })?;
            if pos + 1 + len > body.len() {
                return Err(DecodeError::TruncatedIe { ie_type });
            }
            out.push(GtpV1Ie {
                ie_type,
                value: body[pos + 1..pos + 1 + len].to_vec(),
            });
            pos += 1 + len;
        } else {
            if pos + 3 > body.len() {
                return Err(DecodeError::TruncatedIe { ie_type });
            }
            let len = u16::from_be_bytes([body[pos + 1], body[pos + 2]]) as usize;
            if pos + 3 + len > body.len() {
                return Err(DecodeError::TruncatedIe { ie_type });
            }
            out.push(GtpV1Ie {
                ie_type,
                value: body[pos + 3..pos + 3 + len].to_vec(),
            });
            pos += 3 + len;
        }
    }
    Ok(out)
}

/// Serialise information elements. Fixed types emit no length; 128+ emit a two-octet length.
pub fn encode_v1_ies(ies: &[GtpV1Ie]) -> Vec<u8> {
    let mut out = Vec::new();
    for ie in ies {
        out.push(ie.ie_type);
        if ie.ie_type >= 128 {
            out.extend_from_slice(&(ie.value.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(&ie.value);
    }
    out
}

/// First IE of a type, if present.
pub fn find_v1(ies: &[GtpV1Ie], ie_type: u8) -> Option<&[u8]> {
    ies.iter()
        .find(|ie| ie.ie_type == ie_type)
        .map(|ie| ie.value.as_slice())
}

/// Every IE of a type, in order.
pub fn find_all_v1(ies: &[GtpV1Ie], ie_type: u8) -> Vec<&[u8]> {
    ies.iter()
        .filter(|ie| ie.ie_type == ie_type)
        .map(|ie| ie.value.as_slice())
        .collect()
}

// ===========================================================================
// GTPv2-C
// ===========================================================================

/// GTPv2-C header (TS 29.274 §5.1).
///
/// The differences from v1 that matter: version bits are `010`, the TEID is **conditional**
/// on the T flag rather than always present, and the sequence number is 24 bits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GtpV2Header {
    pub piggyback: bool,
    pub message_priority: Option<u8>,
    pub message_type: u8,
    /// Present exactly when the T flag is set.
    pub teid: Option<u32>,
    /// 24-bit sequence number.
    pub sequence: u32,
}

impl GtpV2Header {
    pub fn new(message_type: u8, teid: u32, sequence: u32) -> Self {
        Self {
            piggyback: false,
            message_priority: None,
            message_type,
            teid: Some(teid),
            sequence: sequence & 0x00FF_FFFF,
        }
    }

    /// Echo Request/Response never carry a TEID (TS 29.274 §5.1: the T flag is 0).
    pub fn without_teid(message_type: u8, sequence: u32) -> Self {
        Self {
            teid: None,
            ..Self::new(message_type, 0, sequence)
        }
    }

    pub fn flags(&self) -> u8 {
        let mut flags = 0b0100_0000; // version 2
        if self.piggyback {
            flags |= 0b0001_0000;
        }
        if self.teid.is_some() {
            flags |= 0b0000_1000;
        }
        if self.message_priority.is_some() {
            flags |= 0b0000_0100;
        }
        flags
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GtpV2Ie {
    pub ie_type: u8,
    /// Which occurrence of the type this is, within its grouping (TS 29.274 §6.1.3).
    pub instance: u8,
    pub value: Vec<u8>,
}

impl GtpV2Ie {
    pub fn new(ie_type: u8, instance: u8, value: Vec<u8>) -> Self {
        Self {
            ie_type,
            instance,
            value,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GtpV2Message {
    pub header: GtpV2Header,
    pub ies: Vec<GtpV2Ie>,
}

impl GtpV2Message {
    pub fn decode(data: &[u8]) -> Result<Self, DecodeError> {
        if data.len() < 4 {
            return Err(DecodeError::TooShort {
                len: data.len(),
                needed: 4,
            });
        }
        let flags = data[0];
        let version = flags >> 5;
        if version != 2 {
            return Err(DecodeError::BadVersion { version });
        }
        let piggyback = flags & 0b0001_0000 != 0;
        let t = flags & 0b0000_1000 != 0;
        let mp = flags & 0b0000_0100 != 0;
        let message_type = data[1];
        let declared = u16::from_be_bytes([data[2], data[3]]) as usize;

        let available = data.len() - 4;
        if declared > available {
            return Err(DecodeError::LengthMismatch {
                declared,
                available,
            });
        }
        let rest = &data[4..4 + declared];

        // TEID (4, conditional) + sequence (3) + spare/message-priority (1).
        let fixed = if t { 8 } else { 4 };
        if rest.len() < fixed {
            return Err(DecodeError::TooShort {
                len: data.len(),
                needed: 4 + fixed,
            });
        }
        let mut pos = 0usize;
        let teid = if t {
            let v = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
            pos = 4;
            Some(v)
        } else {
            None
        };
        let sequence =
            u32::from_be_bytes([0, rest[pos], rest[pos + 1], rest[pos + 2]]) & 0x00FF_FFFF;
        let last = rest[pos + 3];
        pos += 4;

        let mut ies = Vec::new();
        while pos < rest.len() {
            if pos + 4 > rest.len() {
                return Err(DecodeError::TruncatedIe { ie_type: rest[pos] });
            }
            let ie_type = rest[pos];
            let len = u16::from_be_bytes([rest[pos + 1], rest[pos + 2]]) as usize;
            let instance = rest[pos + 3] & 0x0F;
            if pos + 4 + len > rest.len() {
                return Err(DecodeError::TruncatedIe { ie_type });
            }
            ies.push(GtpV2Ie {
                ie_type,
                instance,
                value: rest[pos + 4..pos + 4 + len].to_vec(),
            });
            pos += 4 + len;
        }

        Ok(GtpV2Message {
            header: GtpV2Header {
                piggyback,
                message_priority: if mp { Some(last >> 4) } else { None },
                message_type,
                teid,
                sequence,
            },
            ies,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut rest = Vec::new();
        if let Some(teid) = self.header.teid {
            rest.extend_from_slice(&teid.to_be_bytes());
        }
        let seq = self.header.sequence.to_be_bytes();
        rest.extend_from_slice(&seq[1..4]);
        rest.push(self.header.message_priority.map(|p| p << 4).unwrap_or(0));

        for ie in &self.ies {
            rest.push(ie.ie_type);
            rest.extend_from_slice(&(ie.value.len() as u16).to_be_bytes());
            rest.push(ie.instance & 0x0F);
            rest.extend_from_slice(&ie.value);
        }

        let mut out = Vec::with_capacity(4 + rest.len());
        out.push(self.header.flags());
        out.push(self.header.message_type);
        out.extend_from_slice(&(rest.len() as u16).to_be_bytes());
        out.extend_from_slice(&rest);
        out
    }

    pub fn find(&self, ie_type: u8) -> Option<&[u8]> {
        self.ies
            .iter()
            .find(|ie| ie.ie_type == ie_type)
            .map(|ie| ie.value.as_slice())
    }

    pub fn find_instance(&self, ie_type: u8, instance: u8) -> Option<&[u8]> {
        self.ies
            .iter()
            .find(|ie| ie.ie_type == ie_type && ie.instance == instance)
            .map(|ie| ie.value.as_slice())
    }
}

/// Parse the inner IEs of a grouped GTPv2 IE (Bearer Context and friends).
pub fn parse_v2_grouped(value: &[u8]) -> Result<Vec<GtpV2Ie>, DecodeError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < value.len() {
        if pos + 4 > value.len() {
            return Err(DecodeError::TruncatedIe {
                ie_type: value[pos],
            });
        }
        let ie_type = value[pos];
        let len = u16::from_be_bytes([value[pos + 1], value[pos + 2]]) as usize;
        let instance = value[pos + 3] & 0x0F;
        if pos + 4 + len > value.len() {
            return Err(DecodeError::TruncatedIe { ie_type });
        }
        out.push(GtpV2Ie {
            ie_type,
            instance,
            value: value[pos + 4..pos + 4 + len].to_vec(),
        });
        pos += 4 + len;
    }
    Ok(out)
}

/// Serialise IEs for use as the value of a grouped GTPv2 IE.
pub fn encode_v2_grouped(ies: &[GtpV2Ie]) -> Vec<u8> {
    let mut out = Vec::new();
    for ie in ies {
        out.push(ie.ie_type);
        out.extend_from_slice(&(ie.value.len() as u16).to_be_bytes());
        out.push(ie.instance & 0x0F);
        out.extend_from_slice(&ie.value);
    }
    out
}

// ===========================================================================
// Subscriber identifiers, APNs and addresses
// ===========================================================================

/// Encode decimal digits as TBCD (telephony BCD): two digits per octet, **low nibble first**,
/// odd counts padded with the filler `0xF`. Used for IMSI, MSISDN and IMEI.
///
/// Non-digit characters are dropped rather than encoded — an IMSI is digits by definition.
pub fn encode_tbcd(digits: &str) -> Vec<u8> {
    let nibbles: Vec<u8> = digits
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();
    let mut out = Vec::with_capacity(nibbles.len().div_ceil(2));
    for pair in nibbles.chunks(2) {
        let low = pair[0];
        let high = pair.get(1).copied().unwrap_or(0x0F);
        out.push((high << 4) | low);
    }
    out
}

/// Decode TBCD back to digits, stopping at the `0xF` filler.
pub fn decode_tbcd(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        for nibble in [b & 0x0F, b >> 4] {
            if nibble == 0x0F {
                return out;
            }
            if nibble > 9 {
                // Not a decimal digit; the field is not TBCD after all.
                return out;
            }
            out.push((b'0' + nibble) as char);
        }
    }
    out
}

/// Encode an APN as DNS-style labels: one length octet per label, no trailing root label
/// (TS 23.003 §9.1, TS 29.060 §7.7.30).
pub fn encode_apn(apn: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(apn.len() + 4);
    for label in apn.split('.').filter(|l| !l.is_empty()) {
        let bytes = label.as_bytes();
        let len = bytes.len().min(63);
        out.push(len as u8);
        out.extend_from_slice(&bytes[..len]);
    }
    out
}

/// Decode a labelled APN back to dotted form. Returns `None` if the labels do not tile the
/// buffer exactly, which means it was not an APN.
pub fn decode_apn(value: &[u8]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos < value.len() {
        let len = value[pos] as usize;
        if len == 0 || pos + 1 + len > value.len() {
            return None;
        }
        parts.push(String::from_utf8_lossy(&value[pos + 1..pos + 1 + len]).to_string());
        pos += 1 + len;
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

/// GTPv1 End User Address (IE 128) for an IETF-allocated PDP address.
///
/// Octet 1 is `1111 0001` — four spare bits set to 1, then PDP Type Organisation 1 (IETF);
/// octet 2 is the PDP Type Number (0x21 IPv4, 0x57 IPv6, 0x8D IPv4v6); the address follows.
pub fn encode_end_user_address(addr: Option<IpAddr>) -> Vec<u8> {
    match addr {
        Some(IpAddr::V4(v4)) => {
            let mut out = vec![0xF1, 0x21];
            out.extend_from_slice(&v4.octets());
            out
        }
        Some(IpAddr::V6(v6)) => {
            let mut out = vec![0xF1, 0x57];
            out.extend_from_slice(&v6.octets());
            out
        }
        // A dynamic address the network has not chosen yet: type only, no address.
        None => vec![0xF1, 0x21],
    }
}

/// Decode a GTPv1 End User Address. Returns `(pdp_type, address)`; the address is `None` for
/// the "give me a dynamic one" form a UE sends in a Create PDP Context Request.
pub fn decode_end_user_address(value: &[u8]) -> (&'static str, Option<IpAddr>) {
    if value.len() < 2 {
        return ("unknown", None);
    }
    let (name, expected) = match value[1] {
        0x21 => ("IPv4", 4usize),
        0x57 => ("IPv6", 16),
        0x8D => ("IPv4v6", 4),
        _ => ("unknown", 0),
    };
    let rest = &value[2..];
    if expected == 4 && rest.len() >= 4 {
        (
            name,
            Some(IpAddr::V4(Ipv4Addr::new(
                rest[0], rest[1], rest[2], rest[3],
            ))),
        )
    } else if expected == 16 && rest.len() >= 16 {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&rest[..16]);
        (name, Some(IpAddr::V6(Ipv6Addr::from(octets))))
    } else {
        (name, None)
    }
}

/// GTPv2-C PDN Address Allocation (IE 79). PDN type 1 = IPv4, 2 = IPv6, 3 = IPv4v6.
pub fn encode_paa(addr: IpAddr) -> Vec<u8> {
    match addr {
        IpAddr::V4(v4) => {
            let mut out = vec![1u8];
            out.extend_from_slice(&v4.octets());
            out
        }
        IpAddr::V6(v6) => {
            // TS 29.274 §8.14: an IPv6 PAA is a prefix length followed by the prefix.
            let mut out = vec![2u8, 64];
            out.extend_from_slice(&v6.octets());
            out
        }
    }
}

/// Decode a GTPv2-C PAA. Returns `(pdn_type, address)`.
pub fn decode_paa(value: &[u8]) -> (&'static str, Option<IpAddr>) {
    match value.first() {
        Some(1) if value.len() >= 5 => (
            "IPv4",
            Some(IpAddr::V4(Ipv4Addr::new(
                value[1], value[2], value[3], value[4],
            ))),
        ),
        Some(2) if value.len() >= 18 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[2..18]);
            ("IPv6", Some(IpAddr::V6(Ipv6Addr::from(octets))))
        }
        Some(3) if value.len() >= 22 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[2..18]);
            (
                "IPv4v6",
                Some(IpAddr::V4(Ipv4Addr::new(
                    value[18], value[19], value[20], value[21],
                ))),
            )
        }
        Some(1) => ("IPv4", None),
        Some(2) => ("IPv6", None),
        Some(3) => ("IPv4v6", None),
        _ => ("unknown", None),
    }
}

/// GTPv2-C Fully Qualified TEID (IE 87), TS 29.274 §8.22.
///
/// Octet 1 is `V4 | V6 | interface-type(6 bits)`, then the 4-octet TEID/GRE key, then the
/// addresses the flags announced.
pub fn encode_fteid(interface_type: u8, teid: u32, addr: IpAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(21);
    let flag = match addr {
        IpAddr::V4(_) => 0x80,
        IpAddr::V6(_) => 0x40,
    };
    out.push(flag | (interface_type & 0x3F));
    out.extend_from_slice(&teid.to_be_bytes());
    match addr {
        IpAddr::V4(v4) => out.extend_from_slice(&v4.octets()),
        IpAddr::V6(v6) => out.extend_from_slice(&v6.octets()),
    }
    out
}

/// Decode an F-TEID. Returns `(interface_type, teid, address)`.
pub fn decode_fteid(value: &[u8]) -> Option<(u8, u32, Option<IpAddr>)> {
    if value.len() < 5 {
        return None;
    }
    let interface_type = value[0] & 0x3F;
    let teid = u32::from_be_bytes([value[1], value[2], value[3], value[4]]);
    let has_v4 = value[0] & 0x80 != 0;
    let has_v6 = value[0] & 0x40 != 0;
    let addr = if has_v4 && value.len() >= 9 {
        Some(IpAddr::V4(Ipv4Addr::new(
            value[5], value[6], value[7], value[8],
        )))
    } else if has_v6 && value.len() >= 21 {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&value[5..21]);
        Some(IpAddr::V6(Ipv6Addr::from(octets)))
    } else {
        None
    };
    Some((interface_type, teid, addr))
}

/// Protocol Configuration Options carrying DNS server addresses in the network-to-MS
/// direction (TS 24.008 §10.5.6.3; GTPv1 IE 132, GTPv2 IE 78).
///
/// Octet 1 is `1000 0000`: extension bit set, configuration protocol 000 (PPP). Each
/// container is a two-octet identifier, a one-octet length and its contents; `0x000D` is
/// "DNS Server IPv4 Address" and `0x0003` is "DNS Server IPv6 Address".
pub fn encode_pco_dns(servers: &[IpAddr]) -> Vec<u8> {
    let mut out = vec![0x80u8];
    for server in servers {
        match server {
            IpAddr::V4(v4) => {
                out.extend_from_slice(&[0x00, 0x0D, 0x04]);
                out.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                out.extend_from_slice(&[0x00, 0x03, 0x10]);
                out.extend_from_slice(&v6.octets());
            }
        }
    }
    out
}

/// Radio access technology name (TS 29.060 §7.7.50 / TS 29.274 §8.17). The two
/// specifications number these differently, so the version has to be passed in.
pub fn rat_type_name(version: GtpVersion, value: u8) -> &'static str {
    match (version, value) {
        (GtpVersion::V1, 1) => "UTRAN",
        (GtpVersion::V1, 2) => "GERAN",
        (GtpVersion::V1, 3) => "WLAN",
        (GtpVersion::V1, 4) => "GAN",
        (GtpVersion::V1, 5) => "HSPA Evolution",
        (GtpVersion::V1, 6) => "EUTRAN",
        (GtpVersion::V2, 1) => "UTRAN",
        (GtpVersion::V2, 2) => "GERAN",
        (GtpVersion::V2, 3) => "WLAN",
        (GtpVersion::V2, 4) => "GAN",
        (GtpVersion::V2, 5) => "HSPA Evolution",
        (GtpVersion::V2, 6) => "EUTRAN",
        (GtpVersion::V2, 8) => "EUTRAN-NB-IoT",
        (GtpVersion::V2, 9) => "LTE-M",
        (GtpVersion::V2, 10) => "NR",
        _ => "unknown",
    }
}

// ===========================================================================
// Cause values
// ===========================================================================

/// A cause the model can name, with its code in each version.
///
/// Only causes that exist in **both** TS 29.060 Table 38 and TS 29.274 Table 8.4-1 are
/// offered, so the same name is answerable whichever version the peer used. The numeric
/// escape hatch on the action covers everything else.
pub struct CauseName {
    pub name: &'static str,
    pub v1: u8,
    pub v2: u8,
    /// True only for the causes that *grant* something. Used by the fail-closed rule, which
    /// must never synthesise one of these.
    pub accepts: bool,
}

pub const CAUSES: &[CauseName] = &[
    CauseName {
        name: "request_accepted",
        v1: 128,
        v2: 16,
        accepts: true,
    },
    CauseName {
        name: "new_pdp_type_due_to_network_preference",
        v1: 129,
        v2: 18,
        accepts: true,
    },
    CauseName {
        name: "new_pdp_type_due_to_single_address_bearer_only",
        v1: 130,
        v2: 19,
        accepts: true,
    },
    CauseName {
        name: "invalid_message_format",
        v1: 193,
        v2: 65,
        accepts: false,
    },
    CauseName {
        name: "imsi_not_known",
        v1: 194,
        v2: 95,
        accepts: false,
    },
    CauseName {
        name: "version_not_supported",
        v1: 198,
        v2: 66,
        accepts: false,
    },
    CauseName {
        name: "no_resources_available",
        v1: 199,
        v2: 73,
        accepts: false,
    },
    CauseName {
        name: "service_not_supported",
        v1: 200,
        v2: 68,
        accepts: false,
    },
    CauseName {
        name: "mandatory_ie_incorrect",
        v1: 201,
        v2: 69,
        accepts: false,
    },
    CauseName {
        name: "mandatory_ie_missing",
        v1: 202,
        v2: 70,
        accepts: false,
    },
    CauseName {
        name: "system_failure",
        v1: 204,
        v2: 72,
        accepts: false,
    },
    CauseName {
        name: "user_authentication_failed",
        v1: 209,
        v2: 91,
        accepts: false,
    },
    CauseName {
        name: "context_not_found",
        v1: 210,
        v2: 64,
        accepts: false,
    },
    CauseName {
        name: "all_dynamic_addresses_are_occupied",
        v1: 211,
        v2: 83,
        accepts: false,
    },
    CauseName {
        name: "no_memory_available",
        v1: 212,
        v2: 90,
        accepts: false,
    },
    CauseName {
        name: "missing_or_unknown_apn",
        v1: 219,
        v2: 77,
        accepts: false,
    },
    CauseName {
        name: "apn_access_denied_no_subscription",
        v1: 222,
        v2: 92,
        accepts: false,
    },
];

/// Look a cause up by name, tolerating the spellings a model actually produces:
/// `"Request accepted"`, `"REQUEST_ACCEPTED"`, `"request-accepted"`.
pub fn cause_by_name(name: &str) -> Option<&'static CauseName> {
    let normalised: String = name
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == ' ' || c == '-' || c == '_' {
                Some('_')
            } else {
                None
            }
        })
        .collect();
    CAUSES.iter().find(|c| c.name == normalised)
}

/// Every cause name the model may use, for error messages and documentation.
pub fn cause_names() -> Vec<&'static str> {
    CAUSES.iter().map(|c| c.name).collect()
}

/// Does this numeric cause value grant something?
///
/// TS 29.060 §7.7.1 splits the v1 space into request (0-63), accept (128-191) and reject
/// (192-255); TS 29.274 §8.4 splits the v2 space into request (0-15), accept (16-63) and
/// reject (64-255).
pub fn cause_accepts(version: GtpVersion, value: u8) -> bool {
    match version {
        GtpVersion::V1 => (128..=191).contains(&value),
        GtpVersion::V2 => (16..=63).contains(&value),
    }
}

// ===========================================================================
// Inner IP header of a G-PDU
// ===========================================================================

/// The decoded header of the user packet a G-PDU carries.
///
/// Structured deliberately: the root `CLAUDE.md` rule is that a model cannot reliably parse
/// bytes, so it is handed addresses, a protocol name and ports, never a blob to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InnerIp {
    pub version: u8,
    pub source: IpAddr,
    pub destination: IpAddr,
    pub protocol: u8,
    pub protocol_name: &'static str,
    /// TTL for IPv4, Hop Limit for IPv6.
    pub ttl: u8,
    /// Total length for IPv4 (header included), payload length for IPv6.
    pub length: u16,
    pub source_port: Option<u16>,
    pub destination_port: Option<u16>,
    /// Offset of the transport payload within the inner packet, when it could be found.
    pub payload_offset: Option<usize>,
}

pub fn ip_protocol_name(protocol: u8) -> &'static str {
    match protocol {
        1 => "ICMP",
        2 => "IGMP",
        6 => "TCP",
        17 => "UDP",
        41 => "IPv6",
        47 => "GRE",
        50 => "ESP",
        51 => "AH",
        58 => "ICMPv6",
        89 => "OSPF",
        132 => "SCTP",
        _ => "unknown",
    }
}

/// Decode the header of an encapsulated user IP packet. `None` when it is not IP at all.
pub fn decode_inner_ip(packet: &[u8]) -> Option<InnerIp> {
    let first = *packet.first()?;
    match first >> 4 {
        4 => {
            if packet.len() < 20 {
                return None;
            }
            let ihl = (first & 0x0F) as usize * 4;
            if ihl < 20 {
                return None;
            }
            let protocol = packet[9];
            let source = IpAddr::V4(Ipv4Addr::new(
                packet[12], packet[13], packet[14], packet[15],
            ));
            let destination = IpAddr::V4(Ipv4Addr::new(
                packet[16], packet[17], packet[18], packet[19],
            ));
            let (source_port, destination_port, payload_offset) =
                transport_ports(packet, ihl, protocol);
            Some(InnerIp {
                version: 4,
                source,
                destination,
                protocol,
                protocol_name: ip_protocol_name(protocol),
                ttl: packet[8],
                length: u16::from_be_bytes([packet[2], packet[3]]),
                source_port,
                destination_port,
                payload_offset,
            })
        }
        6 => {
            if packet.len() < 40 {
                return None;
            }
            let protocol = packet[6];
            let mut s = [0u8; 16];
            s.copy_from_slice(&packet[8..24]);
            let mut d = [0u8; 16];
            d.copy_from_slice(&packet[24..40]);
            let (source_port, destination_port, payload_offset) =
                transport_ports(packet, 40, protocol);
            Some(InnerIp {
                version: 6,
                source: IpAddr::V6(Ipv6Addr::from(s)),
                destination: IpAddr::V6(Ipv6Addr::from(d)),
                protocol,
                protocol_name: ip_protocol_name(protocol),
                ttl: packet[7],
                length: u16::from_be_bytes([packet[4], packet[5]]),
                source_port,
                destination_port,
                payload_offset,
            })
        }
        _ => None,
    }
}

fn transport_ports(
    packet: &[u8],
    offset: usize,
    protocol: u8,
) -> (Option<u16>, Option<u16>, Option<usize>) {
    if !matches!(protocol, 6 | 17 | 132) || packet.len() < offset + 4 {
        return (None, None, None);
    }
    let sport = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
    let dport = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
    let payload_offset = match protocol {
        17 => Some(offset + 8),
        6 if packet.len() > offset + 12 => Some(offset + ((packet[offset + 12] >> 4) as usize * 4)),
        _ => None,
    };
    (Some(sport), Some(dport), payload_offset)
}
