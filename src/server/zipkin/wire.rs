//! Zipkin v2 JSON spans: validation and normalisation shared by the collector and the
//! reporter, plus the query API's endpoint table and the shape each endpoint answers with.
//!
//! The rules follow what the official Zipkin server accepts, measured against
//! zipkin-server 3.5.1: ids are lower-case hex without a prefix, left-padded to 16 (or, for a
//! trace id longer than 16, to 32) characters, never all zero; `kind` is one of four names;
//! unknown keys are ignored; a scalar tag value is kept as its text; an unparseable endpoint
//! address and a non-positive timestamp or duration are dropped rather than refused.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::io::Read;

/// Largest request or response body, after decompression as well as before.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Most spans in one report or one query answer.
pub const MAX_SPANS: usize = 1000;
/// Most annotations, and most tags, on one span.
pub const MAX_ANNOTATIONS: usize = 256;
pub const MAX_TAGS: usize = 256;
/// Longest name, service name, tag key or value, or annotation value.
pub const MAX_TEXT: usize = 64 * 1024;

pub const KINDS: [&str; 4] = ["CLIENT", "SERVER", "PRODUCER", "CONSUMER"];

fn hex_id(value: &Value, field: &str, max: usize) -> Result<String> {
    let s = value
        .as_str()
        .with_context(|| format!("{field} must be a string"))?;
    ensure!(
        !s.is_empty() && s.len() <= max,
        "{field} must be 1..={max} hex characters"
    );
    ensure!(
        s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{field} should be lower-hex encoded with no prefix"
    );
    ensure!(s.bytes().any(|b| b != b'0'), "{field} must not be zero");
    let width = if s.len() > 16 { 32 } else { 16 };
    Ok(format!("{s:0>width$}"))
}

/// A trace id as Zipkin stores it: 16 or 32 lower-case hex characters.
pub fn trace_id(value: &Value) -> Result<String> {
    hex_id(value, "traceId", 32)
}

fn text(value: &Value, field: &str) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => {
            ensure!(s.len() <= MAX_TEXT, "{field} exceeds {MAX_TEXT} bytes");
            Ok((!s.is_empty()).then(|| s.clone()))
        }
        _ => bail!("{field} must be a string"),
    }
}

fn micros(value: &Value) -> Option<u64> {
    value.as_u64().filter(|n| *n > 0)
}

fn endpoint(value: &Value, field: &str) -> Result<Option<Value>> {
    let Some(map) = value.as_object() else {
        ensure!(value.is_null(), "{field} must be an object");
        return Ok(None);
    };
    let mut out = Map::new();
    if let Some(name) = text(
        map.get("serviceName").unwrap_or(&Value::Null),
        "serviceName",
    )? {
        out.insert("serviceName".into(), json!(name));
    }
    if let Some(ip) = map
        .get("ipv4")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
    {
        out.insert("ipv4".into(), json!(ip.to_string()));
    }
    if let Some(ip) = map
        .get("ipv6")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<std::net::Ipv6Addr>().ok())
    {
        out.insert("ipv6".into(), json!(ip.to_string()));
    }
    if let Some(port) = map
        .get("port")
        .and_then(Value::as_u64)
        .filter(|p| (1..=65535).contains(p))
    {
        out.insert("port".into(), json!(port));
    }
    Ok((!out.is_empty()).then_some(Value::Object(out)))
}

/// Validate one v2 span and return it in canonical form, keys in Zipkin's own order.
pub fn span(value: &Value) -> Result<Value> {
    let map = value.as_object().context("a span must be a JSON object")?;
    let mut out = Map::new();
    out.insert(
        "traceId".into(),
        json!(trace_id(map.get("traceId").context("traceId required")?)?),
    );
    if let Some(parent) = map.get("parentId").filter(|v| !v.is_null()) {
        out.insert("parentId".into(), json!(hex_id(parent, "parentId", 16)?));
    }
    out.insert(
        "id".into(),
        json!(hex_id(map.get("id").context("id required")?, "id", 16)?),
    );
    if let Some(kind) = map.get("kind").filter(|v| !v.is_null()) {
        let kind = kind.as_str().context("kind must be a string")?;
        ensure!(
            KINDS.contains(&kind),
            "kind must be CLIENT, SERVER, PRODUCER or CONSUMER"
        );
        out.insert("kind".into(), json!(kind));
    }
    if let Some(name) = text(map.get("name").unwrap_or(&Value::Null), "name")? {
        out.insert("name".into(), json!(name));
    }
    for field in ["timestamp", "duration"] {
        if let Some(n) = map.get(field).and_then(micros) {
            out.insert(field.into(), json!(n));
        }
    }
    for field in ["localEndpoint", "remoteEndpoint"] {
        if let Some(e) = endpoint(map.get(field).unwrap_or(&Value::Null), field)? {
            out.insert(field.into(), e);
        }
    }
    if let Some(list) = map.get("annotations").filter(|v| !v.is_null()) {
        let list = list.as_array().context("annotations must be an array")?;
        ensure!(
            list.len() <= MAX_ANNOTATIONS,
            "more than {MAX_ANNOTATIONS} annotations"
        );
        let mut annotations = Vec::new();
        for a in list {
            let timestamp = a["timestamp"]
                .as_u64()
                .context("annotation timestamp must be a positive integer")?;
            let value = text(&a["value"], "annotation value")?
                .context("annotation value must not be empty")?;
            annotations.push(json!({"timestamp": timestamp, "value": value}));
        }
        if !annotations.is_empty() {
            out.insert("annotations".into(), Value::Array(annotations));
        }
    }
    if let Some(tags) = map.get("tags").filter(|v| !v.is_null()) {
        let tags = tags.as_object().context("tags must be an object")?;
        ensure!(tags.len() <= MAX_TAGS, "more than {MAX_TAGS} tags");
        let mut kept = Map::new();
        for (k, v) in tags {
            ensure!(
                !k.is_empty() && k.len() <= MAX_TEXT,
                "tag keys must be 1..={MAX_TEXT} bytes"
            );
            let v = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => bail!("tag {k} must be a string"),
            };
            ensure!(v.len() <= MAX_TEXT, "tag {k} exceeds {MAX_TEXT} bytes");
            kept.insert(k.clone(), json!(v));
        }
        if !kept.is_empty() {
            out.insert("tags".into(), Value::Object(kept));
        }
    }
    for field in ["debug", "shared"] {
        if map.get(field).and_then(Value::as_bool) == Some(true) {
            out.insert(field.into(), json!(true));
        }
    }
    Ok(Value::Object(out))
}

