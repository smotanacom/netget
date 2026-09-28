//! nsqd's TCP protocol (V2): the client's commands and the server's frames.
//!
//! Everything here is a pure function of its arguments, shared by the session loop, the action
//! executor, the tests and the `nsq_frame` fuzz target. **The model never writes a frame**: it
//! supplies a verdict, an error code and text, or message bodies, and this module decides the
//! sizes, the frame types, the message ids and the timestamps' layout.
//!
//! Client → server, after the four-byte magic `"  V2"`:
//!
//! ```text
//! <COMMAND> <params separated by spaces>\n
//! [ 4-byte big-endian size ][ body ]          IDENTIFY, PUB, MPUB, DPUB and AUTH only
//! ```
//!
//! Server → client, every frame:
//!
//! ```text
//! [ 4-byte size = 4 + len(data) ][ 4-byte frame type ][ data ]
//! frame type 0 response ("OK", "CLOSE_WAIT", "_heartbeat_", IDENTIFY's JSON)
//!            1 error    ("E_CODE description")
//!            2 message  (timestamp i64 ns | attempts u16 | 16-byte ASCII id | body)
//! ```
//!
//! Error codes and texts follow nsqd 1.3's `protocol_v2.go` so a client that matches on them
//! sees what it would see from nsqd. Every size a peer declares is judged against its bound
//! from the size field alone, before anything is allocated for the body.

use serde_json::{json, Value};

/// The protocol magic a V2 client sends first.
pub const MAGIC_V2: &[u8; 4] = b"  V2";

pub const FRAME_RESPONSE: u32 = 0;
pub const FRAME_ERROR: u32 = 1;
pub const FRAME_MESSAGE: u32 = 2;

/// The longest command line, newline included. nsqd reads commands through a 16 KiB buffer;
/// no command is longer than `REQ <16-byte id> <timeout>` or `SUB <64> <64>`, so 1 KiB is ample
/// and a line past it is not a command.
pub const MAX_LINE: usize = 1024;

/// nsqd's `--max-msg-size` default: the largest single message body.
pub const MAX_MSG_SIZE: usize = 1024 * 1024;

/// nsqd's `--max-body-size` default: the largest IDENTIFY, MPUB or AUTH body.
pub const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

/// The most messages one MPUB may declare: nsqd's own bound, `(max_body_size - 4) / 5` (a
/// four-byte count, then at least a four-byte size and one byte per message).
pub const MAX_MPUB_MESSAGES: usize = (MAX_BODY_SIZE - 4) / 5;

/// nsqd's `--max-rdy-count` default.
pub const MAX_RDY_COUNT: u64 = 2500;

/// nsqd's `--max-req-timeout` default, in milliseconds.
pub const MAX_REQ_TIMEOUT_MS: i64 = 60 * 60 * 1000;

/// nsqd's `--msg-timeout` and `--max-msg-timeout` defaults, in milliseconds.
pub const MSG_TIMEOUT_MS: i64 = 60_000;
pub const MAX_MSG_TIMEOUT_MS: i64 = 15 * 60 * 1000;

/// nsqd's `--max-heartbeat-interval` default, in milliseconds, and the least a client may ask.
pub const MAX_HEARTBEAT_MS: i64 = 60_000;
pub const MIN_HEARTBEAT_MS: i64 = 1000;

/// Message ids are 16 bytes on the wire.
pub const MSG_ID_LEN: usize = 16;

/// The longest topic or channel name, `#ephemeral` included.
pub const MAX_NAME_LEN: usize = 64;

/// The response frame nsqd sends as a heartbeat.
pub const HEARTBEAT: &[u8] = b"_heartbeat_";

/// Every error code nsqd sends. A code outside this list is refused by the executor.
pub const ERROR_CODES: &[&str] = &[
    "E_INVALID",
    "E_BAD_BODY",
    "E_BAD_TOPIC",
    "E_BAD_CHANNEL",
    "E_BAD_MESSAGE",
    "E_PUB_FAILED",
    "E_MPUB_FAILED",
    "E_DPUB_FAILED",
    "E_FIN_FAILED",
    "E_REQ_FAILED",
    "E_TOUCH_FAILED",
    "E_AUTH_FAILED",
    "E_UNAUTHORIZED",
    "E_AUTH_DISABLED",
    "E_TOO_MANY_CHANNEL_CONSUMERS",
];

