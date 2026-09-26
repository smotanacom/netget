//! PackStream — Bolt's value encoding — and Bolt's message chunking.
//!
//! Hand-written because the whole codec is ~300 lines and the property that matters most is one
//! no published crate states: **a peer cannot make the decoder recurse without bound or allocate
//! what the bytes do not contain.** Two guards carry that:
//!
//! 1. **Depth.** Lists, maps and structures nest, and one marker byte opens a level (`0x91` is a
//!    one-element list), so ~1 KB on the wire buys a thousand frames of recursion. [`decode`]
//!    recurses with an explicit counter and refuses past [`MAX_PACKSTREAM_DEPTH`]. Real Bolt
//!    traffic nests three or four deep (a RUN's parameter map holding a list of maps).
//! 2. **Declared lengths.** A `0xD6` list header declares a 32-bit element count in four bytes.
//!    Every count and byte length is checked against the bytes *remaining* before anything is
//!    allocated: an element needs at least one byte, a map entry at least two, so a count larger
//!    than that is refused as [`DecodeError::DeclaredLengthExceedsInput`] and
//!    `Vec::with_capacity` is only ever asked for what could actually be present.
//!
//! Chunking ([`Dechunker`]) carries the third bound: a message is the concatenation of chunks
//! (each a 2-byte big-endian size and that many bytes) ended by a zero-size chunk, and the total
//! is capped at [`MAX_MESSAGE_BYTES`] as chunks arrive, so a peer that never sends the
//! terminating zero cannot grow the buffer past it.
//!
//! Encoding always picks the smallest form: integers in `-16..=127` are one byte, strings under
//! 16 bytes use the tiny marker, and so on. What NetGet encodes is either something it decoded
//! (so already depth-bounded) or a JSON value the model wrote, which `bolt::values` bounds to the
//! same depth before building it.

/// Deepest nesting of lists, maps and structures the decoder accepts. A message's own top-level
/// structure is depth 1, its fields depth 2, and so on.
pub const MAX_PACKSTREAM_DEPTH: usize = 32;

/// Largest message, summed over its chunks, the server accepts. Neo4j imposes no fixed limit of
/// its own; 1 MiB is several thousand times what `cypher-shell` sends for an interactive query and
/// still room for a RUN carrying a sizeable parameter map.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// The largest payload one chunk can carry (its size field is a `u16`).
pub const MAX_CHUNK_BYTES: usize = u16::MAX as usize;

/// A PackStream value.
///
/// Maps keep their entries in wire order as a `Vec` — PackStream allows duplicate keys (the last
/// wins) and Bolt's own metadata maps are small, so a hash map would buy nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    String(String),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
    Struct { tag: u8, fields: Vec<Value> },
}

