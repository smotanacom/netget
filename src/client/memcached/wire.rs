//! The memcached **text** protocol, client side: encode the model's requests, and read the
//! server's replies back one whole response at a time.
//!
//! Pure functions over bytes. The connection loop owns the socket and the FIFO of requests in
//! flight; this module owns the grammar and every bound on what a server may make the client
//! buffer.
//!
//! # Why the reader needs the request
//!
//! A memcached reply does not say which command it answers. `STORED`, `NOT_FOUND`, a bare
//! number and `END` are only meaningful against the request they follow, and the protocol
//! answers strictly in order. So [`parse_reply`] takes the [`Expect`] for the oldest request
//! in flight and returns one complete [`Reply`] for it, or `None` when more bytes are needed.
//!
//! # Bounds
//!
//! * **A line** — every unit except a value's data block is one CRLF-terminated line of at
//!   most [`MAX_REPLY_LINE`] octets. A server that sends more without a CRLF has lost framing
//!   and the connection is failed.
//! * **A value** — the `<bytes>` a `VALUE` header **declares** is checked against
//!   [`MAX_VALUE_LEN`] (memcached's own default item limit) before one octet of the block is
//!   waited for, so `VALUE k 0 4000000000` costs its header line and nothing more.
//! * **A `get` response** — at most one `VALUE` per requested key, and only for a key that was
//!   requested. Anything else is a server answering a question nobody asked.
//! * **`stats`** — at most [`MAX_STATS_ENTRIES`] `STAT` lines per response.
//!
//! There is no nesting anywhere in this grammar, so there is no depth to bound.

use crate::server::memcached::protocol::{MAX_KEY_LEN, MAX_VALUE_LEN};
use serde_json::{json, Map, Value};

/// Longest reply line the client will buffer before declaring the stream unframed.
///
/// A `VALUE` header is the longest legitimate line: a 250-octet key, a 32-bit flags field, a
/// 20-digit byte count and a 20-digit CAS unique. `SERVER_ERROR` messages are short. 2 KiB is
/// four times the longest real line.
pub const MAX_REPLY_LINE: usize = 2048;

/// Most `STAT` lines accepted in one `stats` response.
///
/// memcached 1.6's `stats` prints ~90 lines, `stats settings` ~70, and `stats items` /
/// `stats slabs` ~20 per slab class across at most 64 classes. 4096 covers every group with
/// room to spare and stops a server that streams `STAT` lines forever.
pub const MAX_STATS_ENTRIES: usize = 4096;

/// Most octets one response may occupy, headers and data blocks together.
///
/// Per-value and per-line bounds do not bound a *response*: 32 keys of 1 MiB each, or 4096
/// `STAT` lines of 2 KiB, are each legal one piece at a time. The transport task buffers one
/// response until it is complete, so this is also the ceiling on that buffer.
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Most keys one `get`/`gets` may ask for. Every key becomes its own model event, so this is
/// also the most model turns one request can cause.
pub const MAX_GET_KEYS: usize = 32;

/// A storage verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreVerb {
    Set,
    Add,
    Replace,
    Append,
    Prepend,
    Cas,
}

impl StoreVerb {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreVerb::Set => "set",
            StoreVerb::Add => "add",
            StoreVerb::Replace => "replace",
            StoreVerb::Append => "append",
            StoreVerb::Prepend => "prepend",
            StoreVerb::Cas => "cas",
        }
    }
}

/// One request, validated. Built from the model's action by `actions.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Get {
        keys: Vec<String>,
        with_cas: bool,
    },
    Store {
        verb: StoreVerb,
        key: String,
        flags: u32,
        exptime: i64,
        value: String,
        /// Required for `cas`, absent otherwise.
        cas_unique: Option<u64>,
    },
    Delete {
        key: String,
    },
    Incr {
        key: String,
        delta: u64,
    },
    Decr {
        key: String,
        delta: u64,
    },
    Touch {
        key: String,
        exptime: i64,
    },
    Stats {
        group: Option<String>,
    },
    Version,
    FlushAll {
        delay: Option<u64>,
    },
}

