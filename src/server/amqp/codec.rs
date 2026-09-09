//! AMQP 0-9-1 wire codec: frames, primitive types, field tables and Basic properties.
//!
//! Written by hand rather than taken from a crate. `lapin` — the dependency the `amqp`
//! feature already carries — is an AMQP *client*: it can parse and produce frames, but its
//! types are wired into a client-side connection state machine, so a broker cannot borrow
//! them. `lapin` stays for `src/client/amqp` and for the E2E tests, which drive this server
//! with a real client.
//!
//! Everything here is bounds-checked **and depth-bounded**. [`Decoder`] never indexes without
//! `get`, every length read off the wire is validated against the bytes remaining, every
//! offset arithmetic uses `checked_add`, and field-table nesting is capped, so no frame —
//! however malformed — can panic a connection task or overflow the stack.
//! Encoding truncates over-long short strings at a UTF-8 char boundary rather than slicing.
//!
//! The depth half of that sentence was missing, and so was the bound. Field tables are
//! recursive, five bytes bought a level, and a stack overflow is a `SIGSEGV` against a guard
//! page rather than a panic — so it killed the whole process rather than one connection, and
//! `tokio::spawn` could not contain it. See `MAX_FIELD_TABLE_DEPTH`.

use crate::utils::truncate::truncate_str;
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};

// ============================================================================
// Constants (AMQP 0-9-1 section 4.2.3 and the class/method registry)
// ============================================================================

/// The 8-byte protocol header an AMQP 0-9-1 client opens with, and the one this
/// broker echoes when it rejects a version it does not speak.
pub const PROTOCOL_HEADER_091: [u8; 8] = *b"AMQP\x00\x00\x09\x01";

pub const FRAME_METHOD: u8 = 1;
pub const FRAME_HEADER: u8 = 2;
pub const FRAME_BODY: u8 = 3;
pub const FRAME_HEARTBEAT: u8 = 8;
pub const FRAME_END: u8 = 0xCE;

/// Bytes of framing overhead around a payload: 1 type + 2 channel + 4 size + 1 end.
pub const FRAME_OVERHEAD: usize = 8;

pub const CLASS_CONNECTION: u16 = 10;
pub const CLASS_CHANNEL: u16 = 20;
pub const CLASS_EXCHANGE: u16 = 40;
pub const CLASS_QUEUE: u16 = 50;
pub const CLASS_BASIC: u16 = 60;

pub const CONNECTION_START: u16 = 10;
pub const CONNECTION_START_OK: u16 = 11;
pub const CONNECTION_TUNE: u16 = 30;
pub const CONNECTION_TUNE_OK: u16 = 31;
pub const CONNECTION_OPEN: u16 = 40;
pub const CONNECTION_OPEN_OK: u16 = 41;
pub const CONNECTION_CLOSE: u16 = 50;
pub const CONNECTION_CLOSE_OK: u16 = 51;

pub const CHANNEL_OPEN: u16 = 10;
pub const CHANNEL_OPEN_OK: u16 = 11;
pub const CHANNEL_CLOSE: u16 = 40;
pub const CHANNEL_CLOSE_OK: u16 = 41;

pub const EXCHANGE_DECLARE: u16 = 10;
pub const EXCHANGE_DECLARE_OK: u16 = 11;

pub const QUEUE_DECLARE: u16 = 10;
pub const QUEUE_DECLARE_OK: u16 = 11;
pub const QUEUE_BIND: u16 = 20;
pub const QUEUE_BIND_OK: u16 = 21;

pub const BASIC_QOS: u16 = 10;
pub const BASIC_QOS_OK: u16 = 11;
pub const BASIC_CONSUME: u16 = 20;
pub const BASIC_CONSUME_OK: u16 = 21;
pub const BASIC_CANCEL: u16 = 30;
pub const BASIC_CANCEL_OK: u16 = 31;
pub const BASIC_PUBLISH: u16 = 40;
pub const BASIC_RETURN: u16 = 50;
pub const BASIC_DELIVER: u16 = 60;
pub const BASIC_ACK: u16 = 80;
pub const BASIC_REJECT: u16 = 90;
pub const BASIC_NACK: u16 = 120;