/// Whether nsqd closes the connection after sending this error. Only the three per-message
/// failures are recoverable (`ClientErr` in nsqd); every other code is a `FatalClientErr`.
pub fn is_fatal(code: &str) -> bool {
    !matches!(code, "E_FIN_FAILED" | "E_REQ_FAILED" | "E_TOUCH_FAILED")
}

/// A command the client sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Identify(Vec<u8>),
    Sub {
        topic: String,
        channel: String,
    },
    Rdy(u64),
    Fin(String),
    Req {
        id: String,
        timeout_ms: i64,
    },
    Touch(String),
    Pub {
        topic: String,
        body: Vec<u8>,
    },
    Mpub {
        topic: String,
        messages: Vec<Vec<u8>>,
    },
    Dpub {
        topic: String,
        defer_ms: i64,
        body: Vec<u8>,
    },
    Nop,
    Cls,
    Auth(Vec<u8>),
}

impl Command {
    /// The command's name as it appears on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Identify(_) => "IDENTIFY",
            Command::Sub { .. } => "SUB",
            Command::Rdy(_) => "RDY",
            Command::Fin(_) => "FIN",
            Command::Req { .. } => "REQ",
            Command::Touch(_) => "TOUCH",
            Command::Pub { .. } => "PUB",
            Command::Mpub { .. } => "MPUB",
            Command::Dpub { .. } => "DPUB",
            Command::Nop => "NOP",
            Command::Cls => "CLS",
            Command::Auth(_) => "AUTH",
        }
    }
}

/// A refusal in nsqd's own vocabulary: the error frame's code and description, and whether the
/// connection closes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    pub code: &'static str,
    pub message: String,
    pub fatal: bool,
}

impl WireError {
    fn fatal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fatal: true,
        }
    }

    /// Which bound or rule refused the command, for the `decision=` tag.
    pub fn decision(&self) -> &'static str {
        if self.message.contains("too big") || self.message.contains("too long") {
            "fail_closed_too_large"
        } else if self.message.contains("message count") {
            "fail_closed_mpub_count"
        } else {
            "fail_closed_bad_command"
        }
    }

    pub fn to_frame(&self) -> Vec<u8> {
        error_frame(self.code, &self.message)
    }
}

/// Whether `name` is a valid topic or channel name: `^[.a-zA-Z0-9_-]+(#ephemeral)?$`, 1 to 64
/// characters in all.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let stem = name.strip_suffix("#ephemeral").unwrap_or(name);
    !stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether a name carries the `#ephemeral` suffix.
pub fn is_ephemeral(name: &str) -> bool {
    name.ends_with("#ephemeral")
}