impl Value {
    /// Look a key up in a map value; the last occurrence wins, as the specification says.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Convenience: a string-keyed map from literal pairs.
    pub fn map<K: Into<String>>(entries: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn string(s: impl Into<String>) -> Value {
        Value::String(s.into())
    }
}

/// Why a byte sequence is not a PackStream value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended inside a value.
    UnexpectedEnd,
    /// Nesting passed [`MAX_PACKSTREAM_DEPTH`].
    TooDeep,
    /// A marker byte in a range the specification reserves.
    ReservedMarker(u8),
    /// A map key that is not a string.
    NonStringKey,
    /// A string whose bytes are not UTF-8.
    InvalidUtf8,
    /// A size or count larger than the remaining input could possibly hold.
    DeclaredLengthExceedsInput { declared: u64, remaining: usize },
    /// Bytes left over after the one value a message holds.
    TrailingBytes(usize),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::UnexpectedEnd => write!(f, "input ended inside a value"),
            DecodeError::TooDeep => {
                write!(f, "nesting deeper than {MAX_PACKSTREAM_DEPTH} levels")
            }
            DecodeError::ReservedMarker(m) => write!(f, "reserved marker byte 0x{m:02X}"),
            DecodeError::NonStringKey => write!(f, "map key is not a string"),
            DecodeError::InvalidUtf8 => write!(f, "string is not UTF-8"),
            DecodeError::DeclaredLengthExceedsInput {
                declared,
                remaining,
            } => write!(
                f,
                "declared length {declared} exceeds the {remaining} bytes remaining"
            ),
            DecodeError::TrailingBytes(n) => write!(f, "{n} bytes after the value"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode exactly one value occupying the whole of `bytes`.
pub fn decode(bytes: &[u8]) -> Result<Value, DecodeError> {
    let mut pos = 0;
    let value = decode_at(bytes, &mut pos, 1)?;
    if pos != bytes.len() {
        return Err(DecodeError::TrailingBytes(bytes.len() - pos));
    }
    Ok(value)
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], DecodeError> {
    let end = pos.checked_add(n).ok_or(DecodeError::UnexpectedEnd)?;
    if end > bytes.len() {
        return Err(DecodeError::UnexpectedEnd);
    }
    let slice = &bytes[*pos..end];
    *pos = end;
    Ok(slice)
}

fn read_uint(bytes: &[u8], pos: &mut usize, width: usize) -> Result<u64, DecodeError> {
    let raw = take(bytes, pos, width)?;
    Ok(raw.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

/// Refuse a declared count unless `count * min_bytes_each` could fit in what is left.
fn check_declared(
    bytes: &[u8],
    pos: usize,
    count: u64,
    min_bytes_each: u64,
) -> Result<usize, DecodeError> {
    let remaining = bytes.len() - pos;
    let needed = count.saturating_mul(min_bytes_each);
    if needed > remaining as u64 {
        return Err(DecodeError::DeclaredLengthExceedsInput {
            declared: count,
            remaining,
        });
    }
    // Cannot truncate: `count <= remaining`, which is a usize.
    Ok(count as usize)
}

fn decode_at(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<Value, DecodeError> {
    let marker = take(bytes, pos, 1)?[0];
    match marker {
        // TINY_INT, both halves.
        0x00..=0x7F => Ok(Value::Int(i64::from(marker))),
        0xF0..=0xFF => Ok(Value::Int(i64::from(marker as i8))),
        0x80..=0x8F => decode_string(bytes, pos, u64::from(marker & 0x0F)),
        0x90..=0x9F => decode_list(bytes, pos, u64::from(marker & 0x0F), depth),
        0xA0..=0xAF => decode_map(bytes, pos, u64::from(marker & 0x0F), depth),
        0xB0..=0xBF => decode_struct(bytes, pos, u64::from(marker & 0x0F), depth),
        0xC0 => Ok(Value::Null),
        0xC1 => {
            let raw = take(bytes, pos, 8)?;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(raw);
            Ok(Value::Float(f64::from_be_bytes(buf)))
        }
        0xC2 => Ok(Value::Bool(false)),
        0xC3 => Ok(Value::Bool(true)),
        0xC8 => Ok(Value::Int(i64::from(take(bytes, pos, 1)?[0] as i8))),
        0xC9 => {
            let raw = take(bytes, pos, 2)?;
            Ok(Value::Int(i64::from(i16::from_be_bytes([raw[0], raw[1]]))))
        }
        0xCA => {
            let raw = take(bytes, pos, 4)?;
            Ok(Value::Int(i64::from(i32::from_be_bytes([
                raw[0], raw[1], raw[2], raw[3],
            ]))))
        }
        0xCB => {
            let raw = take(bytes, pos, 8)?;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(raw);
            Ok(Value::Int(i64::from_be_bytes(buf)))
        }
        0xCC..=0xCE => {
            let width = 1usize << (marker - 0xCC);
            let len = read_uint(bytes, pos, width)?;
            let len = check_declared(bytes, *pos, len, 1)?;
            Ok(Value::Bytes(take(bytes, pos, len)?.to_vec()))
        }
        0xD0..=0xD2 => {
            let width = 1usize << (marker - 0xD0);
            let len = read_uint(bytes, pos, width)?;
            decode_string(bytes, pos, len)
        }
        0xD4..=0xD6 => {
            let width = 1usize << (marker - 0xD4);
            let count = read_uint(bytes, pos, width)?;
            decode_list(bytes, pos, count, depth)
        }
        0xD8..=0xDA => {
            let width = 1usize << (marker - 0xD8);
            let count = read_uint(bytes, pos, width)?;
            decode_map(bytes, pos, count, depth)
        }
        // STRUCT_8 / STRUCT_16 from PackStream v1: never produced by a Bolt 5 client, accepted
        // for completeness.
        0xDC => {
            let count = read_uint(bytes, pos, 1)?;
            decode_struct(bytes, pos, count, depth)
        }
        0xDD => {
            let count = read_uint(bytes, pos, 2)?;
            decode_struct(bytes, pos, count, depth)
        }
        other => Err(DecodeError::ReservedMarker(other)),
    }
}

fn decode_string(bytes: &[u8], pos: &mut usize, len: u64) -> Result<Value, DecodeError> {
    let len = check_declared(bytes, *pos, len, 1)?;
    let raw = take(bytes, pos, len)?;
    std::str::from_utf8(raw)
        .map(|s| Value::String(s.to_string()))
        .map_err(|_| DecodeError::InvalidUtf8)
}

fn decode_list(
    bytes: &[u8],
    pos: &mut usize,
    count: u64,
    depth: usize,
) -> Result<Value, DecodeError> {
    if depth > MAX_PACKSTREAM_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let count = check_declared(bytes, *pos, count, 1)?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(decode_at(bytes, pos, depth + 1)?);
    }
    Ok(Value::List(items))
}

fn decode_map(
    bytes: &[u8],
    pos: &mut usize,
    count: u64,
    depth: usize,
) -> Result<Value, DecodeError> {
    if depth > MAX_PACKSTREAM_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let count = check_declared(bytes, *pos, count, 2)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key = match decode_at(bytes, pos, depth + 1)? {
            Value::String(s) => s,
            _ => return Err(DecodeError::NonStringKey),
        };
        let value = decode_at(bytes, pos, depth + 1)?;
        entries.push((key, value));
    }
    Ok(Value::Map(entries))
}

fn decode_struct(
    bytes: &[u8],
    pos: &mut usize,
    count: u64,
    depth: usize,
) -> Result<Value, DecodeError> {
    if depth > MAX_PACKSTREAM_DEPTH {
        return Err(DecodeError::TooDeep);
    }
    let tag = take(bytes, pos, 1)?[0];
    let count = check_declared(bytes, *pos, count, 1)?;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        fields.push(decode_at(bytes, pos, depth + 1)?);
    }
    Ok(Value::Struct { tag, fields })
}

/// Encode one value, choosing the smallest representation.
pub fn encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0xC0),
        Value::Bool(false) => out.push(0xC2),
        Value::Bool(true) => out.push(0xC3),
        Value::Int(i) => encode_int(*i, out),
        Value::Float(f) => {
            out.push(0xC1);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Bytes(b) => {
            encode_size(b.len(), [0xCC, 0xCD, 0xCE], None, out);
            out.extend_from_slice(b);
        }
        Value::String(s) => {
            encode_size(s.len(), [0xD0, 0xD1, 0xD2], Some(0x80), out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::List(items) => {
            encode_size(items.len(), [0xD4, 0xD5, 0xD6], Some(0x90), out);
            for item in items {
                encode(item, out);
            }
        }
        Value::Map(entries) => {
            encode_size(entries.len(), [0xD8, 0xD9, 0xDA], Some(0xA0), out);
            for (key, value) in entries {
                encode(&Value::String(key.clone()), out);
                encode(value, out);
            }
        }
        Value::Struct { tag, fields } => {
            let n = fields.len();
            if n < 16 {
                out.push(0xB0 | n as u8);
            } else if n <= u8::MAX as usize {
                out.push(0xDC);
                out.push(n as u8);
            } else {
                out.push(0xDD);
                out.extend_from_slice(&(n.min(u16::MAX as usize) as u16).to_be_bytes());
            }
            out.push(*tag);
            for field in fields.iter().take(u16::MAX as usize) {
                encode(field, out);
            }
        }
    }
}

/// Encode to a fresh buffer.
pub fn to_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode(value, &mut out);
    out
}

fn encode_int(i: i64, out: &mut Vec<u8>) {
    if (-16..=127).contains(&i) {
        out.push(i as i8 as u8);
    } else if i8::try_from(i).is_ok() {
        out.push(0xC8);
        out.push(i as i8 as u8);
    } else if let Ok(v) = i16::try_from(i) {
        out.push(0xC9);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = i32::try_from(i) {
        out.push(0xCA);
        out.extend_from_slice(&v.to_be_bytes());
    } else {
        out.push(0xCB);
        out.extend_from_slice(&i.to_be_bytes());
    }
}

/// Write a size header: the tiny marker when there is one and `len < 16`, else the 8/16/32-bit
/// form. Lengths past `u32::MAX` cannot occur for anything NetGet builds (messages are capped at
/// [`MAX_MESSAGE_BYTES`] on the way in and model answers are far smaller on the way out).
fn encode_size(len: usize, wide: [u8; 3], tiny: Option<u8>, out: &mut Vec<u8>) {
    match tiny {
        Some(base) if len < 16 => out.push(base | len as u8),
        _ if len <= u8::MAX as usize => {
            out.push(wide[0]);
            out.push(len as u8);
        }
        _ if len <= u16::MAX as usize => {
            out.push(wide[1]);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            out.push(wide[2]);
            out.extend_from_slice(&(len.min(u32::MAX as usize) as u32).to_be_bytes());
        }
    }
}

/// Frame one encoded message as chunks of at most [`MAX_CHUNK_BYTES`], ended by `00 00`.
pub fn chunk(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 4 + 2 * (message.len() / MAX_CHUNK_BYTES));
    for piece in message.chunks(MAX_CHUNK_BYTES) {
        out.extend_from_slice(&(piece.len() as u16).to_be_bytes());
        out.extend_from_slice(piece);
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// Encode a value and frame it as one chunked message.
pub fn message_bytes(value: &Value) -> Vec<u8> {
    chunk(&to_bytes(value))
}

/// Why the byte stream is not a sequence of acceptable messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The message's chunks summed past the cap.
    TooLarge { limit: usize },
}

/// Reassembles chunked messages from a byte stream, enforcing [`MAX_MESSAGE_BYTES`] as chunks
/// arrive.
///
/// Bytes are appended with [`push`](Self::push); [`next_message`](Self::next_message) returns a
/// message once its terminating zero chunk has been seen. Zero chunks between messages are
/// Bolt's NOOP keep-alive and are skipped. A chunk is moved out of the input buffer as soon as it
/// is complete, so the input holds at most one partial chunk plus whatever the last read added.
#[derive(Debug)]
pub struct Dechunker {
    input: Vec<u8>,
    message: Vec<u8>,
    limit: usize,
}

impl Dechunker {
    pub fn new(limit: usize) -> Self {
        Self {
            input: Vec::new(),
            message: Vec::new(),
            limit,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }

    /// Bytes buffered that have not yet become part of a complete message.
    pub fn pending(&self) -> usize {
        self.input.len() + self.message.len()
    }

    pub fn next_message(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut consumed = 0usize;
        let result = loop {
            let rest = &self.input[consumed..];
            if rest.len() < 2 {
                break Ok(None);
            }
            let size = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
            if size == 0 {
                consumed += 2;
                if self.message.is_empty() {
                    continue; // NOOP between messages
                }
                break Ok(Some(std::mem::take(&mut self.message)));
            }
            // Refuse on the declared size, before waiting for the bytes it announces.
            if self.message.len() + size > self.limit {
                break Err(FrameError::TooLarge { limit: self.limit });
            }
            if rest.len() < 2 + size {
                break Ok(None);
            }
            self.message.extend_from_slice(&rest[2..2 + size]);
            consumed += 2 + size;
        };
        self.input.drain(..consumed);
        result
    }
}
