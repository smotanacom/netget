//! Typed InfluxDB v2 write line protocol. No time-series storage or schema state.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_LINE_BYTES: usize = 16 * 1024;
pub const MAX_POINTS: usize = 256;
pub const MAX_LINES: usize = 1024;
pub const MAX_TAGS: usize = 64;
pub const MAX_FIELDS: usize = 64;
pub const MAX_NAME_BYTES: usize = 1024;
pub const MIN_TIMESTAMP_NS: i64 = i64::MIN + 2;
pub const MAX_TIMESTAMP_NS: i64 = i64::MAX - 1;
pub const DEFAULT_LLM_FALLBACK: bool = false;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Precision {
    #[default]
    #[serde(rename = "ns")]
    Nanoseconds,
    #[serde(rename = "us")]
    Microseconds,
    #[serde(rename = "ms")]
    Milliseconds,
    #[serde(rename = "s")]
    Seconds,
}
impl Precision {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "ns" => Self::Nanoseconds,
            "us" => Self::Microseconds,
            "ms" => Self::Milliseconds,
            "s" => Self::Seconds,
            _ => bail!("precision must be ns, us, ms or s"),
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nanoseconds => "ns",
            Self::Microseconds => "us",
            Self::Milliseconds => "ms",
            Self::Seconds => "s",
        }
    }
    pub fn to_nanoseconds(self, timestamp: i64) -> Result<i64> {
        let factor = match self {
            Self::Nanoseconds => 1,
            Self::Microseconds => 1000,
            Self::Milliseconds => 1_000_000,
            Self::Seconds => 1_000_000_000,
        };
        let ns = timestamp
            .checked_mul(factor)
            .context("timestamp precision overflow")?;
        ensure!(
            (MIN_TIMESTAMP_NS..=MAX_TIMESTAMP_NS).contains(&ns),
            "timestamp outside InfluxDB v2 range"
        );
        Ok(ns)
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum FieldValue {
    Float(f64),
    Integer(i64),
    Unsigned(u64),
    Boolean(bool),
    String(String),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub measurement: String,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    pub fields: BTreeMap<String, FieldValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteBatch {
    pub org: String,
    pub bucket: String,
    #[serde(default)]
    pub precision: Precision,
    pub points: Vec<Point>,
    #[serde(default)]
    pub gzip: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ObservedPoint {
    pub line: usize,
    #[serde(flatten)]
    pub point: Point,
    pub timestamp_ns: i64,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LineError {
    pub line: usize,
    pub message: String,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParsedBatch {
    pub points: Vec<ObservedPoint>,
    pub errors: Vec<LineError>,
}

pub fn validate_token(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_NAME_BYTES
            && value.bytes().all(|b| b.is_ascii_graphic()),
        "token must be nonempty printable ASCII, <=1024 bytes"
    );
    Ok(())
}
pub fn validate_target(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_NAME_BYTES && !value.chars().any(char::is_control),
        "org/bucket must be nonempty, <=1024 bytes, without controls"
    );
    Ok(())
}
fn name(value: &str, namespace: bool) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_NAME_BYTES
            && !value.bytes().any(|b| b < 32 || b == 127)
            && !value.ends_with('\\'),
        "empty, oversized, control or trailing-backslash name"
    );
    if namespace {
        ensure!(
            !value.starts_with('_'),
            "reserved InfluxDB underscore namespace"
        );
    }
    Ok(())
}
fn validate_point(p: &Point, precision: Precision) -> Result<()> {
    name(&p.measurement, true)?;
    ensure!(
        !p.measurement.starts_with('#'),
        "measurement cannot begin comment marker"
    );
    ensure!(p.tags.len() <= MAX_TAGS, "tag count limit");
    ensure!(
        !p.fields.is_empty() && p.fields.len() <= MAX_FIELDS,
        "point requires 1..64 fields"
    );
    for (key, value) in &p.tags {
        name(key, true)?;
        ensure!(key != "time", "reserved time tag key");
        name(value, false)?;
    }
    for (key, value) in &p.fields {
        name(key, true)?;
        ensure!(key != "time", "reserved time field key");
        match value {
            FieldValue::Float(n) => ensure!(n.is_finite(), "nonfinite float field"),
            FieldValue::String(s) => ensure!(
                s.len() <= MAX_LINE_BYTES && !s.contains(['\r', '\n', '\0']),
                "string field byte/newline/control limit"
            ),
            _ => {}
        }
    }
    if let Some(ts) = p.timestamp {
        precision.to_nanoseconds(ts)?;
    }
    Ok(())
}
fn escaped(value: &str, special: &[u8]) -> String {
    let mut out = String::new();
    for c in value.chars() {
        if c.is_ascii() && special.contains(&(c as u8)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
fn encode_point(p: &Point, precision: Precision) -> Result<String> {
    validate_point(p, precision)?;
    let mut out = escaped(&p.measurement, b", ");
    for (key, value) in &p.tags {
        out.push(',');
        out.push_str(&escaped(key, b",= "));
        out.push('=');
        out.push_str(&escaped(value, b",= "));
    }
    out.push(' ');
    for (index, (key, value)) in p.fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&escaped(key, b",= "));
        out.push('=');
        match value {
            FieldValue::Float(n) => out.push_str(&n.to_string()),
            FieldValue::Integer(n) => {
                out.push_str(&n.to_string());
                out.push('i');
            }
            FieldValue::Unsigned(n) => {
                out.push_str(&n.to_string());
                out.push('u');
            }
            FieldValue::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
            FieldValue::String(s) => {
                out.push('"');
                out.push_str(&escaped(s, b"\\\""));
                out.push('"');
            }
        }
        ensure!(out.len() <= MAX_LINE_BYTES, "encoded line byte limit");
    }
    if let Some(ts) = p.timestamp {
        out.push(' ');
        out.push_str(&ts.to_string());
    }
    ensure!(out.len() <= MAX_LINE_BYTES, "encoded line byte limit");
    Ok(out)
}
pub fn encode_batch(batch: &WriteBatch) -> Result<Vec<u8>> {
    validate_target(&batch.org)?;
    validate_target(&batch.bucket)?;
    ensure!(
        !batch.points.is_empty() && batch.points.len() <= MAX_POINTS,
        "batch requires 1..256 points"
    );
    let mut out = Vec::new();
    for p in &batch.points {
        let line = encode_point(p, batch.precision)?;
        ensure!(
            out.len() + line.len() < MAX_BODY_BYTES,
            "encoded body byte limit"
        );
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    if batch.gzip {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&out)?;
        out = e.finish()?;
        ensure!(out.len() <= MAX_BODY_BYTES, "compressed body byte limit");
    }
    Ok(out)
}
pub fn decode_body(bytes: &[u8], encoding: &str) -> Result<Vec<u8>> {
    ensure!(bytes.len() <= MAX_BODY_BYTES, "body byte limit");
    match encoding {
        "" | "identity" => Ok(bytes.to_vec()),
        "gzip" => {
            let mut d = flate2::bufread::MultiGzDecoder::new(bytes);
            let mut out = Vec::new();
            d.by_ref()
                .take((MAX_BODY_BYTES + 1) as u64)
                .read_to_end(&mut out)
                .context("malformed gzip body")?;
            ensure!(
                out.len() <= MAX_BODY_BYTES && d.get_ref().is_empty(),
                "decompressed body byte limit or trailing bytes"
            );
            Ok(out)
        }
        _ => bail!("unsupported content encoding"),
    }
}
fn token(bytes: &[u8], at: &mut usize, delimiters: &[u8], escapes: &[u8]) -> Result<String> {
    let mut out = Vec::new();
    while *at < bytes.len() {
        let b = bytes[*at];
        if b == b'\\'
            && bytes
                .get(*at + 1)
                .is_some_and(|next| escapes.contains(next))
        {
            out.push(bytes[*at + 1]);
            *at += 2;
        } else if delimiters.contains(&b) {
            break;
        } else {
            out.push(b);
            *at += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}
fn expect(bytes: &[u8], at: &mut usize, b: u8) -> Result<()> {
    ensure!(bytes.get(*at) == Some(&b), "expected '{}'", b as char);
    *at += 1;
    Ok(())
}
fn field(bytes: &[u8], at: &mut usize) -> Result<FieldValue> {
    if bytes.get(*at) == Some(&b'"') {
        *at += 1;
        let mut string = Vec::new();
        while *at < bytes.len() && bytes[*at] != b'"' {
            let b = bytes[*at];
            let replacement = if b == b'\\' {
                bytes.get(*at + 1).and_then(|c| match c {
                    b'\\' => Some(b'\\'),
                    b'"' => Some(b'"'),
                    b'n' => Some(b'\n'),
                    b'r' => Some(b'\r'),
                    b't' => Some(b'\t'),
                    _ => None,
                })
            } else {
                None
            };
            if let Some(c) = replacement {
                string.push(c);
                *at += 2;
            } else {
                string.push(b);
                *at += 1;
            }
        }
        let s = String::from_utf8(string)?;
        expect(bytes, at, b'"')?;
        ensure!(
            *at == bytes.len() || matches!(bytes[*at], b',' | b' '),
            "unexpected characters after string field"
        );
        return Ok(FieldValue::String(s));
    }
    let s = token(bytes, at, b", ", b"")?;
    ensure!(!s.is_empty(), "empty field value");
    Ok(match s.as_str() {
        "t" | "T" | "true" | "True" | "TRUE" => FieldValue::Boolean(true),
        "f" | "F" | "false" | "False" | "FALSE" => FieldValue::Boolean(false),
        _ if s.ends_with('i') => {
            FieldValue::Integer(s[..s.len() - 1].parse().context("invalid int64 field")?)
        }
        _ if s.ends_with('u') => {
            FieldValue::Unsigned(s[..s.len() - 1].parse().context("invalid uint64 field")?)
        }
        _ => {
            let n: f64 = s.parse().context("invalid float field")?;
            ensure!(n.is_finite(), "nonfinite float field");
            FieldValue::Float(n)
        }
    })
}
fn parse_point(line: &str, precision: Precision) -> Result<Point> {
    ensure!(line.len() <= MAX_LINE_BYTES, "line byte limit");
    let bytes = line.as_bytes();
    let mut at = 0;
    let measurement = token(bytes, &mut at, b", ", b", ")?;
    let mut tags = BTreeMap::new();
    while bytes.get(at) == Some(&b',') {
        at += 1;
        ensure!(tags.len() < MAX_TAGS, "tag count limit");
        let key = token(bytes, &mut at, b"=, ", b",= ")?;
        expect(bytes, &mut at, b'=')?;
        let value = token(bytes, &mut at, b",= ", b",= ")?;
        ensure!(tags.insert(key, value).is_none(), "duplicate tag key");
    }
    expect(bytes, &mut at, b' ')?;
    let mut fields = BTreeMap::new();
    loop {
        ensure!(fields.len() < MAX_FIELDS, "field count limit");
        let key = token(bytes, &mut at, b"=, ", b",= ")?;
        expect(bytes, &mut at, b'=')?;
        let value = field(bytes, &mut at)?;
        ensure!(fields.insert(key, value).is_none(), "duplicate field key");
        if bytes.get(at) != Some(&b',') {
            break;
        }
        at += 1;
    }
    let timestamp = if at == bytes.len() {
        None
    } else {
        expect(bytes, &mut at, b' ')?;
        let ts = &line[at..];
        ensure!(
            !ts.is_empty()
                && ts
                    .strip_prefix('-')
                    .unwrap_or(ts)
                    .bytes()
                    .all(|b| b.is_ascii_digit()),
            "invalid timestamp syntax"
        );
        Some(ts.parse().context("timestamp int64 overflow")?)
    };
    let point = Point {
        measurement,
        tags,
        fields,
        timestamp,
    };
    validate_point(&point, precision)?;
    Ok(point)
}
pub fn parse_batch(bytes: &[u8], precision: Precision, received_ns: i64) -> Result<ParsedBatch> {
    ensure!(bytes.len() <= MAX_BODY_BYTES, "body byte limit");
    ensure!(
        (MIN_TIMESTAMP_NS..=MAX_TIMESTAMP_NS).contains(&received_ns),
        "receiver timestamp outside range"
    );
    let body = std::str::from_utf8(bytes).context("line protocol must be UTF-8")?;
    let mut out = ParsedBatch {
        points: Vec::new(),
        errors: Vec::new(),
    };
    for (index, line) in body.lines().enumerate() {
        ensure!(index < MAX_LINES, "line count limit");
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        ensure!(
            out.points.len() + out.errors.len() < MAX_POINTS,
            "point count limit"
        );
        match parse_point(line, precision) {
            Ok(point) => {
                let timestamp_ns = point
                    .timestamp
                    .map(|ts| precision.to_nanoseconds(ts))
                    .transpose()?
                    .unwrap_or(received_ns);
                out.points.push(ObservedPoint {
                    line: index + 1,
                    point,
                    timestamp_ns,
                });
            }
            Err(error) => out.errors.push(LineError {
                line: index + 1,
                message: error.to_string(),
            }),
        }
    }
    Ok(out)
}