impl Request {
    /// The command word, for events and logs.
    pub fn command(&self) -> &'static str {
        match self {
            Request::Get {
                with_cas: false, ..
            } => "get",
            Request::Get { with_cas: true, .. } => "gets",
            Request::Store { verb, .. } => verb.as_str(),
            Request::Delete { .. } => "delete",
            Request::Incr { .. } => "incr",
            Request::Decr { .. } => "decr",
            Request::Touch { .. } => "touch",
            Request::Stats { .. } => "stats",
            Request::Version => "version",
            Request::FlushAll { .. } => "flush_all",
        }
    }

    /// The bytes on the wire. Never `noreply`: every request must be answered, or the FIFO
    /// that pairs replies with requests would be off by one from then on.
    pub fn encode(&self) -> Vec<u8> {
        let line = match self {
            Request::Get { keys, .. } => format!("{} {}", self.command(), keys.join(" ")),
            Request::Store {
                verb,
                key,
                flags,
                exptime,
                value,
                cas_unique,
            } => {
                let mut head = format!(
                    "{} {} {} {} {}",
                    verb.as_str(),
                    key,
                    flags,
                    exptime,
                    value.len()
                );
                if let Some(cas) = cas_unique {
                    head.push_str(&format!(" {cas}"));
                }
                let mut out = head.into_bytes();
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(value.as_bytes());
                out.extend_from_slice(b"\r\n");
                return out;
            }
            Request::Delete { key } => format!("delete {key}"),
            Request::Incr { key, delta } => format!("incr {key} {delta}"),
            Request::Decr { key, delta } => format!("decr {key} {delta}"),
            Request::Touch { key, exptime } => format!("touch {key} {exptime}"),
            Request::Stats { group: None } => "stats".to_string(),
            Request::Stats { group: Some(g) } => format!("stats {g}"),
            Request::Version => "version".to_string(),
            Request::FlushAll { delay: None } => "flush_all".to_string(),
            Request::FlushAll { delay: Some(d) } => format!("flush_all {d}"),
        };
        let mut out = line.into_bytes();
        out.extend_from_slice(b"\r\n");
        out
    }

    /// What the reply to this request looks like.
    pub fn expect(&self) -> Expect {
        let command = self.command();
        match self {
            Request::Get { keys, with_cas } => Expect::Values {
                keys: keys.clone(),
                with_cas: *with_cas,
            },
            Request::Store { key, .. }
            | Request::Delete { key }
            | Request::Incr { key, .. }
            | Request::Decr { key, .. }
            | Request::Touch { key, .. } => Expect::Status {
                command,
                key: Some(key.clone()),
            },
            Request::Stats { group } => Expect::Stats {
                group: group.clone(),
            },
            Request::Version | Request::FlushAll { .. } => Expect::Status { command, key: None },
        }
    }
}

/// Refuse a key memcached would refuse, or that would break the command line it sits in.
///
/// A key is one space-delimited token on a CRLF-terminated line, so a space or a control
/// octet in it would turn the model's key into a different command. Refused, never rewritten.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("a memcached key cannot be empty".to_string());
    }
    if key.len() > MAX_KEY_LEN {
        return Err(format!(
            "key is {} bytes; memcached refuses keys longer than {MAX_KEY_LEN}",
            key.len()
        ));
    }
    if key.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(format!(
            "key {key:?} contains a space or a control character; a memcached key is one \
             token on the command line"
        ));
    }
    Ok(())
}

/// A `stats` group is one lowercase word (`settings`, `items`, `slabs`, `sizes`, `conns`).
pub fn validate_stats_group(group: &str) -> Result<(), String> {
    if group.is_empty() || group.len() > 32 || !group.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(format!(
            "stats group {group:?} is not one lowercase word such as settings, items or slabs"
        ));
    }
    Ok(())
}

/// What the reply to the oldest request in flight looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// `VALUE … END` for these keys.
    Values { keys: Vec<String>, with_cas: bool },
    /// One status line (`STORED`, `DELETED`, a number, `VERSION …`, `OK`).
    Status {
        command: &'static str,
        key: Option<String>,
    },
    /// `STAT … END`.
    Stats { group: Option<String> },
}