/// AMQP reply code for a method this broker does not implement (0-9-1 section 4.2.7).
pub const REPLY_NOT_IMPLEMENTED: u16 = 540;
/// AMQP reply code for a refused connection.
pub const REPLY_ACCESS_REFUSED: u16 = 403;
/// AMQP reply code for a resource another connection holds exclusively.
pub const REPLY_RESOURCE_LOCKED: u16 = 405;

/// Human name for a class/method pair, for logs and close reasons.
pub fn method_name(class_id: u16, method_id: u16) -> String {
    let name = match (class_id, method_id) {
        (CLASS_CONNECTION, CONNECTION_START) => "connection.start",
        (CLASS_CONNECTION, CONNECTION_START_OK) => "connection.start-ok",
        (CLASS_CONNECTION, CONNECTION_TUNE) => "connection.tune",
        (CLASS_CONNECTION, CONNECTION_TUNE_OK) => "connection.tune-ok",
        (CLASS_CONNECTION, CONNECTION_OPEN) => "connection.open",
        (CLASS_CONNECTION, CONNECTION_OPEN_OK) => "connection.open-ok",
        (CLASS_CONNECTION, CONNECTION_CLOSE) => "connection.close",
        (CLASS_CONNECTION, CONNECTION_CLOSE_OK) => "connection.close-ok",
        (CLASS_CHANNEL, CHANNEL_OPEN) => "channel.open",
        (CLASS_CHANNEL, CHANNEL_OPEN_OK) => "channel.open-ok",
        (CLASS_CHANNEL, CHANNEL_CLOSE) => "channel.close",
        (CLASS_CHANNEL, CHANNEL_CLOSE_OK) => "channel.close-ok",
        (CLASS_EXCHANGE, EXCHANGE_DECLARE) => "exchange.declare",
        (CLASS_QUEUE, QUEUE_DECLARE) => "queue.declare",
        (CLASS_QUEUE, QUEUE_BIND) => "queue.bind",
        (CLASS_BASIC, BASIC_QOS) => "basic.qos",
        (CLASS_BASIC, BASIC_CONSUME) => "basic.consume",
        (CLASS_BASIC, BASIC_CANCEL) => "basic.cancel",
        (CLASS_BASIC, BASIC_PUBLISH) => "basic.publish",
        (CLASS_BASIC, BASIC_ACK) => "basic.ack",
        (CLASS_BASIC, BASIC_REJECT) => "basic.reject",
        (CLASS_BASIC, BASIC_NACK) => "basic.nack",
        _ => "",
    };
    if name.is_empty() {
        format!("class {} method {}", class_id, method_id)
    } else {
        name.to_string()
    }
}

// ============================================================================
// Decoding
// ============================================================================

