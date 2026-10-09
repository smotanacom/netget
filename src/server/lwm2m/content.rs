//! LwM2M payloads as JSON values: SenML JSON (RFC 8428, content format 110), plain text (0)
//! for one resource, and CoRE link format (40) for registration and discovery.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use serde_json::{json, Map, Value as Json};

pub const CF_TEXT: u16 = 0;
pub const CF_LINK: u16 = 40;
pub const CF_OPAQUE: u16 = 42;
pub const CF_SENML_JSON: u16 = 110;
pub const CF_TLV: u16 = 11542;
pub const MAX_RECORDS: usize = 1024;

/// An LwM2M path: `/object[/instance[/resource[/resource-instance]]]`.
pub fn valid_path(p: &str) -> bool {
    let parts: Vec<&str> = p.strip_prefix('/').unwrap_or("").split('/').collect();
    !p.is_empty()
        && p.starts_with('/')
        && parts.len() <= 4
        && parts.iter().all(|s| {
            !s.is_empty()
                && s.len() <= 5
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.parse::<u32>().is_ok_and(|n| n <= 65535)
        })
}

/// One value: `{path, value}` (string, number or boolean), `{path, opaque: hex}` or
/// `{path, object_link: "obj:inst"}`.
fn record_value(r: &Map<String, Json>) -> Result<(String, Json)> {
    if let Some(v) = r.get("v") {
        return Ok(("value".into(), v.clone()));
    }
    if let Some(v) = r.get("vs") {
        return Ok(("value".into(), v.clone()));
    }
    if let Some(v) = r.get("vb") {
        return Ok(("value".into(), v.clone()));
    }
    if let Some(v) = r.get("vd").and_then(Json::as_str) {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(v.trim_end_matches('='))
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(v))
            .context("vd is not base64")?;
        return Ok(("opaque".into(), json!(hex::encode(bytes))));
    }
    if let Some(v) = r.get("vlo") {
        return Ok(("object_link".into(), v.clone()));
    }
    bail!("a SenML record without a value")
}

pub fn senml_decode(payload: &[u8]) -> Result<Vec<Json>> {
    let records: Vec<Json> = serde_json::from_slice(payload).context("not SenML JSON")?;
    ensure!(
        records.len() <= MAX_RECORDS,
        "more than {MAX_RECORDS} SenML records"
    );
    let mut base = String::new();
    let mut out = Vec::new();
    for r in records {
        let r = r.as_object().context("a SenML record is an object")?;
        if let Some(bn) = r.get("bn").and_then(Json::as_str) {
            base = bn.to_owned();
        }
        let name = format!(
            "{base}{}",
            r.get("n").and_then(Json::as_str).unwrap_or_default()
        );
        let (field, value) = record_value(r)?;
        let mut v = json!({"path": name});
        v[field] = value;
        out.push(v);
    }
    Ok(out)
}

/// Values as SenML JSON, one record per value with its full path as the name.
pub fn senml_encode(values: &[Json]) -> Result<Vec<u8>> {
    ensure!(
        values.len() <= MAX_RECORDS,
        "more than {MAX_RECORDS} values"
    );
    let mut records = Vec::new();
    for v in values {
        let path = v["path"].as_str().context("each value has a path")?;
        ensure!(valid_path(path), "{path:?} is not an LwM2M path");
        let mut r = json!({"n": path});
        if let Some(h) = v.get("opaque").and_then(Json::as_str) {
            r["vd"] = json!(base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(hex::decode(h).context("opaque is hex")?));
        } else if let Some(l) = v.get("object_link") {
            r["vlo"] = l.clone();
        } else {
            match v.get("value") {
                Some(Json::String(s)) => r["vs"] = json!(s),
                Some(Json::Number(n)) => r["v"] = json!(n),
                Some(Json::Bool(b)) => r["vb"] = json!(b),
                _ => bail!(
                    "{path}: value is a string, number or boolean (or give opaque / object_link)"
                ),
            }
        }
        records.push(r);
    }
    Ok(serde_json::to_vec(&records)?)
}

/// A plain-text payload for one resource.
pub fn text_decode(path: &str, payload: &[u8]) -> Vec<Json> {
    let s = String::from_utf8_lossy(payload).into_owned();
    // Without the object model the type is unknown: a number only when it reads back exactly,
    // so "+02" (a UTC offset) stays text and "21.5" becomes 21.5.
    let value = match (s.parse::<i64>(), s.parse::<f64>()) {
        (Ok(n), _) if n.to_string() == s => json!(n),
        (_, Ok(f)) if f.is_finite() && f.to_string() == s => json!(f),
        _ => json!(s),
    };
    vec![json!({"path": path, "value": value})]
}

pub fn text_encode(v: &Json) -> Result<Vec<u8>> {
    Ok(match v.get("value") {
        Some(Json::String(s)) => s.clone().into_bytes(),
        Some(Json::Number(n)) => n.to_string().into_bytes(),
        Some(Json::Bool(b)) => (if *b { "1" } else { "0" }).as_bytes().to_vec(),
        _ => bail!("a plain-text value is a string, number or boolean"),
    })
}

/// Decode a response or request payload by its content format.
pub fn decode(format: Option<u32>, path: &str, payload: &[u8]) -> Result<Vec<Json>> {
    match format.map(|f| f as u16) {
        _ if payload.is_empty() => Ok(vec![]),
        Some(CF_SENML_JSON) => senml_decode(payload),
        Some(CF_TEXT) | None => Ok(text_decode(path, payload)),
        Some(CF_OPAQUE) => Ok(vec![json!({"path": path, "opaque": hex::encode(payload)})]),
        Some(other) => bail!(
            "content format {other} is not supported (SenML JSON 110, text 0 and opaque 42 are)"
        ),
    }
}

/// `</1/0>,</3/0>;ver=1.1` → `[{path, attributes}]`.
pub fn links_decode(payload: &[u8]) -> Result<Vec<Json>> {
    let text = std::str::from_utf8(payload).context("link format is not UTF-8")?;
    let mut out = Vec::new();
    for link in text.split(',').map(str::trim).filter(|l| !l.is_empty()) {
        ensure!(out.len() < MAX_RECORDS, "more than {MAX_RECORDS} links");
        let mut parts = link.split(';');
        let target = parts.next().unwrap_or_default();
        let path = target
            .strip_prefix('<')
            .and_then(|t| t.strip_suffix('>'))
            .context("a link target is <path>")?;
        let mut attrs = Map::new();
        for a in parts {
            let (k, v) = a.split_once('=').unwrap_or((a, ""));
            attrs.insert(k.trim().to_owned(), json!(v.trim().trim_matches('"')));
        }
        out.push(json!({"path": path, "attributes": attrs}));
    }
    Ok(out)
}

pub fn links_encode(paths: &[String]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| format!("<{p}>"))
        .collect::<Vec<_>>()
        .join(",")
        .into_bytes()
}
