//! Wire facts from the public Loki push schema and Snappy block format.
//! No third-party protocol implementation is linked or vendored.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};
pub const DEFAULT_LLM_FALLBACK: bool = false;
pub const DEFAULT_REQUIRE_TENANT: bool = false;
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_STREAMS: usize = 64;
pub const MAX_ENTRIES: usize = 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;
pub const MAX_LABELS: usize = 32;
pub const MAX_METADATA: usize = 64;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_VALUE_BYTES: usize = 2048;
pub const MAX_PROTO_FIELDS: usize = 16384;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Json,
    GzipJson,
    SnappyProtobuf,
}
impl Encoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::GzipJson => "gzip_json",
            Self::SnappyProtobuf => "snappy_protobuf",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub timestamp_ns: i64,
    pub line: String,
    #[serde(default)]
    pub structured_metadata: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Stream {
    pub labels: BTreeMap<String, String>,
    pub entries: Vec<Entry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushBatch {
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub encoding: Encoding,
    pub streams: Vec<Stream>,
}
pub fn validate_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty()
            && token.len() <= 1024
            && token.bytes().all(|b| (0x21..=0x7e).contains(&b)),
        "token printable ASCII byte limit"
    );
    Ok(())
}
pub fn validate_tenant(tenant: &str) -> Result<()> {
    ensure!(
        !tenant.is_empty()
            && tenant.len() <= 150
            && !matches!(tenant, "." | "..")
            && tenant
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!-_.*'()".contains(&b)),
        "single tenant ID character/byte limit"
    );
    Ok(())
}
fn name(name: &str) -> Result<()> {
    let mut b = name.bytes();
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME_BYTES
            && b.next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && b.all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "label/metadata name character/byte limit"
    );
    Ok(())
}
fn labels(map: &BTreeMap<String, String>, metadata: bool) -> Result<()> {
    ensure!(
        map.len() <= if metadata { MAX_METADATA } else { MAX_LABELS },
        "label/metadata count limit"
    );
    if !metadata {
        ensure!(!map.is_empty(), "stream requires labels");
    }
    for (k, v) in map {
        name(k)?;
        ensure!(
            metadata || !k.starts_with("__"),
            "reserved stream label namespace"
        );
        ensure!(
            v.len() <= MAX_VALUE_BYTES,
            "label/metadata value byte limit"
        );
    }
    Ok(())
}
pub fn validate_batch(batch: &PushBatch) -> Result<()> {
    if let Some(tenant) = &batch.tenant_id {
        validate_tenant(tenant)?;
    }
    ensure!(
        !batch.streams.is_empty() && batch.streams.len() <= MAX_STREAMS,
        "stream count limit"
    );
    let mut total = 0;
    let mut proto_fields = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for stream in &batch.streams {
        labels(&stream.labels, false)?;
        proto_fields += 2;
        ensure!(seen.insert(&stream.labels), "duplicate stream label set");
        ensure!(!stream.entries.is_empty(), "stream requires entries");
        total += stream.entries.len();
        ensure!(total <= MAX_ENTRIES, "entry count limit");
        for e in &stream.entries {
            ensure!(e.line.len() <= MAX_LINE_BYTES, "log line byte limit");
            labels(&e.structured_metadata, true)?;
            proto_fields += 5 + 3 * e.structured_metadata.len();
            ensure!(
                batch.encoding != Encoding::SnappyProtobuf || proto_fields <= MAX_PROTO_FIELDS,
                "protobuf field count limit"
            );
        }
    }
    Ok(())
}
pub fn encode_batch(batch: &PushBatch) -> Result<Vec<u8>> {
    validate_batch(batch)?;
    let bytes = match batch.encoding {
        Encoding::SnappyProtobuf => encode_proto(&batch.streams),
        _ => serde_json::to_vec(
            &json!({"streams":batch.streams.iter().map(|s|json!({"stream":s.labels,"values":s.entries.iter().map(|e|if e.structured_metadata.is_empty(){json!([e.timestamp_ns.to_string(),e.line])}else{json!([e.timestamp_ns.to_string(),e.line,e.structured_metadata])}).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
        )?,
    };
    ensure!(
        bytes.len() <= MAX_BODY_BYTES,
        "uncompressed body byte limit"
    );
    let bytes = match batch.encoding {
        Encoding::Json => bytes,
        Encoding::SnappyProtobuf => snappy_encode(&bytes),
        Encoding::GzipJson => {
            let mut gzip =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            gzip.write_all(&bytes)?;
            gzip.finish()?
        }
    };
    ensure!(bytes.len() <= MAX_BODY_BYTES, "wire body byte limit");
    Ok(bytes)
}
pub fn decode_batch(bytes: &[u8], encoding: Encoding) -> Result<Vec<Stream>> {
    ensure!(bytes.len() <= MAX_BODY_BYTES, "wire body byte limit");
    let decoded = match encoding {
        Encoding::Json => bytes.to_vec(),
        Encoding::SnappyProtobuf => snappy_decode(bytes)?,
        Encoding::GzipJson => {
            let mut decoder = flate2::read::MultiGzDecoder::new(bytes);
            let mut out = Vec::new();
            decoder
                .by_ref()
                .take((MAX_BODY_BYTES + 1) as u64)
                .read_to_end(&mut out)
                .context("invalid gzip")?;
            ensure!(out.len() <= MAX_BODY_BYTES, "gzip decompressed byte limit");
            out
        }
    };
    let streams = match encoding {
        Encoding::SnappyProtobuf => decode_proto(&decoded)?,
        _ => decode_json(&decoded)?,
    };
    validate_batch(&PushBatch {
        tenant_id: None,
        encoding,
        streams: streams.clone(),
    })?;
    Ok(streams)
}
// Reject duplicate JSON keys before converting the public wire shape. Parsing depth is
// separately bounded so an ignored/unknown field cannot smuggle a nesting bomb.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON value with unique object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = a.next_element::<StrictValue>()? {
                    out.push(v.0);
                }
                Ok(StrictValue(Value::Array(out)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if out.contains_key(&k) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    out.insert(k, a.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(out)))
            }
        }
        d.deserialize_any(V)
    }
}
fn json_depth(bytes: &[u8]) -> Result<()> {
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for &b in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                quoted = false;
            }
        } else {
            match b {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    ensure!(depth <= 8, "JSON nesting depth limit");
                }
                b']' | b'}' => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
    }
    Ok(())
}
fn decode_json(bytes: &[u8]) -> Result<Vec<Stream>> {
    json_depth(bytes)?;
    let value = serde_json::from_slice::<StrictValue>(bytes)?.0;
    let mut root = value
        .as_object()
        .context("push JSON object required")?
        .clone();
    ensure!(root.len() == 1, "only streams push field supported");
    let streams = root.remove("streams").context("streams required")?;
    let wire = streams.as_array().context("streams array required")?;
    ensure!(wire.len() <= MAX_STREAMS, "stream count limit");
    let mut out = Vec::new();
    let mut count = 0;
    for stream in wire {
        let o = stream.as_object().context("stream object required")?;
        ensure!(o.len() == 2, "stream and values required only");
        let map: BTreeMap<String, String> =
            serde_json::from_value(o.get("stream").context("stream labels required")?.clone())?;
        let values = o
            .get("values")
            .and_then(Value::as_array)
            .context("values array required")?;
        count += values.len();
        ensure!(count <= MAX_ENTRIES, "entry count limit");
        let mut entries = Vec::new();
        for value in values {
            let arr = value.as_array().context("log value array required")?;
            ensure!(
                matches!(arr.len(), 2 | 3),
                "log value requires timestamp string,line and optional metadata"
            );
            let text = arr[0]
                .as_str()
                .context("timestamp must be decimal nanoseconds STRING")?;
            ensure!(
                !text.is_empty()
                    && text.len() <= 20
                    && text
                        .bytes()
                        .enumerate()
                        .all(|(i, b)| b.is_ascii_digit() || (i == 0 && b == b'-')),
                "decimal nanoseconds timestamp required"
            );
            let timestamp_ns = text
                .parse::<i64>()
                .context("timestamp signed nanoseconds range")?;
            let line = arr[1].as_str().context("line must be string")?.to_owned();
            let structured_metadata = if arr.len() == 3 {
                serde_json::from_value(arr[2].clone())?
            } else {
                BTreeMap::new()
            };
            entries.push(Entry {
                timestamp_ns,
                line,
                structured_metadata,
            });
        }
        out.push(Stream {
            labels: map,
            entries,
        });
    }
    Ok(out)
}
fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn message(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    varint(out, (field << 3) | 2);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}
fn integer(out: &mut Vec<u8>, field: u64, n: u64) {
    varint(out, field << 3);
    varint(out, n);
}
fn label_text(labels: &BTreeMap<String, String>) -> String {
    format!(
        "{{{}}}",
        labels
            .iter()
            .map(|(k, v)| format!("{k}={}", serde_json::to_string(v).unwrap()))
            .collect::<Vec<_>>()
            .join(",")
    )
}
fn encode_proto(streams: &[Stream]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in streams {
        let mut wire = Vec::new();
        message(&mut wire, 1, label_text(&s.labels).as_bytes());
        for e in &s.entries {
            let mut entry = Vec::new();
            let mut timestamp = Vec::new();
            integer(
                &mut timestamp,
                1,
                e.timestamp_ns.div_euclid(1_000_000_000) as u64,
            );
            integer(
                &mut timestamp,
                2,
                e.timestamp_ns.rem_euclid(1_000_000_000) as u64,
            );
            message(&mut entry, 1, &timestamp);
            message(&mut entry, 2, e.line.as_bytes());
            for (k, v) in &e.structured_metadata {
                let mut pair = Vec::new();
                message(&mut pair, 1, k.as_bytes());
                message(&mut pair, 2, v.as_bytes());
                message(&mut entry, 3, &pair);
            }
            message(&mut wire, 2, &entry);
        }
        message(&mut out, 1, &wire);
    }
    out
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).context("length overflow")?;
        let v = self.bytes.get(self.at..end).context("truncated field")?;
        self.at = end;
        Ok(v)
    }
    fn number(&mut self) -> Result<u64> {
        let mut n = 0;
        for shift in (0..70).step_by(7) {
            let b = self.take(1)?[0];
            ensure!(shift != 63 || b <= 1, "varint overflow");
            n |= u64::from(b & 127) << shift;
            if b & 128 == 0 {
                return Ok(n);
            }
        }
        bail!("varint overflow")
    }
    fn field(&mut self, budget: &mut usize) -> Result<Option<(u64, u8)>> {
        if self.at == self.bytes.len() {
            return Ok(None);
        }
        *budget = budget
            .checked_sub(1)
            .context("protobuf field count limit")?;
        let key = self.number()?;
        ensure!(
            key >> 3 > 0 && key >> 3 <= 0x1fffffff,
            "invalid protobuf field number"
        );
        Ok(Some((key >> 3, (key & 7) as u8)))
    }
    fn blob(&mut self, wire: u8) -> Result<&'a [u8]> {
        ensure!(wire == 2, "expected length-delimited field");
        let n = usize::try_from(self.number()?)?;
        self.take(n)
    }
    fn scalar(&mut self, wire: u8) -> Result<u64> {
        ensure!(wire == 0, "expected integer field");
        self.number()
    }
    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.number()?;
            }
            1 => {
                self.take(8)?;
            }
            2 => {
                self.blob(wire)?;
            }
            5 => {
                self.take(4)?;
            }
            _ => bail!("protobuf groups/invalid wire types unsupported"),
        };
        Ok(())
    }
}
fn text(bytes: &[u8]) -> Result<String> {
    Ok(std::str::from_utf8(bytes)?.to_owned())
}
fn timestamp(bytes: &[u8], budget: &mut usize) -> Result<i64> {
    let mut r = Reader::new(bytes);
    let (mut seconds, mut nanos) = (None, None);
    while let Some((field, wire)) = r.field(budget)? {
        match field {
            1 => {
                ensure!(seconds.is_none(), "duplicate timestamp seconds");
                seconds = Some(r.scalar(wire)? as i64);
            }
            2 => {
                ensure!(nanos.is_none(), "duplicate timestamp nanos");
                nanos = Some(r.scalar(wire)?);
            }
            _ => r.skip(wire)?,
        }
    }
    let nanos = nanos.unwrap_or(0);
    ensure!(nanos < 1_000_000_000, "timestamp nanos range");
    Ok(i64::try_from(
        i128::from(seconds.unwrap_or(0)) * 1_000_000_000 + i128::from(nanos),
    )?)
}
fn pair(bytes: &[u8], budget: &mut usize) -> Result<(String, String)> {
    let mut r = Reader::new(bytes);
    let (mut name, mut value) = (None, None);
    while let Some((field, wire)) = r.field(budget)? {
        match field {
            1 => {
                ensure!(name.is_none(), "duplicate metadata name");
                name = Some(text(r.blob(wire)?)?);
            }
            2 => {
                ensure!(value.is_none(), "duplicate metadata value");
                value = Some(text(r.blob(wire)?)?);
            }
            _ => r.skip(wire)?,
        }
    }
    Ok((
        name.context("metadata name required")?,
        value.unwrap_or_default(),
    ))
}
fn entry(bytes: &[u8], budget: &mut usize) -> Result<Entry> {
    let mut r = Reader::new(bytes);
    let (mut ts, mut line) = (None, None);
    let mut metadata = BTreeMap::new();
    while let Some((field, wire)) = r.field(budget)? {
        match field {
            1 => {
                ensure!(ts.is_none(), "duplicate entry timestamp");
                ts = Some(timestamp(r.blob(wire)?, budget)?);
            }
            2 => {
                ensure!(line.is_none(), "duplicate entry line");
                line = Some(text(r.blob(wire)?)?);
            }
            3 => {
                let (k, v) = pair(r.blob(wire)?, budget)?;
                ensure!(metadata.insert(k, v).is_none(), "duplicate metadata name");
                ensure!(metadata.len() <= MAX_METADATA, "metadata count limit");
            }
            4 => bail!("parsed query metadata is not a push field"),
            _ => r.skip(wire)?,
        }
    }
    Ok(Entry {
        timestamp_ns: ts.context("entry timestamp required")?,
        line: line.unwrap_or_default(),
        structured_metadata: metadata,
    })
}
fn decode_proto(bytes: &[u8]) -> Result<Vec<Stream>> {
    let mut budget = MAX_PROTO_FIELDS;
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    let mut total = 0;
    while let Some((field, wire)) = r.field(&mut budget)? {
        match field {
            1 => {
                ensure!(out.len() < MAX_STREAMS, "stream count limit");
                let mut s = Reader::new(r.blob(wire)?);
                let mut labels = None;
                let mut entries = Vec::new();
                while let Some((field, wire)) = s.field(&mut budget)? {
                    match field {
                        1 => {
                            ensure!(labels.is_none(), "duplicate stream labels");
                            labels = Some(parse_labels(&text(s.blob(wire)?)?)?);
                        }
                        2 => {
                            total += 1;
                            ensure!(total <= MAX_ENTRIES, "entry count limit");
                            entries.push(entry(s.blob(wire)?, &mut budget)?);
                        }
                        3 => {
                            s.scalar(wire)?;
                        }
                        _ => s.skip(wire)?,
                    }
                }
                out.push(Stream {
                    labels: labels.context("stream labels required")?,
                    entries,
                });
            }
            2 => ensure!(
                matches!(text(r.blob(wire)?)?.as_str(), "" | "loki"),
                "only Loki push format supported"
            ),
            _ => r.skip(wire)?,
        }
    }
    Ok(out)
}
// Prometheus label-set text uses Go quoted strings, including octal/hex escapes.
fn parse_labels(s: &str) -> Result<BTreeMap<String, String>> {
    let b = s.as_bytes();
    let mut at = 0;
    let mut out = BTreeMap::new();
    fn ws(b: &[u8], at: &mut usize) {
        while b.get(*at).is_some_and(u8::is_ascii_whitespace) {
            *at += 1;
        }
    }
    ws(b, &mut at);
    ensure!(b.get(at) == Some(&b'{'), "label set requires braces");
    at += 1;
    loop {
        ws(b, &mut at);
        if b.get(at) == Some(&b'}') {
            at += 1;
            break;
        }
        let start = at;
        while b
            .get(at)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
        {
            at += 1;
        }
        let key = std::str::from_utf8(&b[start..at])?.to_owned();
        name(&key)?;
        ws(b, &mut at);
        ensure!(b.get(at) == Some(&b'='), "label equality required");
        at += 1;
        ws(b, &mut at);
        let value = go_string(b, &mut at)?;
        ensure!(out.insert(key, value).is_none(), "duplicate stream label");
        ensure!(out.len() <= MAX_LABELS, "label count limit");
        ws(b, &mut at);
        match b.get(at) {
            Some(b',') => at += 1,
            Some(b'}') => {
                at += 1;
                break;
            }
            _ => bail!("label comma/end required"),
        }
    }
    ws(b, &mut at);
    ensure!(at == b.len(), "trailing label text");
    Ok(out)
}
fn go_string(bytes: &[u8], at: &mut usize) -> Result<String> {
    ensure!(bytes.get(*at) == Some(&b'"'), "quoted label value required");
    *at += 1;
    let mut out = Vec::new();
    loop {
        let c = *bytes.get(*at).context("unterminated label value")?;
        *at += 1;
        match c {
            b'"' => break,
            b'\\' => {
                let e = *bytes.get(*at).context("truncated label escape")?;
                *at += 1;
                match e {
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(10),
                    b'r' => out.push(13),
                    b't' => out.push(9),
                    b'v' => out.push(11),
                    b'\\' | b'"' => out.push(e),
                    b'x' | b'u' | b'U' => {
                        let n = match e {
                            b'x' => 2,
                            b'u' => 4,
                            _ => 8,
                        };
                        let end = at.checked_add(n).context("escape length")?;
                        let raw = std::str::from_utf8(
                            bytes.get(*at..end).context("truncated hex escape")?,
                        )?;
                        let value = u32::from_str_radix(raw, 16)?;
                        *at = end;
                        if e == b'x' {
                            out.push(u8::try_from(value)?);
                        } else {
                            let ch = char::from_u32(value).context("invalid Unicode scalar")?;
                            let mut buf = [0; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                    b'0'..=b'7' => {
                        let end = at.checked_add(2).context("escape length")?;
                        let rest = bytes.get(*at..end).context("truncated octal escape")?;
                        ensure!(
                            rest.iter().all(|b| (b'0'..=b'7').contains(b)),
                            "invalid octal escape"
                        );
                        let n = u32::from(e - b'0') * 64
                            + u32::from(rest[0] - b'0') * 8
                            + u32::from(rest[1] - b'0');
                        out.push(u8::try_from(n)?);
                        *at = end;
                    }
                    _ => bail!("unknown label escape"),
                }
            }
            b'\n' | b'\r' => bail!("unescaped newline in label value"),
            _ => out.push(c),
        }
    }
    Ok(String::from_utf8(out)?)
}
pub fn snappy_encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, bytes.len() as u64);
    if !bytes.is_empty() {
        let n = bytes.len() - 1;
        if n < 60 {
            out.push((n as u8) << 2);
        } else {
            let width = ((usize::BITS - n.leading_zeros()) as usize).div_ceil(8);
            out.push(((59 + width) as u8) << 2);
            out.extend_from_slice(&(n as u32).to_le_bytes()[..width]);
        }
        out.extend_from_slice(bytes);
    }
    out
}
pub fn snappy_decode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader::new(bytes);
    let n = usize::try_from(r.number()?)?;
    ensure!(r.at <= 5, "Snappy length preamble overflow");
    ensure!(n <= MAX_BODY_BYTES, "Snappy decompressed byte limit");
    let mut out = Vec::with_capacity(n);
    while r.at < bytes.len() {
        let tag = r.take(1)?[0];
        let (kind, len, offset) = match tag & 3 {
            0 => {
                let v = usize::from(tag >> 2);
                let len = if v < 60 {
                    v + 1
                } else {
                    let width = v - 59;
                    let mut raw = [0; 4];
                    raw[..width].copy_from_slice(r.take(width)?);
                    usize::try_from(u32::from_le_bytes(raw))?
                        .checked_add(1)
                        .context("literal length overflow")?
                };
                (0, len, 0)
            }
            1 => (
                1,
                usize::from((tag >> 2) & 7) + 4,
                (usize::from(tag & 0xe0) << 3) | usize::from(r.take(1)?[0]),
            ),
            2 => {
                let b = r.take(2)?;
                (
                    2,
                    usize::from(tag >> 2) + 1,
                    usize::from(u16::from_le_bytes([b[0], b[1]])),
                )
            }
            _ => {
                let b = r.take(4)?;
                (
                    3,
                    usize::from(tag >> 2) + 1,
                    usize::try_from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))?,
                )
            }
        };
        ensure!(
            len <= n.saturating_sub(out.len()),
            "Snappy output length mismatch/limit"
        );
        if kind == 0 {
            out.extend_from_slice(r.take(len)?);
        } else {
            ensure!(
                offset > 0 && offset <= out.len(),
                "invalid Snappy backreference"
            );
            for _ in 0..len {
                out.push(out[out.len() - offset]);
            }
        }
    }
    ensure!(out.len() == n, "Snappy truncated output");
    Ok(out)
}