/// Bounds-checked reader over a frame payload.
///
/// Every accessor returns `Err` rather than panicking when the declared length of a field
/// exceeds the bytes remaining, which is the whole attack surface of a length-prefixed
/// binary protocol.
/// How deeply an AMQP field table may nest before the frame is refused.
///
/// Field tables are recursive: a value may itself be a table (`F`) or an array (`A`) of
/// values. Nothing in AMQP 0-9-1 bounds that, and nothing here did either — see
/// `field_value_at` for why an unbounded version was a pre-authentication kill of the entire
/// process.
///
/// Real tables are one or two deep. `client-properties` is a flat map with a nested
/// `capabilities` table inside it, which is depth 2, and `amq-protocol` — what lapin emits —
/// produces nothing deeper. 32 is generous by an order of magnitude and still bottoms the
/// recursion out in a few hundred bytes of stack.
const MAX_FIELD_TABLE_DEPTH: usize = 32;

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow!("AMQP field length {} overflows the payload offset", n))?;
        let slice = self.buf.get(self.pos..end).ok_or_else(|| {
            anyhow!(
                "AMQP payload truncated: {} bytes wanted at offset {}, only {} available",
                n,
                self.pos,
                self.remaining()
            )
        })?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b: [u8; 1] = self
            .take(1)?
            .try_into()
            .map_err(|_| anyhow!("AMQP octet read failed"))?;
        Ok(b[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b: [u8; 2] = self
            .take(2)?
            .try_into()
            .map_err(|_| anyhow!("AMQP short read failed"))?;
        Ok(u16::from_be_bytes(b))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| anyhow!("AMQP long read failed"))?;
        Ok(u32::from_be_bytes(b))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b: [u8; 8] = self
            .take(8)?
            .try_into()
            .map_err(|_| anyhow!("AMQP long-long read failed"))?;
        Ok(u64::from_be_bytes(b))
    }

    /// `shortstr`: one length octet then that many bytes of UTF-8.
    pub fn short_string(&mut self) -> Result<String> {
        let len = self.u8()? as usize;
        let bytes = self.take(len)?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// `longstr`: a 32-bit length then that many raw bytes.
    pub fn long_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    /// `longstr` rendered as text. AMQP long strings are byte strings; anything that is
    /// not valid UTF-8 is rendered lossily rather than handed to the model as bytes.
    pub fn long_string(&mut self) -> Result<String> {
        let bytes = self.long_bytes()?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// `bit` fields: packed least-significant-bit first, 8 to an octet.
    pub fn bits(&mut self, count: usize) -> Result<Vec<bool>> {
        let bytes = self.take(count.div_ceil(8))?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let byte = bytes.get(i / 8).copied().unwrap_or(0);
            out.push(byte & (1u8 << (i % 8)) != 0);
        }
        Ok(out)
    }

    /// `table`: a 32-bit byte length then key/value pairs, decoded to a JSON object.
    ///
    /// The table is consumed from the outer payload by its declared length *first*, so a
    /// value of a type this decoder does not recognise costs only the rest of that table:
    /// the entries decoded so far are returned and the outer payload stays in sync.
    pub fn field_table(&mut self) -> Result<Value> {
        self.field_table_at(0)
    }

    fn field_table_at(&mut self, depth: usize) -> Result<Value> {
        if depth >= MAX_FIELD_TABLE_DEPTH {
            return Err(anyhow!(
                "AMQP field table nested deeper than {} levels",
                MAX_FIELD_TABLE_DEPTH
            ));
        }
        let body = self.long_bytes()?;
        let mut inner = Decoder::new(body);
        let mut map = Map::new();
        while inner.remaining() > 0 {
            let Ok(key) = inner.short_string() else { break };
            match inner.field_value_at(depth + 1) {
                Ok(value) => {
                    map.insert(key, value);
                }
                Err(_) => break,
            }
        }
        Ok(Value::Object(map))
    }

    /// One typed field-table value. Type ids follow RabbitMQ's dialect, which is what
    /// `amq-protocol` (and therefore `lapin`) emits: `s` is a signed 16-bit integer, not
    /// a short string, and both `l` and `L` are signed 64-bit.
    ///
    /// `depth` is not decoration. A field value may be a nested table (`F`) or a nested array
    /// (`A`), so this is mutually recursive with itself and with `field_table_at`, and one
    /// more level costs the peer **five bytes** on the wire (`A` plus a four-byte length). At
    /// the default `frame_max` of 128 KiB that is ~26 000 levels in a single frame, and at the
    /// permitted maximum of 1 MiB about 210 000 — far past what a 2 MiB tokio worker stack
    /// survives. The overflow is a `SIGSEGV` against a guard page, not a panic, so
    /// `tokio::spawn` cannot contain it and the whole NetGet process dies, taking every other
    /// server and client in it.
    ///
    /// It was reachable **before authentication**, in two writes: the protocol header, then
    /// one `Connection.Start-Ok` whose `client-properties` table `mod.rs` decodes in
    /// `Phase::AwaitStartOk`, before any credential is examined and before any model decision
    /// is asked for.
    fn field_value_at(&mut self, depth: usize) -> Result<Value> {
        if depth >= MAX_FIELD_TABLE_DEPTH {
            return Err(anyhow!(
                "AMQP field table nested deeper than {} levels",
                MAX_FIELD_TABLE_DEPTH
            ));
        }
        let tag = self.u8()?;
        let value = match tag {
            b't' => Value::Bool(self.u8()? != 0),
            b'b' => Value::from(self.u8()? as i8),
            b'B' => Value::from(self.u8()?),
            b's' | b'U' => Value::from(self.u16()? as i16),
            b'u' => Value::from(self.u16()?),
            b'I' => Value::from(self.u32()? as i32),
            b'i' => Value::from(self.u32()?),
            b'l' | b'L' => Value::from(self.u64()? as i64),
            b'f' => Value::from(f32::from_bits(self.u32()?)),
            b'd' => Value::from(f64::from_bits(self.u64()?)),
            b'D' => {
                let scale = self.u8()? as u32;
                let raw = self.u32()? as i32;
                Value::from(raw as f64 / 10f64.powi(scale as i32))
            }
            b'S' => Value::String(self.long_string()?),
            b'A' => {
                let body = self.long_bytes()?;
                let mut inner = Decoder::new(body);
                let mut items = Vec::new();
                while inner.remaining() > 0 {
                    match inner.field_value_at(depth + 1) {
                        Ok(v) => items.push(v),
                        Err(_) => break,
                    }
                }
                Value::Array(items)
            }
            b'T' => Value::from(self.u64()?),
            b'F' => self.field_table_at(depth + 1)?,
            b'x' => {
                let bytes = self.long_bytes()?;
                Value::String(String::from_utf8_lossy(bytes).into_owned())
            }
            b'V' => Value::Null,
            other => {
                return Err(anyhow!(
                    "unknown AMQP field-table type id {:?}",
                    other as char
                ))
            }
        };
        Ok(value)
    }
}