/// Parse the next command from `buf`.
///
/// `Ok(None)` means more bytes are needed; `Ok(Some((command, used)))` a whole command of `used`
/// bytes. A body command's declared size is judged as soon as its four bytes are present, so an
/// over-size declaration is refused without waiting for (or allocating) the body. Parsing is a
/// pure function of the buffer, so a caller may stop waiting for bytes at any point and resume.
pub fn parse_command(buf: &[u8]) -> Result<Option<(Command, usize)>, WireError> {
    let window = &buf[..buf.len().min(MAX_LINE)];
    let Some(nl) = window.iter().position(|b| *b == b'\n') else {
        if buf.len() >= MAX_LINE {
            return Err(WireError::fatal(
                "E_INVALID",
                format!("command line too long (over {MAX_LINE} bytes)"),
            ));
        }
        return Ok(None);
    };
    let mut line = &buf[..nl];
    if line.last() == Some(&b'\r') {
        line = &line[..line.len() - 1];
    }
    let line = String::from_utf8_lossy(line).into_owned();
    let params: Vec<&str> = line.split(' ').collect();
    let after_line = nl + 1;

    let body = |label: &str, min: i64, max: usize| -> Result<Option<(Vec<u8>, usize)>, WireError> {
        let rest = &buf[after_line..];
        if rest.len() < 4 {
            return Ok(None);
        }
        let size = i32::from_be_bytes(rest[..4].try_into().unwrap()) as i64;
        let (code, what) = match label {
            "PUB" | "DPUB" => ("E_BAD_MESSAGE", "message"),
            _ => ("E_BAD_BODY", "body"),
        };
        if size < min {
            let kind = if what == "message" {
                "message body"
            } else {
                "body"
            };
            return Err(WireError::fatal(
                code,
                format!("{label} invalid {kind} size {size}"),
            ));
        }
        if size as usize > max {
            return Err(WireError::fatal(
                code,
                format!("{label} {what} too big {size} > {max}"),
            ));
        }
        let size = size as usize;
        if rest.len() < 4 + size {
            return Ok(None);
        }
        Ok(Some((rest[4..4 + size].to_vec(), after_line + 4 + size)))
    };

    let topic_param = |label: &str| -> Result<String, WireError> {
        let topic = params[1];
        if !valid_name(topic) {
            return Err(WireError::fatal(
                "E_BAD_TOPIC",
                format!("{label} topic name {topic:?} is not valid"),
            ));
        }
        Ok(topic.to_string())
    };

    let command = match params[0] {
        "IDENTIFY" => match body("IDENTIFY", 1, MAX_BODY_SIZE)? {
            None => return Ok(None),
            Some((body, used)) => return Ok(Some((Command::Identify(body), used))),
        },
        "AUTH" => match body("AUTH", 1, MAX_BODY_SIZE)? {
            None => return Ok(None),
            Some((body, used)) => return Ok(Some((Command::Auth(body), used))),
        },
        "PUB" => {
            if params.len() < 2 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "PUB insufficient number of parameters",
                ));
            }
            let topic = topic_param("PUB")?;
            match body("PUB", 1, MAX_MSG_SIZE)? {
                None => return Ok(None),
                Some((body, used)) => return Ok(Some((Command::Pub { topic, body }, used))),
            }
        }
        "DPUB" => {
            if params.len() < 3 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "DPUB insufficient number of parameters",
                ));
            }
            let topic = topic_param("DPUB")?;
            let defer_ms: i64 = params[2].parse().map_err(|_| {
                WireError::fatal(
                    "E_INVALID",
                    format!("DPUB could not parse timeout {}", params[2]),
                )
            })?;
            if !(0..=MAX_REQ_TIMEOUT_MS).contains(&defer_ms) {
                return Err(WireError::fatal(
                    "E_INVALID",
                    format!("DPUB timeout {defer_ms} out of range 0-{MAX_REQ_TIMEOUT_MS}"),
                ));
            }
            match body("DPUB", 1, MAX_MSG_SIZE)? {
                None => return Ok(None),
                Some((body, used)) => {
                    return Ok(Some((
                        Command::Dpub {
                            topic,
                            defer_ms,
                            body,
                        },
                        used,
                    )))
                }
            }
        }
        "MPUB" => {
            if params.len() < 2 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "MPUB insufficient number of parameters",
                ));
            }
            let topic = topic_param("MPUB")?;
            // The message count is judged from its own four bytes, before the body arrives.
            let rest = &buf[after_line..];
            if rest.len() >= 8 {
                let size = i32::from_be_bytes(rest[..4].try_into().unwrap()) as i64;
                if (1..=MAX_BODY_SIZE as i64).contains(&size) {
                    let count = i32::from_be_bytes(rest[4..8].try_into().unwrap()) as i64;
                    check_mpub_count(count)?;
                }
            }
            match body("MPUB", 1, MAX_BODY_SIZE)? {
                None => return Ok(None),
                Some((body, used)) => {
                    let messages = split_mpub(&body)?;
                    return Ok(Some((Command::Mpub { topic, messages }, used)));
                }
            }
        }
        "SUB" => {
            if params.len() < 3 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "SUB insufficient number of parameters",
                ));
            }
            let topic = topic_param("SUB")?;
            let channel = params[2];
            if !valid_name(channel) {
                return Err(WireError::fatal(
                    "E_BAD_CHANNEL",
                    format!("SUB channel name {channel:?} is not valid"),
                ));
            }
            Command::Sub {
                topic,
                channel: channel.to_string(),
            }
        }
        "RDY" => {
            let count: i64 = match params.get(1) {
                None => 1,
                Some(p) => p.parse().map_err(|_| {
                    WireError::fatal("E_INVALID", format!("RDY could not parse count {p}"))
                })?,
            };
            if count < 0 || count as u64 > MAX_RDY_COUNT {
                return Err(WireError::fatal(
                    "E_INVALID",
                    format!("RDY count {count} out of range 0-{MAX_RDY_COUNT}"),
                ));
            }
            Command::Rdy(count as u64)
        }
        "FIN" => {
            if params.len() < 2 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "FIN insufficient number of params",
                ));
            }
            Command::Fin(message_id(params[1])?)
        }
        "TOUCH" => {
            if params.len() < 2 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "TOUCH insufficient number of params",
                ));
            }
            Command::Touch(message_id(params[1])?)
        }
        "REQ" => {
            if params.len() < 3 {
                return Err(WireError::fatal(
                    "E_INVALID",
                    "REQ insufficient number of params",
                ));
            }
            let id = message_id(params[1])?;
            let timeout_ms: i64 = params[2].parse().map_err(|_| {
                WireError::fatal(
                    "E_INVALID",
                    format!("REQ could not parse timeout {}", params[2]),
                )
            })?;
            // nsqd clamps an out-of-range timeout rather than refusing it.
            Command::Req {
                id,
                timeout_ms: timeout_ms.clamp(0, MAX_REQ_TIMEOUT_MS),
            }
        }
        "NOP" => Command::Nop,
        "CLS" => Command::Cls,
        other => {
            return Err(WireError::fatal(
                "E_INVALID",
                format!(
                    "invalid command {}",
                    crate::utils::sanitize::line_field(other)
                ),
            ))
        }
    };
    Ok(Some((command, after_line)))
}

