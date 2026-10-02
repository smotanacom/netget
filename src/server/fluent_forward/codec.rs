//! Native bounded MessagePack for Forward's typed JSON record surface.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::io::{Read, Write};
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_RECORDS: usize = 256;
pub const MAX_DEPTH: usize = 32;
pub const MAX_VALUES: usize = 16384;
pub const DEFAULT_LLM_FALLBACK: bool = false;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timestamp {
    pub seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nanoseconds: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub timestamp: Timestamp,
    pub record: Map<String, Value>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub tag: String,
    pub entries: Vec<Entry>,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub require_ack: bool,
    #[serde(skip)]
    pub chunk: Option<String>,
}
fn default_mode() -> String {
    "forward".into()
}
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Nil,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(Vec<u8>),
    Bin(Vec<u8>),
    Array(Vec<Node>),
    Map(Vec<(Node, Node)>),
    Ext(i8, Vec<u8>),
}
impl Node {
    pub fn string(&self) -> Result<&str> {
        match self {
            Self::Str(b) => Ok(std::str::from_utf8(b)?),
            _ => bail!("expected MessagePack string"),
        }
    }
    fn json(self) -> Result<Value> {
        Ok(match self {
            Self::Nil => Value::Null,
            Self::Bool(b) => Value::Bool(b),
            Self::Int(i) => Value::Number(i.into()),
            Self::Uint(i) => Value::Number(i.into()),
            Self::Float(f) => Value::Number(Number::from_f64(f).context("nonfinite record value")?),
            Self::Str(b) => Value::String(String::from_utf8(b)?),
            Self::Array(a) => Value::Array(a.into_iter().map(Self::json).collect::<Result<_>>()?),
            Self::Map(pairs) => {
                let mut out = Map::new();
                for (k, v) in pairs {
                    let k = k.string()?.to_owned();
                    ensure!(out.insert(k, v.json()?).is_none(), "duplicate record key");
                }
                Value::Object(out)
            }
            _ => bail!("binary/extension record values are outside the typed JSON surface"),
        })
    }
    fn uint(&self) -> Result<u64> {
        match self {
            Self::Uint(n) => Ok(*n),
            Self::Int(n) => Ok(u64::try_from(*n)?),
            _ => bail!("expected nonnegative integer"),
        }
    }
}
#[derive(Debug)]
enum ParseError {
    Short,
    Bad(&'static str),
}
fn take<'a>(b: &'a [u8], at: &mut usize, n: usize) -> std::result::Result<&'a [u8], ParseError> {
    let end = at
        .checked_add(n)
        .ok_or(ParseError::Bad("length overflow"))?;
    let out = b.get(*at..end).ok_or(ParseError::Short)?;
    *at = end;
    Ok(out)
}
fn uint(b: &[u8], at: &mut usize, n: usize) -> std::result::Result<u64, ParseError> {
    let mut out = 0;
    for v in take(b, at, n)? {
        out = out * 256 + u64::from(*v);
    }
    Ok(out)
}
fn len(b: &[u8], at: &mut usize, n: usize) -> std::result::Result<usize, ParseError> {
    let n = uint(b, at, n)? as usize;
    if n > MAX_FRAME_BYTES {
        return Err(ParseError::Bad("declared length exceeds frame limit"));
    }
    Ok(n)
}
fn parse(
    b: &[u8],
    at: &mut usize,
    depth: usize,
    budget: &mut usize,
) -> std::result::Result<Node, ParseError> {
    if depth > MAX_DEPTH {
        return Err(ParseError::Bad("MessagePack nesting limit"));
    }
    if *budget == 0 {
        return Err(ParseError::Bad("MessagePack value count limit"));
    }
    *budget -= 1;
    let m = take(b, at, 1)?[0];
    let (kind, count) = match m {
        0x00..=0x7f => return Ok(Node::Uint(m as u64)),
        0xe0..=0xff => return Ok(Node::Int(m as i8 as i64)),
        0xc0 => return Ok(Node::Nil),
        0xc2..=0xc3 => return Ok(Node::Bool(m == 0xc3)),
        0xcc..=0xcf => return Ok(Node::Uint(uint(b, at, 1usize << (m - 0xcc))?)),
        0xd0..=0xd3 => {
            let size = 1usize << (m - 0xd0);
            let n = uint(b, at, size)?;
            return Ok(Node::Int(match size {
                1 => n as i8 as i64,
                2 => n as i16 as i64,
                4 => n as i32 as i64,
                _ => n as i64,
            }));
        }
        0xca => return Ok(Node::Float(f32::from_bits(uint(b, at, 4)? as u32) as f64)),
        0xcb => return Ok(Node::Float(f64::from_bits(uint(b, at, 8)?))),
        0xa0..=0xbf => (0, (m & 31) as usize),
        0xd9 => (0, len(b, at, 1)?),
        0xda => (0, len(b, at, 2)?),
        0xdb => (0, len(b, at, 4)?),
        0xc4 => (1, len(b, at, 1)?),
        0xc5 => (1, len(b, at, 2)?),
        0xc6 => (1, len(b, at, 4)?),
        0x90..=0x9f => (2, (m & 15) as usize),
        0xdc => (2, len(b, at, 2)?),
        0xdd => (2, len(b, at, 4)?),
        0x80..=0x8f => (3, (m & 15) as usize),
        0xde => (3, len(b, at, 2)?),
        0xdf => (3, len(b, at, 4)?),
        0xd4..=0xd8 => (4, 1usize << (m - 0xd4)),
        0xc7 => (4, len(b, at, 1)?),
        0xc8 => (4, len(b, at, 2)?),
        0xc9 => (4, len(b, at, 4)?),
        _ => return Err(ParseError::Bad("invalid MessagePack marker")),
    };
    Ok(match kind {
        0 => Node::Str(take(b, at, count)?.to_vec()),
        1 => Node::Bin(take(b, at, count)?.to_vec()),
        2 => {
            if count > *budget {
                return Err(ParseError::Bad("declared array count limit"));
            }
            let mut out = Vec::new();
            for _ in 0..count {
                out.push(parse(b, at, depth + 1, budget)?);
            }
            Node::Array(out)
        }
        3 => {
            if count > *budget / 2 {
                return Err(ParseError::Bad("declared map count limit"));
            }
            let mut out = Vec::new();
            for _ in 0..count {
                let k = parse(b, at, depth + 1, budget)?;
                let v = parse(b, at, depth + 1, budget)?;
                out.push((k, v));
            }
            Node::Map(out)
        }
        _ => {
            let t = take(b, at, 1)?[0] as i8;
            Node::Ext(t, take(b, at, count)?.to_vec())
        }
    })
}
pub fn parse_one(b: &[u8]) -> Result<Option<(Node, usize)>> {
    let mut at = 0;
    match parse(b, &mut at, 0, &mut MAX_VALUES.clone()) {
        Ok(n) => {
            ensure!(at <= MAX_FRAME_BYTES, "MessagePack frame byte limit");
            Ok(Some((n, at)))
        }
        Err(ParseError::Short) => {
            ensure!(
                b.len() <= MAX_FRAME_BYTES,
                "incomplete MessagePack frame byte limit"
            );
            Ok(None)
        }
        Err(ParseError::Bad(message)) => bail!(message),
    }
}
#[derive(Default)]
pub struct Decoder {
    bytes: Vec<u8>,
}
impl Decoder {
    pub fn feed(&mut self, b: &[u8]) -> Result<()> {
        ensure!(
            self.bytes.len() + b.len() <= MAX_FRAME_BYTES + 8192,
            "buffered byte limit"
        );
        self.bytes.extend_from_slice(b);
        Ok(())
    }
    pub fn next_node(&mut self) -> Result<Option<Node>> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        if let Some((n, len)) = parse_one(&self.bytes)? {
            self.bytes.drain(..len);
            Ok(Some(n))
        } else {
            Ok(None)
        }
    }
    pub fn finish(&self) -> Result<()> {
        ensure!(self.bytes.is_empty(), "EOF in MessagePack frame");
        Ok(())
    }
}
fn strnode(s: &str) -> Node {
    Node::Str(s.as_bytes().to_vec())
}
fn jsonnode(v: &Value, depth: usize, budget: &mut usize) -> Result<Node> {
    ensure!(
        depth <= MAX_DEPTH && *budget > 0,
        "record nesting/value count limit"
    );
    *budget -= 1;
    Ok(match v {
        Value::Null => Node::Nil,
        Value::Bool(b) => Node::Bool(*b),
        Value::Number(n) => {
            if let Some(n) = n.as_u64() {
                Node::Uint(n)
            } else if let Some(n) = n.as_i64() {
                Node::Int(n)
            } else {
                let n = n.as_f64().context("invalid number")?;
                ensure!(n.is_finite(), "nonfinite number");
                Node::Float(n)
            }
        }
        Value::String(s) => strnode(s),
        Value::Array(a) => Node::Array(
            a.iter()
                .map(|v| jsonnode(v, depth + 1, budget))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => Node::Map(
            map.iter()
                .map(|(k, v)| {
                    ensure!(*budget > 0, "record value count limit");
                    *budget -= 1;
                    Ok((strnode(k), jsonnode(v, depth + 1, budget)?))
                })
                .collect::<Result<_>>()?,
        ),
    })
}
fn header(
    out: &mut Vec<u8>,
    small: Option<(u8, usize)>,
    m8: Option<u8>,
    m16: u8,
    m32: u8,
    len: usize,
) {
    if let Some((m, max)) = small {
        if len < max {
            out.push(m | len as u8);
            return;
        }
    }
    if let Some(m) = m8 {
        if len <= 255 {
            out.extend_from_slice(&[m, len as u8]);
            return;
        }
    }
    if len <= 65535 {
        out.push(m16);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(m32);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
}
fn emit(n: &Node, out: &mut Vec<u8>) -> Result<()> {
    match n {
        Node::Nil => out.push(0xc0),
        Node::Bool(b) => out.push(if *b { 0xc3 } else { 0xc2 }),
        Node::Uint(n) => {
            if *n < 128 {
                out.push(*n as u8)
            } else {
                out.push(0xcf);
                out.extend_from_slice(&n.to_be_bytes())
            }
        }
        Node::Int(n) => {
            out.push(0xd3);
            out.extend_from_slice(&n.to_be_bytes())
        }
        Node::Float(n) => {
            ensure!(n.is_finite(), "nonfinite value");
            out.push(0xcb);
            out.extend_from_slice(&n.to_bits().to_be_bytes())
        }
        Node::Str(b) => {
            header(out, Some((0xa0, 32)), Some(0xd9), 0xda, 0xdb, b.len());
            out.extend_from_slice(b)
        }
        Node::Bin(b) => {
            header(out, None, Some(0xc4), 0xc5, 0xc6, b.len());
            out.extend_from_slice(b)
        }
        Node::Array(a) => {
            header(out, Some((0x90, 16)), None, 0xdc, 0xdd, a.len());
            for v in a {
                emit(v, out)?;
            }
        }
        Node::Map(a) => {
            header(out, Some((0x80, 16)), None, 0xde, 0xdf, a.len());
            for (k, v) in a {
                emit(k, out)?;
                emit(v, out)?;
            }
        }
        Node::Ext(t, b) => {
            ensure!(b.len() == 8, "only EventTime ext8 supported");
            out.extend_from_slice(&[0xd7, *t as u8]);
            out.extend_from_slice(b)
        }
    }
    ensure!(out.len() <= MAX_FRAME_BYTES, "encoded frame byte limit");
    Ok(())
}
pub fn encode_node(n: &Node) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    emit(n, &mut out)?;
    Ok(out)
}
fn timestamp(n: &Node) -> Result<Timestamp> {
    match n {
        Node::Ext(0, b) if b.len() == 8 => {
            let seconds = u32::from_be_bytes(b[..4].try_into()?) as u64;
            let nano = u32::from_be_bytes(b[4..].try_into()?);
            ensure!(nano < 1_000_000_000, "EventTime nanoseconds out of range");
            Ok(Timestamp {
                seconds,
                nanoseconds: Some(nano),
            })
        }
        _ => Ok(Timestamp {
            seconds: n.uint()?,
            nanoseconds: None,
        }),
    }
}
fn timestampnode(ts: &Timestamp) -> Result<Node> {
    if let Some(nano) = ts.nanoseconds {
        ensure!(nano < 1_000_000_000, "nanoseconds must be <1e9");
        let seconds = u32::try_from(ts.seconds).context("EventTime seconds exceed uint32")?;
        let mut b = seconds.to_be_bytes().to_vec();
        b.extend_from_slice(&nano.to_be_bytes());
        Ok(Node::Ext(0, b))
    } else {
        Ok(Node::Uint(ts.seconds))
    }
}
fn entry(n: Node) -> Result<Entry> {
    let Node::Array(mut a) = n else {
        bail!("entry must be array")
    };
    ensure!(a.len() == 2, "entry must contain timestamp and record");
    let record = a
        .pop()
        .unwrap()
        .json()?
        .as_object()
        .context("record must be map")?
        .clone();
    Ok(Entry {
        timestamp: timestamp(&a[0])?,
        record,
    })
}
pub fn parse_batch(n: Node) -> Result<Option<Batch>> {
    if n == Node::Nil {
        return Ok(None);
    }
    let Node::Array(a) = n else {
        bail!("Forward frame must be an array")
    };
    ensure!(a.len() >= 2 && a.len() <= 4, "invalid Forward frame arity");
    let tag = a[0].string()?.to_owned();
    ensure!(!tag.is_empty() && tag.len() <= 1024, "tag byte limit");
    let single = matches!(&a[1], Node::Uint(_) | Node::Int(_) | Node::Ext(_, _));
    let expected = if single { 3 } else { 2 };
    ensure!(
        a.len() == expected || a.len() == expected + 1,
        "invalid mode arity"
    );
    let options = if a.len() == expected + 1 {
        a.last()
            .unwrap()
            .clone()
            .json()?
            .as_object()
            .context("options must be a map")?
            .clone()
    } else {
        Map::new()
    };
    let chunk = options
        .get("chunk")
        .map(|v| {
            let s = v.as_str().context("chunk must be string")?;
            ensure!(
                !s.is_empty() && s.len() <= 256 && s.bytes().all(|b| b.is_ascii_graphic()),
                "chunk token byte limit"
            );
            Ok::<_, anyhow::Error>(s.to_owned())
        })
        .transpose()?;
    let mut mode = "message";
    let mut entries = if single {
        vec![Entry {
            timestamp: timestamp(&a[1])?,
            record: a[2]
                .clone()
                .json()?
                .as_object()
                .context("record must be map")?
                .clone(),
        }]
    } else {
        match &a[1] {
            Node::Array(records) => {
                mode = "forward";
                ensure!(records.len() <= MAX_RECORDS, "record count limit");
                records
                    .clone()
                    .into_iter()
                    .map(entry)
                    .collect::<Result<Vec<_>>>()?
            }
            Node::Bin(bytes) | Node::Str(bytes) => {
                mode = "packed";
                let mut bytes = bytes.clone();
                if let Some(compression) = options.get("compressed").filter(|v| *v != "text") {
                    ensure!(compression == "gzip", "unsupported packed compression");
                    ensure!(
                        matches!(&a[1], Node::Bin(_)),
                        "compressed packed entries must be bin"
                    );
                    mode = "compressed_packed";
                    let mut reader = flate2::bufread::MultiGzDecoder::new(bytes.as_slice());
                    let mut out = Vec::new();
                    reader
                        .by_ref()
                        .take((MAX_FRAME_BYTES + 1) as u64)
                        .read_to_end(&mut out)?;
                    ensure!(
                        out.len() <= MAX_FRAME_BYTES && reader.get_ref().is_empty(),
                        "packed decompression byte limit/trailing data"
                    );
                    bytes = out;
                }
                let mut out = Vec::new();
                let mut at = 0;
                let mut budget = MAX_VALUES;
                while at < bytes.len() {
                    ensure!(out.len() < MAX_RECORDS, "record count limit");
                    let node = parse(&bytes, &mut at, 0, &mut budget).map_err(|e| match e {
                        ParseError::Short => anyhow::anyhow!("incomplete packed entry"),
                        ParseError::Bad(message) => anyhow::anyhow!(message),
                    })?;
                    out.push(entry(node)?);
                }
                out
            }
            _ => bail!("invalid Forward carrier"),
        }
    };
    ensure!(
        !entries.is_empty() && entries.len() <= MAX_RECORDS,
        "batch must have 1..256 entries"
    );
    if let Some(size) = options.get("size") {
        ensure!(
            size.as_u64() == Some(entries.len() as u64),
            "size option disagrees with entries"
        );
    }
    if single || mode == "forward" {
        ensure!(
            !options.contains_key("compressed"),
            "compression requires PackedForward"
        );
    }
    // Validate the record JSON bound across all entries and modes.
    let mut budget = MAX_VALUES;
    for e in &entries {
        jsonnode(&Value::Object(e.record.clone()), 0, &mut budget)?;
    }
    Ok(Some(Batch {
        tag,
        entries: std::mem::take(&mut entries),
        mode: mode.into(),
        require_ack: chunk.is_some(),
        chunk,
    }))
}
pub fn encode_batch(b: &Batch, chunk: Option<&str>) -> Result<Vec<u8>> {
    ensure!(!b.tag.is_empty() && b.tag.len() <= 1024, "tag byte limit");
    ensure!(
        !b.entries.is_empty() && b.entries.len() <= MAX_RECORDS,
        "batch must contain 1..256 entries"
    );
    let mut budget = MAX_VALUES;
    let entries = b
        .entries
        .iter()
        .map(|e| {
            Ok(Node::Array(vec![
                timestampnode(&e.timestamp)?,
                jsonnode(&Value::Object(e.record.clone()), 0, &mut budget)?,
            ]))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut options = vec![(strnode("size"), Node::Uint(entries.len() as u64))];
    if let Some(chunk) = chunk {
        ensure!(
            !chunk.is_empty() && chunk.len() <= 256 && chunk.bytes().all(|b| b.is_ascii_graphic()),
            "chunk token byte limit"
        );
        options.push((strnode("chunk"), strnode(chunk)));
    }
    let mut root = vec![strnode(&b.tag)];
    match b.mode.as_str() {
        "message" => {
            ensure!(entries.len() == 1, "message mode requires one entry");
            let Node::Array(a) = &entries[0] else {
                unreachable!()
            };
            root.extend(a.clone());
        }
        "forward" => root.push(Node::Array(entries)),
        "packed" | "compressed_packed" => {
            let mut bytes = Vec::new();
            for e in entries {
                emit(&e, &mut bytes)?;
            }
            if b.mode == "compressed_packed" {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(&bytes)?;
                bytes = encoder.finish()?;
                options.push((strnode("compressed"), strnode("gzip")));
            }
            root.push(Node::Bin(bytes));
        }
        _ => bail!("mode must be message, forward, packed or compressed_packed"),
    }
    root.push(Node::Map(options));
    let bytes = encode_node(&Node::Array(root))?;
    let (node, _) = parse_one(&bytes)?.context("incomplete encoded frame")?;
    // Packed records sit inside an opaque bin on the outer frame. Validate the
    // inner stream too, so depth/value limits reject before any bytes are sent.
    parse_batch(node)?.context("encoded batch unexpectedly empty")?;
    Ok(bytes)
}
pub fn encode_ack(chunk: &str) -> Result<Vec<u8>> {
    encode_node(&Node::Map(vec![(strnode("ack"), strnode(chunk))]))
}
pub fn parse_ack(n: Node) -> Result<String> {
    let value = n.json()?;
    let map = value.as_object().context("ACK must be map")?;
    ensure!(map.len() == 1, "unexpected ACK fields");
    Ok(map
        .get("ack")
        .and_then(Value::as_str)
        .context("ACK string missing")?
        .to_owned())
}