// ============================================================================
// Encoding
// ============================================================================

/// Writer for AMQP method arguments and property lists.
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// `shortstr`. The wire format allows 255 bytes; longer input is truncated at a UTF-8
    /// char boundary, never sliced by byte index.
    pub fn short_string(&mut self, s: &str) {
        let s = truncate_str(s, 255);
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
    }

    pub fn long_string(&mut self, bytes: &[u8]) {
        self.u32(bytes.len() as u32);
        self.buf.extend_from_slice(bytes);
    }

    pub fn bits(&mut self, bits: &[bool]) {
        for chunk in bits.chunks(8) {
            let mut byte = 0u8;
            for (i, set) in chunk.iter().enumerate() {
                if *set {
                    byte |= 1u8 << i;
                }
            }
            self.buf.push(byte);
        }
    }

    /// Encode a JSON object as a field table. Non-object input encodes as an empty table.
    pub fn field_table(&mut self, value: &Value) {
        let mut inner = Encoder::new();
        if let Some(map) = value.as_object() {
            for (key, val) in map {
                inner.short_string(key);
                inner.field_value(val);
            }
        }
        self.long_string(&inner.buf);
    }

    fn field_value(&mut self, value: &Value) {
        match value {
            Value::Null => self.u8(b'V'),
            Value::Bool(b) => {
                self.u8(b't');
                self.u8(u8::from(*b));
            }
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    self.u8(b'l');
                    self.u64(i as u64);
                } else {
                    self.u8(b'd');
                    self.u64(n.as_f64().unwrap_or(0.0).to_bits());
                }
            }
            Value::String(s) => {
                self.u8(b'S');
                self.long_string(s.as_bytes());
            }
            Value::Array(items) => {
                let mut inner = Encoder::new();
                for item in items {
                    inner.field_value(item);
                }
                self.u8(b'A');
                self.long_string(&inner.buf);
            }
            Value::Object(_) => {
                self.u8(b'F');
                self.field_table(value);
            }
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