/// One `VALUE` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub flags: u32,
    pub cas: Option<u64>,
    pub data: Vec<u8>,
}

/// One complete response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// A `get`/`gets` response: the hits in the order the server sent them, and the keys it
    /// did not return.
    Values {
        items: Vec<Item>,
        misses: Vec<String>,
    },
    /// A status line, uninterpreted: `STORED`, `NOT_STORED`, `EXISTS`, `NOT_FOUND`,
    /// `DELETED`, `TOUCHED`, `OK`, `VERSION x`, or a counter's decimal value.
    Status { line: String },
    /// `STAT` lines, in order.
    Stats { entries: Vec<(String, String)> },
    /// `ERROR`, `CLIENT_ERROR <msg>` or `SERVER_ERROR <msg>`.
    Error { kind: &'static str, message: String },
}

/// Why the byte stream can no longer be read as memcached replies. Always fatal for the
/// connection: after any of these the next reply's start is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    LineTooLong,
    ValueTooLarge { key: String, declared: u64 },
    UnexpectedLine(String),
    UnrequestedKey(String),
    TooManyStats,
    BadValueTerminator,
    ResponseTooLarge,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::LineTooLong => write!(
                f,
                "server sent more than {MAX_REPLY_LINE} octets without a CRLF"
            ),
            WireError::ValueTooLarge { key, declared } => write!(
                f,
                "VALUE for {key:?} declares {declared} octets, over the {MAX_VALUE_LEN}-octet \
                 item limit this client reads"
            ),
            WireError::UnexpectedLine(line) => write!(
                f,
                "reply line {:?} is not a valid answer to the request in flight",
                crate::utils::truncate::truncate_for_log(line, 80)
            ),
            WireError::UnrequestedKey(key) => {
                write!(
                    f,
                    "server returned a VALUE for {key:?}, which was not requested"
                )
            }
            WireError::TooManyStats => {
                write!(f, "stats response exceeded {MAX_STATS_ENTRIES} STAT lines")
            }
            WireError::BadValueTerminator => {
                write!(f, "a VALUE data block was not followed by CRLF")
            }
            WireError::ResponseTooLarge => write!(
                f,
                "one response exceeded {MAX_RESPONSE_BYTES} octets before it was complete"
            ),
        }
    }
}

impl std::error::Error for WireError {}

/// The next CRLF-terminated line starting at `at`, as `(line, next_offset)`.
fn line_at(buf: &[u8], at: usize) -> Result<Option<(&str, usize)>, WireError> {
    let rest = &buf[at..];
    let window = &rest[..rest.len().min(MAX_REPLY_LINE + 2)];
    match window.windows(2).position(|w| w == b"\r\n") {
        Some(end) => {
            let line = std::str::from_utf8(&rest[..end]).map_err(|_| {
                WireError::UnexpectedLine(String::from_utf8_lossy(&rest[..end]).into())
            })?;
            Ok(Some((line, at + end + 2)))
        }
        None if rest.len() > MAX_REPLY_LINE => Err(WireError::LineTooLong),
        None => Ok(None),
    }
}

/// An error line, which may stand in for any reply.
fn error_line(line: &str) -> Option<Reply> {
    if line == "ERROR" {
        return Some(Reply::Error {
            kind: "error",
            message: "the server did not recognise the command".to_string(),
        });
    }
    if let Some(msg) = line.strip_prefix("CLIENT_ERROR") {
        return Some(Reply::Error {
            kind: "client_error",
            message: msg.trim().to_string(),
        });
    }
    if let Some(msg) = line.strip_prefix("SERVER_ERROR") {
        return Some(Reply::Error {
            kind: "server_error",
            message: msg.trim().to_string(),
        });
    }
    None
}

