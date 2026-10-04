//! RESTCONF data resource identifiers (RFC 8040 section 3.5.3): `module:node/child=key1,key2`,
//! percent-decoded, with the module carried forward from the first segment.
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value as Json};

pub const MAX_SEGMENTS: usize = 64;
pub const MAX_PATH: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub module: Option<String>,
    pub name: String,
    pub keys: Option<Vec<String>>,
}

fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            ensure!(i + 2 < bytes.len(), "a truncated percent escape");
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])?;
            out.push(
                u8::from_str_radix(hex, 16).map_err(|_| anyhow::anyhow!("a bad percent escape"))?,
            );
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(String::from_utf8(out)?)
}

/// Parse the part of a data URL after `{root}/data/` (empty means the whole datastore).
pub fn parse(path: &str) -> Result<Vec<Segment>> {
    ensure!(
        path.len() <= MAX_PATH,
        "the path is longer than {MAX_PATH} bytes"
    );
    if path.is_empty() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for raw in path.split('/') {
        ensure!(
            out.len() < MAX_SEGMENTS,
            "more than {MAX_SEGMENTS} path segments"
        );
        ensure!(!raw.is_empty(), "an empty path segment");
        let (ident, keys) = match raw.split_once('=') {
            Some((i, k)) => (
                i,
                Some(k.split(',').map(decode).collect::<Result<Vec<_>>>()?),
            ),
            None => (raw, None),
        };
        let (module, name) = match ident.split_once(':') {
            Some((m, n)) => (Some(m.to_owned()), n.to_owned()),
            None => (None, ident.to_owned()),
        };
        if let Some(m) = &module {
            ensure!(identifier(m), "{m:?} is not a module name");
        } else if out.is_empty() {
            bail!("the first path segment names its module, e.g. example:{ident}");
        }
        // `module:` alone addresses the module's top-level nodes (FreeCONF's convention for
        // modules without a top container).
        let module_root =
            name.is_empty() && module.is_some() && keys.is_none() && path.split('/').count() == 1;
        ensure!(
            module_root || identifier(&name),
            "{name:?} is not a node name"
        );
        out.push(Segment { module, name, keys });
    }
    Ok(out)
}

pub fn to_json(segments: &[Segment]) -> Json {
    json!(segments
        .iter()
        .map(|s| {
            let mut v = json!({"name": s.name});
            if let Some(m) = &s.module {
                v["module"] = json!(m);
            }
            if let Some(k) = &s.keys {
                v["keys"] = json!(k);
            }
            v
        })
        .collect::<Vec<_>>())
}

/// The module of the last segment that names one (the target's namespace).
pub fn target_module(segments: &[Segment]) -> Option<&str> {
    segments.iter().rev().find_map(|s| s.module.as_deref())
}

/// Percent-encode a key value for a URL path segment.
pub fn encode_key(k: &str) -> String {
    k.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
