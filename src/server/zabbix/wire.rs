//! The Zabbix trapper wire format: the `ZBXD` header, the `sender data` request, and the
//! response `zabbix_sender` parses.
//!
//! Everything here is a pure function of its arguments, shared by the session loop, the action
//! executor, the tests and the `zabbix_packet` fuzz target. **The model never writes the
//! response**: it supplies two counts, and [`render_result`] writes the JSON and the `info`
//! string `processed: P; failed: F; total: T; seconds spent: S` that `zabbix_sender` scans with
//! `sscanf` to choose its exit status.
//!
//! The header is Zabbix's "Zabbix protocol" framing (Zabbix 4.0+):
//!
//! ```text
//! "ZBXD" | flags (1) | data length | reserved
//!                      4 bytes LE    4 bytes LE      (flags without 0x04)
//!                      8 bytes LE    8 bytes LE      (flags with 0x04, "large packet")
//! ```
//!
//! `0x01` must be set; `0x02` means the data is zlib-compressed and `reserved` is its
//! uncompressed size. NetGet refuses compressed packets: `zabbix_sender` 7.4 does not compress
//! (measured — it sends flags `0x01`), and refusing removes the one path where the bytes read
//! and the bytes processed differ.

use serde_json::Value;

pub const MAGIC: &[u8; 4] = b"ZBXD";
pub const FLAG_PROTOCOL: u8 = 0x01;
pub const FLAG_COMPRESSED: u8 = 0x02;
pub const FLAG_LARGE: u8 = 0x04;

/// Header length without and with [`FLAG_LARGE`].
pub const HEADER_LEN: usize = 13;
pub const LARGE_HEADER_LEN: usize = 21;

/// The largest request body NetGet reads: 1 MiB.
///
/// The protocol permits 1 GiB (Zabbix's `ZBX_MAX_RECV_DATA_SIZE`), which is a limit for a
/// server that streams values into a database. Every byte here goes into one model prompt as
/// the event's item list, and `zabbix_sender` splits its input into requests of at most 250
/// values, so 1 MiB is ~4 KiB per value at the sender's own batch size — far past a metric, a
/// log line or a short text item — while a gigabyte would be a prompt no backend accepts.
/// Checked against the **declared** length before anything is allocated for it.
pub const MAX_DATA_BYTES: usize = 1024 * 1024;

/// The most values one request may carry. Four times `zabbix_sender`'s own batch of 250, for
/// other senders; past it the request is refused rather than handed to the model.
pub const MAX_ITEMS: usize = 1000;

/// Why a header was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    /// The first four bytes are not `ZBXD`.
    BadMagic,
    /// `0x01` is not set, or a bit other than `0x01`/`0x02`/`0x04` is.
    BadFlags(u8),
    /// `0x02`: zlib-compressed data, which NetGet does not accept.
    Compressed,
    /// The declared data length is past [`MAX_DATA_BYTES`].
    TooLarge(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub data_len: u64,
    pub reserved: u64,
}

/// How long the header is, given its flags byte.
pub fn header_len(flags: u8) -> usize {
    if flags & FLAG_LARGE != 0 {
        LARGE_HEADER_LEN
    } else {
        HEADER_LEN
    }
}

/// Parse a complete header (`bytes.len() >= header_len(bytes[4])`). The length is judged here,
/// before the caller allocates anything for the body.
pub fn parse_header(bytes: &[u8]) -> Result<Header, HeaderError> {
    if bytes.len() < 5 || &bytes[..4] != MAGIC {
        return Err(HeaderError::BadMagic);
    }
    let flags = bytes[4];
    if flags & FLAG_PROTOCOL == 0 || flags & !(FLAG_PROTOCOL | FLAG_COMPRESSED | FLAG_LARGE) != 0 {
        return Err(HeaderError::BadFlags(flags));
    }
    let need = header_len(flags);
    if bytes.len() < need {
        return Err(HeaderError::BadMagic);
    }
    let (data_len, reserved) = if flags & FLAG_LARGE != 0 {
        (
            u64::from_le_bytes(bytes[5..13].try_into().unwrap()),
            u64::from_le_bytes(bytes[13..21].try_into().unwrap()),
        )
    } else {
        (
            u64::from(u32::from_le_bytes(bytes[5..9].try_into().unwrap())),
            u64::from(u32::from_le_bytes(bytes[9..13].try_into().unwrap())),
        )
    };
    if flags & FLAG_COMPRESSED != 0 {
        return Err(HeaderError::Compressed);
    }
    if data_len > MAX_DATA_BYTES as u64 {
        return Err(HeaderError::TooLarge(data_len));
    }
    Ok(Header {
        flags,
        data_len,
        reserved,
    })
}