/// Read one complete reply for `expect` off the front of `buf`.
///
/// `Ok(None)` means more bytes are needed and nothing was consumed; `Ok(Some((reply, n)))`
/// consumed `n` octets.
pub fn parse_reply(buf: &[u8], expect: &Expect) -> Result<Option<(Reply, usize)>, WireError> {
    let Some((first, mut pos)) = line_at(buf, 0)? else {
        return Ok(None);
    };
    if let Some(err) = error_line(first) {
        return Ok(Some((err, pos)));
    }

    match expect {
        Expect::Status { .. } => {
            if first.starts_with("VALUE ") || first.starts_with("STAT ") || first == "END" {
                return Err(WireError::UnexpectedLine(first.to_string()));
            }
            Ok(Some((
                Reply::Status {
                    line: first.to_string(),
                },
                pos,
            )))
        }
        Expect::Stats { .. } => {
            let mut entries = Vec::new();
            let mut line = first;
            loop {
                if line == "END" {
                    return Ok(Some((Reply::Stats { entries }, pos)));
                }
                let Some(rest) = line.strip_prefix("STAT ") else {
                    return Err(WireError::UnexpectedLine(line.to_string()));
                };
                if entries.len() >= MAX_STATS_ENTRIES {
                    return Err(WireError::TooManyStats);
                }
                if pos > MAX_RESPONSE_BYTES {
                    return Err(WireError::ResponseTooLarge);
                }
                let (name, value) = rest.split_once(' ').unwrap_or((rest, ""));
                entries.push((name.to_string(), value.to_string()));
                match line_at(buf, pos)? {
                    Some((next, next_pos)) => {
                        line = next;
                        pos = next_pos;
                    }
                    None => return Ok(None),
                }
            }
        }
        Expect::Values { keys, with_cas } => {
            let mut items: Vec<Item> = Vec::new();
            let mut line = first;
            loop {
                if line == "END" {
                    let misses = keys
                        .iter()
                        .filter(|k| !items.iter().any(|i| &i.key == *k))
                        .cloned()
                        .collect();
                    return Ok(Some((Reply::Values { items, misses }, pos)));
                }
                let Some(rest) = line.strip_prefix("VALUE ") else {
                    return Err(WireError::UnexpectedLine(line.to_string()));
                };
                let fields: Vec<&str> = rest.split(' ').collect();
                let (key, flags, bytes, cas) = match (fields.as_slice(), with_cas) {
                    ([k, f, b], false) => (*k, *f, *b, None),
                    ([k, f, b, c], true) => (*k, *f, *b, Some(*c)),
                    _ => return Err(WireError::UnexpectedLine(line.to_string())),
                };
                if !keys.iter().any(|k| k == key) || items.iter().any(|i| i.key == key) {
                    return Err(WireError::UnrequestedKey(key.to_string()));
                }
                let bad = || WireError::UnexpectedLine(line.to_string());
                let flags: u32 = flags.parse().map_err(|_| bad())?;
                let declared: u64 = bytes.parse().map_err(|_| bad())?;
                let cas = match cas {
                    Some(c) => Some(c.parse::<u64>().map_err(|_| bad())?),
                    None => None,
                };
                // The declared size, before anything is waited for or allocated.
                let len = usize::try_from(declared)
                    .ok()
                    .filter(|n| *n <= MAX_VALUE_LEN)
                    .ok_or_else(|| WireError::ValueTooLarge {
                        key: key.to_string(),
                        declared,
                    })?;
                let key = key.to_string();
                if pos + len + 2 > MAX_RESPONSE_BYTES {
                    return Err(WireError::ResponseTooLarge);
                }
                if buf.len() < pos + len + 2 {
                    return Ok(None);
                }
                if &buf[pos + len..pos + len + 2] != b"\r\n" {
                    return Err(WireError::BadValueTerminator);
                }
                items.push(Item {
                    key,
                    flags,
                    cas,
                    data: buf[pos..pos + len].to_vec(),
                });
                pos += len + 2;
                match line_at(buf, pos)? {
                    Some((next, next_pos)) => {
                        line = next;
                        pos = next_pos;
                    }
                    None => return Ok(None),
                }
            }
        }
    }
}

/// Render a `stats` response as one JSON object. A name that repeats keeps its last value.
pub fn stats_json(entries: &[(String, String)]) -> Value {
    let mut map = Map::new();
    for (k, v) in entries {
        map.insert(k.clone(), json!(v));
    }
    Value::Object(map)
}