fn check_mpub_count(count: i64) -> Result<(), WireError> {
    if count <= 0 || count as usize > MAX_MPUB_MESSAGES {
        return Err(WireError::fatal(
            "E_BAD_BODY",
            format!("MPUB invalid message count {count}"),
        ));
    }
    Ok(())
}

fn message_id(param: &str) -> Result<String, WireError> {
    if param.len() != MSG_ID_LEN {
        return Err(WireError::fatal("E_INVALID", "Invalid Message ID"));
    }
    Ok(param.to_string())
}

/// Split an MPUB body: a four-byte count, then each message as a four-byte size and its bytes.
pub fn split_mpub(body: &[u8]) -> Result<Vec<Vec<u8>>, WireError> {
    if body.len() < 4 {
        return Err(WireError::fatal(
            "E_BAD_BODY",
            "MPUB failed to read message count",
        ));
    }
    let count = i32::from_be_bytes(body[..4].try_into().unwrap()) as i64;
    check_mpub_count(count)?;
    let mut at = 4usize;
    // Bounded by check_mpub_count, and every message costs at least five bytes of a body that
    // is itself bounded.
    let mut messages = Vec::with_capacity((count as usize).min(body.len() / 5));
    for _ in 0..count {
        if body.len() < at + 4 {
            return Err(WireError::fatal(
                "E_BAD_MESSAGE",
                "MPUB failed to read message body size",
            ));
        }
        let size = i32::from_be_bytes(body[at..at + 4].try_into().unwrap()) as i64;
        at += 4;
        if size <= 0 {
            return Err(WireError::fatal(
                "E_BAD_MESSAGE",
                format!("MPUB invalid message({}) body size {size}", messages.len()),
            ));
        }
        if size as usize > MAX_MSG_SIZE {
            return Err(WireError::fatal(
                "E_BAD_MESSAGE",
                format!("MPUB message too big {size} > {MAX_MSG_SIZE}"),
            ));
        }
        let size = size as usize;
        if body.len() < at + size {
            return Err(WireError::fatal(
                "E_BAD_MESSAGE",
                "MPUB failed to read message body",
            ));
        }
        messages.push(body[at..at + size].to_vec());
        at += size;
    }
    Ok(messages)
}