/// Frame `payload` as Zabbix does for an uncompressed packet that fits the standard header.
pub fn encode(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(FLAG_PROTOCOL);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Frame `payload` with the large (8-byte length) header.
pub fn encode_large(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LARGE_HEADER_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(FLAG_PROTOCOL | FLAG_LARGE);
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// One value a sender reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderItem {
    pub host: String,
    pub key: String,
    pub value: String,
    pub clock: Option<i64>,
    pub ns: Option<i64>,
}

/// A parsed request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    SenderData {
        items: Vec<SenderItem>,
        clock: Option<i64>,
    },
    /// A well-formed request of a kind this server does not answer (`active checks`,
    /// `agent data`, `zabbix.stats`, …), named.
    Other(String),
}

/// Why a request body was refused. The texts are fixed and go to the peer in `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    NotJson,
    NoRequest,
    BadData,
    TooManyItems,
}

impl RequestError {
    pub fn info(self) -> &'static str {
        match self {
            RequestError::NotJson => "cannot parse request as a JSON object",
            RequestError::NoRequest => "cannot find the \"request\" tag",
            RequestError::BadData => "cannot parse the \"data\" array",
            RequestError::TooManyItems => "too many values in one request",
        }
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse a request body. serde_json bounds nesting (128 levels) on its own, so a depth bomb is
/// a parse error rather than a stack overflow.
pub fn parse_request(data: &[u8]) -> Result<Request, RequestError> {
    let json: Value = serde_json::from_slice(data).map_err(|_| RequestError::NotJson)?;
    let obj = json.as_object().ok_or(RequestError::NotJson)?;
    let request = obj
        .get("request")
        .and_then(Value::as_str)
        .ok_or(RequestError::NoRequest)?;
    if request != "sender data" {
        return Ok(Request::Other(request.to_string()));
    }
    let data = obj
        .get("data")
        .and_then(Value::as_array)
        .ok_or(RequestError::BadData)?;
    if data.len() > MAX_ITEMS {
        return Err(RequestError::TooManyItems);
    }
    let mut items = Vec::with_capacity(data.len());
    for entry in data {
        let entry = entry.as_object().ok_or(RequestError::BadData)?;
        items.push(SenderItem {
            host: text(entry.get("host")).ok_or(RequestError::BadData)?,
            key: text(entry.get("key")).ok_or(RequestError::BadData)?,
            value: text(entry.get("value")).unwrap_or_default(),
            clock: int(entry.get("clock")),
            ns: int(entry.get("ns")),
        });
    }
    Ok(Request::SenderData {
        items,
        clock: int(obj.get("clock")),
    })
}

/// `processed: P; failed: F; total: T; seconds spent: S` — the string zabbix_sender scans.
pub fn info_string(processed: u64, failed: u64, total: u64, seconds: f64) -> String {
    format!("processed: {processed}; failed: {failed}; total: {total}; seconds spent: {seconds:.6}")
}

/// A complete `success` response packet.
pub fn render_result(processed: u64, failed: u64, total: u64, seconds: f64) -> Vec<u8> {
    let body = serde_json::json!({
        "response": "success",
        "info": info_string(processed, failed, total, seconds),
    });
    encode(body.to_string().as_bytes())
}

/// A complete `failed` response packet. `info` is always one of this module's fixed texts.
pub fn render_failed(info: &'static str) -> Vec<u8> {
    let body = serde_json::json!({"response": "failed", "info": info});
    encode(body.to_string().as_bytes())
}

/// Read back what [`render_result`] wrote: `(processed, failed, total)`, or `None` for anything
/// else. The session loop uses this to check a reply against the request it answers.
pub fn read_result(packet: &[u8]) -> Option<(u64, u64, u64)> {
    let header = parse_header(packet).ok()?;
    let body = packet.get(header_len(header.flags)..)?;
    let json: Value = serde_json::from_slice(body).ok()?;
    if json.get("response")?.as_str()? != "success" {
        return None;
    }
    let info = json.get("info")?.as_str()?;
    let mut fields = info.split("; ");
    let mut next = |name: &str| -> Option<u64> {
        fields
            .next()?
            .strip_prefix(name)?
            .strip_prefix(": ")?
            .parse()
            .ok()
    };
    Some((next("processed")?, next("failed")?, next("total")?))
}