/// Validate a list of spans: at most `MAX_SPANS`, each canonicalised.
pub fn spans(value: &Value) -> Result<Vec<Value>> {
    let list = value.as_array().context("spans must be a JSON array")?;
    ensure!(list.len() <= MAX_SPANS, "more than {MAX_SPANS} spans");
    list.iter()
        .enumerate()
        .map(|(i, s)| span(s).with_context(|| format!("span {i}")))
        .collect()
}

/// Decode a request body by its `Content-Encoding`, bounded after decompression too.
pub fn decode_body(bytes: &[u8], encoding: &str) -> Result<Vec<u8>> {
    match encoding {
        "identity" => Ok(bytes.to_vec()),
        "gzip" => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(bytes)
                .take(MAX_BODY_BYTES as u64 + 1)
                .read_to_end(&mut out)
                .context("malformed gzip body")?;
            ensure!(
                out.len() <= MAX_BODY_BYTES,
                "decompressed body exceeds byte limit"
            );
            Ok(out)
        }
        _ => bail!("unsupported content encoding"),
    }
}

pub fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

/// What a query endpoint answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `["frontend", "backend"]`
    Names,
    /// `[span, ...]`, one trace
    Trace,
    /// `[[span, ...], ...]`
    Traces,
    /// `[{"parent": "a", "child": "b", "callCount": 1}, ...]`
    Links,
}

/// The read API's endpoints, the query parameters each takes and the shape it answers with.
/// `trace` takes its id in the path, `/api/v2/trace/{traceId}`.
pub const ENDPOINTS: [(&str, &[&str], Shape); 9] = [
    ("services", &[], Shape::Names),
    ("spans", &["serviceName"], Shape::Names),
    ("remoteServices", &["serviceName"], Shape::Names),
    (
        "traces",
        &[
            "serviceName",
            "remoteServiceName",
            "spanName",
            "annotationQuery",
            "minDuration",
            "maxDuration",
            "endTs",
            "lookback",
            "limit",
        ],
        Shape::Traces,
    ),
    ("trace", &[], Shape::Trace),
    ("traceMany", &["traceIds"], Shape::Traces),
    ("dependencies", &["endTs", "lookback"], Shape::Links),
    ("autocompleteKeys", &[], Shape::Names),
    ("autocompleteValues", &["key"], Shape::Names),
];

pub fn endpoint_shape(name: &str) -> Option<(&'static [&'static str], Shape)> {
    ENDPOINTS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, params, shape)| (*params, *shape))
}

/// Check (and canonicalise) a query answer against the shape its endpoint promises.
pub fn result(shape: Shape, value: &Value) -> Result<Value> {
    let list = value
        .as_array()
        .context("a query result must be an array")?;
    match shape {
        Shape::Names => {
            ensure!(list.len() <= MAX_SPANS, "more than {MAX_SPANS} names");
            for n in list {
                let n = n.as_str().context("names must be strings")?;
                ensure!(n.len() <= MAX_TEXT, "name exceeds {MAX_TEXT} bytes");
            }
            Ok(value.clone())
        }
        Shape::Trace => Ok(Value::Array(spans(value)?)),
        Shape::Traces => {
            let total: usize = list.iter().map(|t| t.as_array().map_or(0, Vec::len)).sum();
            ensure!(total <= MAX_SPANS, "more than {MAX_SPANS} spans in all");
            Ok(Value::Array(
                list.iter()
                    .map(|t| spans(t).map(Value::Array))
                    .collect::<Result<_>>()?,
            ))
        }
        Shape::Links => {
            ensure!(list.len() <= MAX_SPANS, "more than {MAX_SPANS} links");
            let mut out = Vec::new();
            for l in list {
                let parent = l["parent"].as_str().context("link parent required")?;
                let child = l["child"].as_str().context("link child required")?;
                let calls = l["callCount"].as_u64().unwrap_or(0);
                let mut link = json!({"parent": parent, "child": child, "callCount": calls});
                if let Some(errors) = l["errorCount"].as_u64().filter(|n| *n > 0) {
                    link["errorCount"] = json!(errors);
                }
                out.push(link);
            }
            Ok(Value::Array(out))
        }
    }
}

/// Decode one `application/x-www-form-urlencoded` component.
pub fn unescape(s: &str) -> Result<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut at = 0;
    while at < b.len() {
        match b[at] {
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b'%' => {
                let pair = b.get(at + 1..at + 3).context("truncated escape")?;
                out.push(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?);
                at += 3;
            }
            c => {
                out.push(c);
                at += 1;
            }
        }
    }
    Ok(String::from_utf8(out)?)
}

pub fn escape(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
