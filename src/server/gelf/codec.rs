//! Bounded GELF 1.1 JSON, compression, TCP framing and source-correlated UDP chunks.
use anyhow::{bail, ensure, Context, Result};
use flate2::{
    bufread::GzDecoder,
    write::{GzEncoder, ZlibEncoder},
    Compression as FlateCompression,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    net::SocketAddr,
    time::Duration,
};
use tokio::time::Instant;

pub const DEFAULT_TRANSPORT: &str = "udp";
pub const DEFAULT_COMPRESSION: &str = "auto";
pub const DEFAULT_CHUNK_SIZE: usize = 1420;
pub const DEFAULT_LLM_FALLBACK: bool = false;
pub const MAX_MESSAGE_BYTES: usize = 256 * 1024;
pub const MAX_DATAGRAM_BYTES: usize = 8192;
pub const MAX_CHUNKS: usize = 128;
pub const MAX_PENDING_MESSAGES: usize = 128;
pub const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RECENT_IDS: usize = 256;
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
}
impl Transport {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "udp" => Ok(Self::Udp),
            "tcp" => Ok(Self::Tcp),
            _ => bail!("transport must be udp or tcp"),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Compression {
    None,
    Gzip,
    Zlib,
}
impl Compression {
    pub fn parse(s: &str, transport: Transport) -> Result<Self> {
        match (s, transport) {
            ("auto", Transport::Udp) | ("gzip", Transport::Udp) => Ok(Self::Gzip),
            ("auto" | "none", Transport::Tcp) | ("none", Transport::Udp) => Ok(Self::None),
            ("zlib", Transport::Udp) => Ok(Self::Zlib),
            ("gzip" | "zlib", Transport::Tcp) => bail!("GELF TCP does not permit compression"),
            _ => bail!("compression must be auto, none, gzip or zlib"),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub host: String,
    pub short_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Names have no leading underscore; the wire encoder adds it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_fields: BTreeMap<String, Value>,
}
fn validate(m: &Message) -> Result<()> {
    ensure!(
        !m.host.is_empty() && !m.short_message.is_empty(),
        "host and short_message must be nonempty"
    );
    if let Some(ts) = m.timestamp {
        ensure!(
            ts.is_finite() && ts >= 0.0,
            "timestamp must be finite nonnegative UNIX seconds"
        );
    }
    if let Some(level) = m.level {
        ensure!(level <= 7, "level must be 0..7");
    }
    for (key, value) in &m.additional_fields {
        ensure!(
            !key.is_empty()
                && key != "id"
                && key
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-')),
            "invalid additional field name"
        );
        ensure!(
            value.is_string() || value.is_number(),
            "additional fields must be strings or numbers"
        );
    }
    Ok(())
}
pub fn encode_json(m: &Message) -> Result<Vec<u8>> {
    validate(m)?;
    let mut object = serde_json::to_value(m)?
        .as_object()
        .context("message object")?
        .clone();
    object.remove("additional_fields");
    object.insert("version".into(), Value::String("1.1".into()));
    for (key, value) in &m.additional_fields {
        object.insert(format!("_{key}"), value.clone());
    }
    let bytes = serde_json::to_vec(&object)?;
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "GELF message exceeds {MAX_MESSAGE_BYTES} bytes"
    );
    Ok(bytes)
}
pub fn parse_json(bytes: &[u8]) -> Result<Message> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_MESSAGE_BYTES,
        "GELF message byte limit"
    );
    let mut object: Map<String, Value> =
        serde_json::from_slice(bytes).context("invalid GELF JSON object")?;
    ensure!(
        object.remove("version") == Some(Value::String("1.1".into())),
        "GELF version must be 1.1"
    );
    let mut extras = BTreeMap::new();
    let keys: Vec<_> = object
        .keys()
        .filter(|k| k.starts_with('_'))
        .cloned()
        .collect();
    for key in keys {
        extras.insert(key[1..].to_owned(), object.remove(&key).unwrap());
    }
    ensure!(
        !object.contains_key("additional_fields"),
        "unprefixed additional_fields is not a GELF wire field"
    );
    let mut m: Message =
        serde_json::from_value(Value::Object(object)).context("invalid GELF fields")?;
    m.additional_fields = extras;
    validate(&m)?;
    Ok(m)
}
pub fn decompress(bytes: &[u8]) -> Result<Message> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_MESSAGE_BYTES,
        "compressed message byte limit"
    );
    let decoded = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(bytes);
        let mut out = Vec::new();
        decoder
            .by_ref()
            .take((MAX_MESSAGE_BYTES + 1) as u64)
            .read_to_end(&mut out)?;
        ensure!(
            out.len() <= MAX_MESSAGE_BYTES,
            "decompressed message byte limit"
        );
        ensure!(
            decoder.get_ref().is_empty(),
            "trailing bytes after gzip message"
        );
        out
    } else if bytes.len() >= 2
        && bytes[0] & 0x0f == 8
        && (u16::from_be_bytes([bytes[0], bytes[1]]) % 31 == 0)
    {
        // The streaming Read adapter can report EOF before the zlib checksum
        // arrives. Require an actual StreamEnd, complete trailer and exact input.
        let mut decoder = flate2::Decompress::new(true);
        let mut out = vec![0u8; MAX_MESSAGE_BYTES + 1];
        let status = decoder.decompress(bytes, &mut out, flate2::FlushDecompress::Finish)?;
        ensure!(
            status == flate2::Status::StreamEnd,
            "incomplete or oversized zlib message"
        );
        ensure!(
            decoder.total_in() as usize == bytes.len(),
            "trailing bytes after zlib message"
        );
        ensure!(
            decoder.total_out() as usize <= MAX_MESSAGE_BYTES,
            "decompressed message byte limit"
        );
        out.truncate(decoder.total_out() as usize);
        out
    } else {
        bytes.to_vec()
    };
    parse_json(&decoded)
}
pub fn encode_udp(
    m: &Message,
    compression: Compression,
    chunk_size: usize,
    id: [u8; 8],
) -> Result<Vec<Vec<u8>>> {
    ensure!(
        (13..=MAX_DATAGRAM_BYTES).contains(&chunk_size),
        "chunk_size must be 13..={MAX_DATAGRAM_BYTES}"
    );
    let json = encode_json(m)?;
    let bytes = match compression {
        Compression::None => json,
        Compression::Gzip => {
            let mut encoder = GzEncoder::new(Vec::new(), FlateCompression::default());
            encoder.write_all(&json)?;
            encoder.finish()?
        }
        Compression::Zlib => {
            let mut encoder = ZlibEncoder::new(Vec::new(), FlateCompression::default());
            encoder.write_all(&json)?;
            encoder.finish()?
        }
    };
    ensure!(
        bytes.len() <= MAX_MESSAGE_BYTES,
        "compressed message byte limit"
    );
    if bytes.len() <= chunk_size {
        return Ok(vec![bytes]);
    }
    let payload_size = chunk_size - 12;
    let count = bytes.len().div_ceil(payload_size);
    ensure!(
        count <= MAX_CHUNKS,
        "message requires more than {MAX_CHUNKS} chunks"
    );
    Ok(bytes
        .chunks(payload_size)
        .enumerate()
        .map(|(seq, payload)| {
            let mut out = Vec::with_capacity(12 + payload.len());
            out.extend_from_slice(&[0x1e, 0x0f]);
            out.extend_from_slice(&id);
            out.push(seq as u8);
            out.push(count as u8);
            out.extend_from_slice(payload);
            out
        })
        .collect())
}
struct Pending {
    started: Instant,
    count: usize,
    bytes: usize,
    parts: Vec<Option<Vec<u8>>>,
}
type Key = (SocketAddr, [u8; 8]);
#[derive(Default)]
pub struct Reassembler {
    pending: HashMap<Key, Pending>,
    recent: HashMap<Key, Instant>,
    bytes: usize,
}
impl Reassembler {
    pub fn recent_count(&self) -> usize {
        self.recent.len()
    }
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    pub fn pending_bytes(&self) -> usize {
        self.bytes
    }
    pub fn expire(&mut self, now: Instant) {
        self.recent.retain(|_, until| *until > now);
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.started) >= CHUNK_TIMEOUT)
            .map(|(k, _)| *k)
            .collect();
        for key in expired {
            self.drop_pending(key);
            self.remember(key, now);
        }
    }
    fn drop_pending(&mut self, key: Key) {
        if let Some(p) = self.pending.remove(&key) {
            self.bytes -= p.bytes;
        }
    }
    fn remember(&mut self, key: Key, now: Instant) {
        if self.recent.len() == MAX_RECENT_IDS {
            if let Some(old) = self.recent.iter().min_by_key(|(_, v)| **v).map(|(k, _)| *k) {
                self.recent.remove(&old);
            }
        }
        self.recent.insert(key, now + CHUNK_TIMEOUT);
    }
    pub fn push(
        &mut self,
        peer: SocketAddr,
        bytes: &[u8],
        now: Instant,
    ) -> Result<Option<Message>> {
        self.expire(now);
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_DATAGRAM_BYTES,
            "GELF datagram byte limit"
        );
        if !bytes.starts_with(&[0x1e, 0x0f]) {
            return decompress(bytes).map(Some);
        }
        ensure!(bytes.len() > 12, "empty or truncated chunk");
        let id: [u8; 8] = bytes[2..10].try_into().unwrap();
        let key = (peer, id);
        let seq = bytes[10] as usize;
        let count = bytes[11] as usize;
        ensure!(
            (1..=MAX_CHUNKS).contains(&count) && seq < count,
            "invalid chunk sequence or count"
        );
        if self.recent.contains_key(&key) {
            return Ok(None);
        }
        if !self.pending.contains_key(&key) {
            ensure!(
                self.pending.len() < MAX_PENDING_MESSAGES,
                "pending message count limit"
            );
            self.pending.insert(
                key,
                Pending {
                    started: now,
                    count,
                    bytes: 0,
                    parts: vec![None; count],
                },
            );
        }
        let pending = self.pending.get(&key).unwrap();
        if pending.count != count
            || pending.parts[seq]
                .as_ref()
                .is_some_and(|p| p != &bytes[12..])
        {
            self.drop_pending(key);
            self.remember(key, now);
            bail!("conflicting GELF chunks");
        }
        if pending.parts[seq].is_some() {
            return Ok(None);
        }
        if pending.bytes + bytes.len() - 12 > MAX_MESSAGE_BYTES
            || self.bytes + bytes.len() - 12 > MAX_PENDING_BYTES
        {
            self.drop_pending(key);
            self.remember(key, now);
            bail!("chunk reassembly byte limit");
        }
        let pending = self.pending.get_mut(&key).unwrap();
        pending.parts[seq] = Some(bytes[12..].to_vec());
        pending.bytes += bytes.len() - 12;
        self.bytes += bytes.len() - 12;
        if pending.parts.iter().any(Option::is_none) {
            return Ok(None);
        }
        let pending = self.pending.remove(&key).unwrap();
        self.bytes -= pending.bytes;
        self.remember(key, now);
        let all: Vec<u8> = pending.parts.into_iter().flat_map(|p| p.unwrap()).collect();
        decompress(&all).map(Some)
    }
}
#[derive(Default)]
pub struct TcpDecoder {
    bytes: Vec<u8>,
}
impl TcpDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            self.bytes.len() + bytes.len() <= MAX_MESSAGE_BYTES + 8192,
            "TCP buffered byte limit"
        );
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    pub fn next_message(&mut self) -> Result<Option<Message>> {
        if let Some(end) = self.bytes.iter().position(|b| *b == 0) {
            ensure!(end <= MAX_MESSAGE_BYTES, "TCP message byte limit");
            let message = parse_json(&self.bytes[..end])?;
            self.bytes.drain(..=end);
            return Ok(Some(message));
        }
        ensure!(
            self.bytes.len() <= MAX_MESSAGE_BYTES,
            "TCP message byte limit"
        );
        Ok(None)
    }
    pub fn finish(&self) -> Result<()> {
        ensure!(self.bytes.is_empty(), "EOF within GELF TCP frame");
        Ok(())
    }
}
