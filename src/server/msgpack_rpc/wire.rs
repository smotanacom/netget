//! MessagePack and the MessagePack-RPC envelope (request `[0, msgid, method, params]`,
//! response `[1, msgid, error, result]`, notification `[2, method, params]`), shared by the
//! server and the client.
//!
//! The stream has no framing, so bytes are buffered (at most `MAX_MESSAGE`) and a message is
//! decoded only from a complete in-memory buffer: no length the peer announces can allocate
//! more than the bytes already received, and nesting stops at `MAX_DEPTH`.
//!
//! Values map to JSON one-to-one where JSON can say them; binary becomes `{"$bin": hex}`, an
//! extension `{"$ext": type, "hex": hex}`, and a map whose keys are not strings uses each key's
//! JSON text as its name. Those objects encode back to the same MessagePack.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Number, Value};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest single message either side accepts.
pub const MAX_MESSAGE: usize = 1024 * 1024;
/// Deepest nesting decoded.
pub const MAX_DEPTH: usize = 32;
/// Deadline for the rest of a message once its first byte has arrived.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub const REQUEST: u64 = 0;
pub const RESPONSE: u64 = 1;
pub const NOTIFICATION: u64 = 2;

/// Decoding outcome for a buffer that may hold only part of a message.
enum Need {
    More,
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

type Step<T> = std::result::Result<Result<T>, Need>;

macro_rules! need {
    ($e:expr) => {
        match $e {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Ok(Err(e)),
            Err(n) => return Err(n),
        }
    };
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Step<&[u8]> {
        if self.buf.len() - self.pos < n {
            return Err(Need::More);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(Ok(s))
    }
    fn uint(&mut self, n: usize) -> Step<u64> {
        let b = need!(self.take(n));
        Ok(Ok(b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x))))
    }
    /// A count of `n` elements needs at least `n` more bytes; refuse it before looping.
    fn count(&mut self, n: u64, per_item: usize) -> Step<usize> {
        let remaining = (self.buf.len() - self.pos) as u64;
        if n.saturating_mul(per_item as u64) > MAX_MESSAGE as u64 {
            return Ok(Err(anyhow::anyhow!(
                "MessagePack count {n} exceeds the message bound"
            )));
        }
        if n.saturating_mul(per_item as u64) > remaining {
            return Err(Need::More);
        }
        Ok(Ok(n as usize))
    }
    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn value(&mut self, depth: usize) -> Step<Value> {
        if depth > MAX_DEPTH {
            return Ok(Err(anyhow::anyhow!(
                "MessagePack nested deeper than {MAX_DEPTH}"
            )));
        }
        let tag = need!(self.take(1))[0];
        let v = match tag {
            0x00..=0x7f => json!(tag),
            0xe0..=0xff => json!(tag as i8),
            0xc0 => Value::Null,
            0xc2 => json!(false),
            0xc3 => json!(true),
            0xcc => json!(need!(self.uint(1))),
            0xcd => json!(need!(self.uint(2))),
            0xce => json!(need!(self.uint(4))),
            0xcf => json!(need!(self.uint(8))),
            0xd0 => json!(need!(self.uint(1)) as u8 as i8),
            0xd1 => json!(need!(self.uint(2)) as u16 as i16),
            0xd2 => json!(need!(self.uint(4)) as u32 as i32),
            0xd3 => json!(need!(self.uint(8)) as i64),
            0xca => {
                let f = f32::from_bits(need!(self.uint(4)) as u32);
                Number::from_f64(f64::from(f))
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
            0xcb => Number::from_f64(f64::from_bits(need!(self.uint(8))))
                .map(Value::Number)
                .unwrap_or(Value::Null),
            0xa0..=0xbf | 0xd9 | 0xda | 0xdb => {
                let n = match tag {
                    0xd9 => need!(self.uint(1)),
                    0xda => need!(self.uint(2)),
                    0xdb => need!(self.uint(4)),
                    t => u64::from(t & 0x1f),
                };
                let n = need!(self.count(n, 1));
                let b = need!(self.take(n));
                match std::str::from_utf8(b) {
                    Ok(s) => json!(s),
                    Err(_) => json!({"$bin": Self::hex(b)}),
                }
            }
            0xc4..=0xc6 => {
                let n = need!(self.uint(1 << (tag - 0xc4)));
                let n = need!(self.count(n, 1));
                json!({"$bin": Self::hex(need!(self.take(n)))})
            }
            0x90..=0x9f | 0xdc | 0xdd => {
                let n = match tag {
                    0xdc => need!(self.uint(2)),
                    0xdd => need!(self.uint(4)),
                    t => u64::from(t & 0x0f),
                };
                let n = need!(self.count(n, 1));
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(need!(self.value(depth + 1)));
                }
                Value::Array(items)
            }
            0x80..=0x8f | 0xde | 0xdf => {
                let n = match tag {
                    0xde => need!(self.uint(2)),
                    0xdf => need!(self.uint(4)),
                    t => u64::from(t & 0x0f),
                };
                let n = need!(self.count(n, 2));
                let mut map = Map::new();
                for _ in 0..n {
                    let k = need!(self.value(depth + 1));
                    let v = need!(self.value(depth + 1));
                    let key = match k {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    map.insert(key, v);
                }
                Value::Object(map)
            }
            0xd4..=0xd8 | 0xc7..=0xc9 => {
                let n = match tag {
                    0xd4..=0xd8 => 1usize << (tag - 0xd4),
                    0xc7 => need!(self.uint(1)) as usize,
                    0xc8 => need!(self.uint(2)) as usize,
                    _ => need!(self.uint(4)) as usize,
                };
                let ext = need!(self.take(1))[0] as i8;
                let n = need!(self.count(n as u64, 1));
                json!({"$ext": ext, "hex": Self::hex(need!(self.take(n)))})
            }
            0xc1 => return Ok(Err(anyhow::anyhow!("MessagePack byte 0xc1 is never used"))),
        };
        Ok(Ok(v))
    }
}

/// Decode one value from the front of `buf`: `Ok(Some((value, used)))`, `Ok(None)` when the
/// buffer holds only part of it, `Err` when it is malformed.
pub fn decode(buf: &[u8]) -> Result<Option<(Value, usize)>> {
    let mut c = Cursor { buf, pos: 0 };
    match c.value(0) {
        Ok(Ok(v)) => Ok(Some((v, c.pos))),
        Ok(Err(e)) => Err(e),
        Err(Need::More) => Ok(None),
    }
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    ensure!(
        s.is_ascii() && s.len().is_multiple_of(2),
        "hex must be an even number of hex digits"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("invalid hex"))
        .collect()
}

fn put_len(out: &mut Vec<u8>, n: usize, fix: Option<(u8, usize)>, tags: [u8; 3]) {
    match fix {
        Some((base, max)) if n <= max => out.push(base | n as u8),
        _ if tags[0] != 0 && n <= 0xff => {
            out.push(tags[0]);
            out.push(n as u8);
        }
        _ if n <= 0xffff => {
            out.push(tags[1]);
            out.extend((n as u16).to_be_bytes());
        }
        _ => {
            out.push(tags[2]);
            out.extend((n as u32).to_be_bytes());
        }
    }
}

/// Encode a JSON value as MessagePack (the inverse of `decode`'s mapping).
pub fn encode(v: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "value nested deeper than {MAX_DEPTH}");
    match v {
        Value::Null => out.push(0xc0),
        Value::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if u <= 0x7f {
                    out.push(u as u8);
                } else {
                    out.push(0xcf);
                    out.extend(u.to_be_bytes());
                }
            } else if let Some(i) = n.as_i64() {
                if i >= -32 {
                    out.push(i as i8 as u8);
                } else {
                    out.push(0xd3);
                    out.extend(i.to_be_bytes());
                }
            } else {
                out.push(0xcb);
                out.extend(n.as_f64().unwrap_or(0.0).to_be_bytes());
            }
        }
        Value::String(s) => {
            put_len(out, s.len(), Some((0xa0, 31)), [0xd9, 0xda, 0xdb]);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(items) => {
            put_len(out, items.len(), Some((0x90, 15)), [0, 0xdc, 0xdd]);
            for i in items {
                encode(i, out, depth + 1)?;
            }
        }
        Value::Object(map) => {
            if let (1, Some(Value::String(h))) = (map.len(), map.get("$bin")) {
                let b = unhex(h)?;
                put_len(out, b.len(), None, [0xc4, 0xc5, 0xc6]);
                out.extend(b);
                return Ok(());
            }
            if let (2, Some(t), Some(Value::String(h))) = (
                map.len(),
                map.get("$ext").and_then(Value::as_i64),
                map.get("hex"),
            ) {
                let b = unhex(h)?;
                ensure!((-128..=127).contains(&t), "extension type out of range");
                match b.len() {
                    1 => out.push(0xd4),
                    2 => out.push(0xd5),
                    4 => out.push(0xd6),
                    8 => out.push(0xd7),
                    16 => out.push(0xd8),
                    n => put_len(out, n, None, [0xc7, 0xc8, 0xc9]),
                }
                out.push(t as i8 as u8);
                out.extend(b);
                return Ok(());
            }
            put_len(out, map.len(), Some((0x80, 15)), [0, 0xde, 0xdf]);
            for (k, v) in map {
                encode(&Value::String(k.clone()), out, depth + 1)?;
                encode(v, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

pub fn encode_message(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode(v, &mut out, 0)?;
    ensure!(
        out.len() <= MAX_MESSAGE,
        "message exceeds {MAX_MESSAGE} bytes"
    );
    Ok(out)
}

/// A stream of messages: buffered bytes and the reader they come from.
pub struct Stream<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Stream<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
        }
    }

    /// The next whole message. `None` on a clean EOF between messages; the first byte may
    /// wait `idle`, the rest must follow within `IO_TIMEOUT`.
    pub async fn next(&mut self, idle: Duration) -> Result<Option<Value>> {
        let mut deadline = None;
        loop {
            if let Some((v, used)) = decode(&self.buf)? {
                self.buf.drain(..used);
                return Ok(Some(v));
            }
            ensure!(
                self.buf.len() < MAX_MESSAGE,
                "message exceeds {MAX_MESSAGE} bytes"
            );
            let wait = if self.buf.is_empty() {
                idle
            } else {
                deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + IO_TIMEOUT)
                    .saturating_duration_since(tokio::time::Instant::now())
            };
            let mut chunk = vec![0u8; 64 * 1024];
            let n = match tokio::time::timeout(wait, self.reader.read(&mut chunk)).await {
                Ok(r) => r?,
                Err(_) if self.buf.is_empty() => {
                    bail!("MessagePack-RPC peer idle for {}s", idle.as_secs())
                }
                Err(_) => bail!(
                    "MessagePack-RPC message incomplete after {}s",
                    IO_TIMEOUT.as_secs()
                ),
            };
            if n == 0 {
                ensure!(self.buf.is_empty(), "connection closed inside a message");
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// An RPC message, checked against the envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum Rpc {
    Request {
        msgid: u64,
        method: String,
        params: Value,
    },
    Response {
        msgid: u64,
        error: Value,
        result: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

pub fn parse(v: Value) -> Result<Rpc> {
    let Value::Array(items) = v else {
        bail!("an RPC message is an array")
    };
    let kind = items
        .first()
        .and_then(Value::as_u64)
        .context("an RPC message starts with its type")?;
    let msgid = |v: &Value| {
        v.as_u64()
            .filter(|m| *m <= u64::from(u32::MAX))
            .context("msgid must be a 32-bit unsigned integer")
    };
    let params = |v: &Value| -> Result<Value> {
        ensure!(v.is_array(), "params must be an array");
        Ok(v.clone())
    };
    Ok(match (kind, items.len()) {
        (REQUEST, 4) => Rpc::Request {
            msgid: msgid(&items[1])?,
            method: items[2]
                .as_str()
                .context("method must be a string")?
                .to_string(),
            params: params(&items[3])?,
        },
        (RESPONSE, 4) => Rpc::Response {
            msgid: msgid(&items[1])?,
            error: items[2].clone(),
            result: items[3].clone(),
        },
        (NOTIFICATION, 3) => Rpc::Notification {
            method: items[1]
                .as_str()
                .context("method must be a string")?
                .to_string(),
            params: params(&items[2])?,
        },
        (k, n) => bail!("RPC message of type {k} with {n} elements"),
    })
}

pub fn request(msgid: u64, method: &str, params: &Value) -> Result<Vec<u8>> {
    encode_message(&json!([REQUEST, msgid, method, params]))
}

pub fn response(msgid: u64, error: &Value, result: &Value) -> Result<Vec<u8>> {
    encode_message(&json!([RESPONSE, msgid, error, result]))
}

pub fn notification(method: &str, params: &Value) -> Result<Vec<u8>> {
    encode_message(&json!([NOTIFICATION, method, params]))
}