/// Encode a command as a client writes it. Used by the tests, the fuzz target and the proptest
/// round trip; the server never sends one.
pub fn encode_command(command: &Command) -> Vec<u8> {
    let with_body = |line: String, body: &[u8]| {
        let mut out = line.into_bytes();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    };
    match command {
        Command::Identify(body) => with_body("IDENTIFY\n".to_string(), body),
        Command::Auth(body) => with_body("AUTH\n".to_string(), body),
        Command::Pub { topic, body } => with_body(format!("PUB {topic}\n"), body),
        Command::Dpub {
            topic,
            defer_ms,
            body,
        } => with_body(format!("DPUB {topic} {defer_ms}\n"), body),
        Command::Mpub { topic, messages } => {
            let mut body = (messages.len() as u32).to_be_bytes().to_vec();
            for m in messages {
                body.extend_from_slice(&(m.len() as u32).to_be_bytes());
                body.extend_from_slice(m);
            }
            with_body(format!("MPUB {topic}\n"), &body)
        }
        Command::Sub { topic, channel } => format!("SUB {topic} {channel}\n").into_bytes(),
        Command::Rdy(n) => format!("RDY {n}\n").into_bytes(),
        Command::Fin(id) => format!("FIN {id}\n").into_bytes(),
        Command::Req { id, timeout_ms } => format!("REQ {id} {timeout_ms}\n").into_bytes(),
        Command::Touch(id) => format!("TOUCH {id}\n").into_bytes(),
        Command::Nop => b"NOP\n".to_vec(),
        Command::Cls => b"CLS\n".to_vec(),
    }
}

/// A frame: its type and data.
pub fn encode_frame(frame_type: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&((data.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(&frame_type.to_be_bytes());
    out.extend_from_slice(data);
    out
}

pub fn response_frame(data: &[u8]) -> Vec<u8> {
    encode_frame(FRAME_RESPONSE, data)
}

/// `E_CODE description`, the description reduced to one line of printable text.
pub fn error_frame(code: &str, message: &str) -> Vec<u8> {
    let message = crate::utils::sanitize::line_field(message);
    let text = if message.trim().is_empty() {
        code.to_string()
    } else {
        format!("{code} {}", message.trim())
    };
    encode_frame(FRAME_ERROR, text.as_bytes())
}

/// A message frame: timestamp (ns since the epoch), attempts, the 16-byte id, the body.
pub fn message_frame(
    timestamp_ns: i64,
    attempts: u16,
    id: &[u8; MSG_ID_LEN],
    body: &[u8],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 2 + MSG_ID_LEN + body.len());
    data.extend_from_slice(&timestamp_ns.to_be_bytes());
    data.extend_from_slice(&attempts.to_be_bytes());
    data.extend_from_slice(id);
    data.extend_from_slice(body);
    encode_frame(FRAME_MESSAGE, &data)
}

/// A message id NetGet generates: 16 lower-case hex digits, as nsqd's GUIDs are.
pub fn message_id_for(n: u64) -> [u8; MSG_ID_LEN] {
    let mut id = [0u8; MSG_ID_LEN];
    id.copy_from_slice(format!("{n:016x}").as_bytes());
    id
}

/// A frame read back: `(frame type, data)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: u32,
    pub data: Vec<u8>,
}

/// A message frame's data, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub timestamp_ns: i64,
    pub attempts: u16,
    pub id: [u8; MSG_ID_LEN],
    pub body: Vec<u8>,
}

/// The largest frame [`parse_frame`] accepts: a message frame carrying the largest body.
pub const MAX_FRAME_DATA: usize = 8 + 2 + MSG_ID_LEN + MAX_MSG_SIZE;