// ============================================================================
// Frames
// ============================================================================

/// One decoded frame: its type, channel, and payload with the `0xCE` end marker removed.
pub struct Frame {
    pub frame_type: u8,
    pub channel: u16,
    pub payload: Vec<u8>,
}

/// Build a method frame: `class-id`, `method-id`, then the already-encoded arguments.
pub fn method_frame(channel: u16, class_id: u16, method_id: u16, args: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + args.len());
    payload.extend_from_slice(&class_id.to_be_bytes());
    payload.extend_from_slice(&method_id.to_be_bytes());
    payload.extend_from_slice(args);
    raw_frame(FRAME_METHOD, channel, &payload)
}

/// Build a content header frame (0-9-1 section 4.2.6.1).
pub fn content_header_frame(channel: u16, class_id: u16, body_size: u64, props: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(12 + props.len());
    payload.extend_from_slice(&class_id.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes()); // weight, unused
    payload.extend_from_slice(&body_size.to_be_bytes());
    payload.extend_from_slice(props);
    raw_frame(FRAME_HEADER, channel, &payload)
}

/// Split a message body into as many body frames as the negotiated frame size requires.
///
/// A zero-length body produces no frames at all, which is what the spec expects when the
/// content header declares `body-size` 0.
pub fn body_frames(channel: u16, body: &[u8], max_frame_size: usize) -> Vec<Vec<u8>> {
    let chunk_size = max_frame_size.saturating_sub(FRAME_OVERHEAD).max(1);
    body.chunks(chunk_size)
        .map(|chunk| raw_frame(FRAME_BODY, channel, chunk))
        .collect()
}

pub fn heartbeat_frame() -> Vec<u8> {
    raw_frame(FRAME_HEARTBEAT, 0, &[])
}

fn raw_frame(frame_type: u8, channel: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_OVERHEAD + payload.len());
    out.push(frame_type);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.push(FRAME_END);
    out
}

// ============================================================================
// Basic content properties
// ============================================================================

/// The Basic class property list carried in a content header frame.
///
/// Property presence is a 16-bit flag word, most significant bit first, and a set bit 0
/// continues the flags into another word. Both directions are handled here.
#[derive(Default, Clone, Debug)]
pub struct BasicProperties {
    pub content_type: Option<String>,
    pub content_encoding: Option<String>,
    pub headers: Option<Value>,
    pub delivery_mode: Option<u8>,
    pub priority: Option<u8>,
    pub correlation_id: Option<String>,
    pub reply_to: Option<String>,
    pub expiration: Option<String>,
    pub message_id: Option<String>,
    pub timestamp: Option<u64>,
    pub kind: Option<String>,
    pub user_id: Option<String>,
    pub app_id: Option<String>,
    pub cluster_id: Option<String>,
}

impl BasicProperties {
    pub fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        // Bit 0 of a flags word means the flags continue in another word. All words are
        // read before any property value; no defined Basic property lives past the first
        // word, so the continuations are consumed and ignored.
        let flags = d.u16()?;
        let mut word = flags;
        let mut words = 1;
        while word & 1 != 0 {
            if words >= 8 {
                return Err(anyhow!(
                    "AMQP content header declares more than 8 property flag words"
                ));
            }
            word = d.u16()?;
            words += 1;
        }