/// Read one frame from `buf`: `Ok(None)` for more bytes, `Err` for a size no frame this server
/// writes could have. Used by the tests and the fuzz target to read the server's side.
pub fn parse_frame(buf: &[u8]) -> Result<Option<(Frame, usize)>, &'static str> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let size = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
    if size < 4 {
        return Err("frame size below the four-byte frame type");
    }
    if size - 4 > MAX_FRAME_DATA {
        return Err("frame size past the largest message frame");
    }
    if buf.len() < 4 + size {
        return Ok(None);
    }
    let frame_type = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    Ok(Some((
        Frame {
            frame_type,
            data: buf[8..4 + size].to_vec(),
        },
        4 + size,
    )))
}

/// Split a message frame's data.
pub fn parse_message(data: &[u8]) -> Option<Message> {
    if data.len() < 8 + 2 + MSG_ID_LEN {
        return None;
    }
    let mut id = [0u8; MSG_ID_LEN];
    id.copy_from_slice(&data[10..10 + MSG_ID_LEN]);
    Some(Message {
        timestamp_ns: i64::from_be_bytes(data[..8].try_into().unwrap()),
        attempts: u16::from_be_bytes(data[8..10].try_into().unwrap()),
        id,
        body: data[10 + MSG_ID_LEN..].to_vec(),
    })
}

/// What an IDENTIFY body asked for, as far as this server honours it.
#[derive(Debug, Clone, PartialEq)]
pub struct Identify {
    pub feature_negotiation: bool,
    /// `None`: the client disabled heartbeats (`-1`). `Some(0)`: the server's default.
    pub heartbeat_ms: Option<i64>,
    pub msg_timeout_ms: i64,
    pub client_id: String,
    pub hostname: String,
    pub user_agent: String,
}

/// Read an IDENTIFY body, refusing what nsqd refuses.
pub fn parse_identify(body: &[u8]) -> Result<Identify, WireError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| WireError::fatal("E_BAD_BODY", "IDENTIFY failed to decode JSON body"))?;
    if !value.is_object() {
        return Err(WireError::fatal(
            "E_BAD_BODY",
            "IDENTIFY failed to decode JSON body",
        ));
    }
    let text = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_str)
            .map(|s| crate::utils::truncate_for_log(&crate::utils::sanitize::line_field(s), 128))
            .unwrap_or_default()
    };
    let heartbeat = value
        .get("heartbeat_interval")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let heartbeat_ms = match heartbeat {
        -1 => None,
        0 => Some(0),
        n if (MIN_HEARTBEAT_MS..=MAX_HEARTBEAT_MS).contains(&n) => Some(n),
        n => {
            return Err(WireError::fatal(
                "E_BAD_BODY",
                format!("IDENTIFY heartbeat interval ({n}) is invalid"),
            ))
        }
    };
    let msg_timeout = value
        .get("msg_timeout")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let msg_timeout_ms = match msg_timeout {
        0 => MSG_TIMEOUT_MS,
        n if (1000..=MAX_MSG_TIMEOUT_MS).contains(&n) => n,
        n => {
            return Err(WireError::fatal(
                "E_BAD_BODY",
                format!("IDENTIFY msg timeout ({n}) is invalid"),
            ))
        }
    };
    Ok(Identify {
        feature_negotiation: value
            .get("feature_negotiation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        heartbeat_ms,
        msg_timeout_ms,
        client_id: text("client_id"),
        hostname: text("hostname"),
        user_agent: text("user_agent"),
    })
}

/// The feature-negotiation reply nsqd sends to an IDENTIFY that asked for one. TLS, deflate,
/// snappy and auth are off, so a client never upgrades the stream.
pub fn identify_response(msg_timeout_ms: i64) -> Vec<u8> {
    json!({
        "max_rdy_count": MAX_RDY_COUNT,
        "version": concat!("netget-", env!("CARGO_PKG_VERSION")),
        "max_msg_timeout": MAX_MSG_TIMEOUT_MS,
        "msg_timeout": msg_timeout_ms,
        "tls_v1": false,
        "deflate": false,
        "deflate_level": 6,
        "max_deflate_level": 6,
        "snappy": false,
        "sample_rate": 0,
        "auth_required": false,
        "output_buffer_size": 16384,
        "output_buffer_timeout": 250
    })
    .to_string()
    .into_bytes()
}