        let has = |bit: u32| flags & (1 << (15 - bit)) != 0;
        let mut props = BasicProperties::default();
        if has(0) {
            props.content_type = Some(d.short_string()?);
        }
        if has(1) {
            props.content_encoding = Some(d.short_string()?);
        }
        if has(2) {
            props.headers = Some(d.field_table()?);
        }
        if has(3) {
            props.delivery_mode = Some(d.u8()?);
        }
        if has(4) {
            props.priority = Some(d.u8()?);
        }
        if has(5) {
            props.correlation_id = Some(d.short_string()?);
        }
        if has(6) {
            props.reply_to = Some(d.short_string()?);
        }
        if has(7) {
            props.expiration = Some(d.short_string()?);
        }
        if has(8) {
            props.message_id = Some(d.short_string()?);
        }
        if has(9) {
            props.timestamp = Some(d.u64()?);
        }
        if has(10) {
            props.kind = Some(d.short_string()?);
        }
        if has(11) {
            props.user_id = Some(d.short_string()?);
        }
        if has(12) {
            props.app_id = Some(d.short_string()?);
        }
        if has(13) {
            props.cluster_id = Some(d.short_string()?);
        }
        Ok(props)
    }

    pub fn encode(&self) -> Vec<u8> {
        let present = [
            self.content_type.is_some(),
            self.content_encoding.is_some(),
            self.headers.is_some(),
            self.delivery_mode.is_some(),
            self.priority.is_some(),
            self.correlation_id.is_some(),
            self.reply_to.is_some(),
            self.expiration.is_some(),
            self.message_id.is_some(),
            self.timestamp.is_some(),
            self.kind.is_some(),
            self.user_id.is_some(),
            self.app_id.is_some(),
            self.cluster_id.is_some(),
        ];
        let mut flags: u16 = 0;
        for (bit, set) in present.iter().enumerate() {
            if *set {
                flags |= 1 << (15 - bit as u16);
            }
        }

        let mut e = Encoder::new();
        e.u16(flags);
        if let Some(v) = &self.content_type {
            e.short_string(v);
        }
        if let Some(v) = &self.content_encoding {
            e.short_string(v);
        }
        if let Some(v) = &self.headers {
            e.field_table(v);
        }
        if let Some(v) = self.delivery_mode {
            e.u8(v);
        }
        if let Some(v) = self.priority {
            e.u8(v);
        }
        if let Some(v) = &self.correlation_id {
            e.short_string(v);
        }
        if let Some(v) = &self.reply_to {
            e.short_string(v);
        }
        if let Some(v) = &self.expiration {
            e.short_string(v);
        }
        if let Some(v) = &self.message_id {
            e.short_string(v);
        }
        if let Some(v) = self.timestamp {
            e.u64(v);
        }
        if let Some(v) = &self.kind {
            e.short_string(v);
        }
        if let Some(v) = &self.user_id {
            e.short_string(v);
        }
        if let Some(v) = &self.app_id {
            e.short_string(v);
        }
        if let Some(v) = &self.cluster_id {
            e.short_string(v);
        }
        e.into_vec()
    }

    /// Render for an event: only the properties the publisher actually set, as JSON.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(v) = &self.content_type {
            map.insert("content_type".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.content_encoding {
            map.insert("content_encoding".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.headers {
            map.insert("headers".into(), v.clone());
        }
        if let Some(v) = self.delivery_mode {
            map.insert("delivery_mode".into(), Value::from(v));
        }
        if let Some(v) = self.priority {
            map.insert("priority".into(), Value::from(v));
        }
        if let Some(v) = &self.correlation_id {
            map.insert("correlation_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.reply_to {
            map.insert("reply_to".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.expiration {
            map.insert("expiration".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.message_id {
            map.insert("message_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.timestamp {
            map.insert("timestamp".into(), Value::from(v));
        }
        if let Some(v) = &self.kind {
            map.insert("type".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.user_id {
            map.insert("user_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.app_id {
            map.insert("app_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.cluster_id {
            map.insert("cluster_id".into(), Value::String(v.clone()));
        }
        Value::Object(map)
    }
}

/// Split the `\0user\0password` payload of a SASL PLAIN response.
///
/// Returns `(username, has_password)`. The password itself is deliberately not returned:
/// nothing in this protocol has a reason to put it in an event or a log.
pub fn parse_plain_response(response: &str) -> (Option<String>, bool) {
    let mut parts = response.split('\0');
    // The first field is the authorization identity, normally empty.
    let _authzid = parts.next();
    let username = parts.next().filter(|s| !s.is_empty()).map(str::to_string);
    let has_password = parts.next().is_some_and(|p| !p.is_empty());
    (username, has_password)
}
