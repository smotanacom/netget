//! DICOM datasets (PS3.5): Implicit and Explicit VR Little Endian element encoding, sequences
//! with defined and undefined lengths, and conversion to and from the DICOM JSON model
//! (PS3.18 F) the handler works with. Bulk binary values are never handed to the handler as
//! bytes: they appear as `{"vr": "OB", "length": n}`; values a client sends may be `hex`.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

pub const IMPLICIT_LE: &str = "1.2.840.10008.1.2";
pub const EXPLICIT_LE: &str = "1.2.840.10008.1.2.1";
pub const MAX_DATASET: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_ELEMENTS: usize = 50_000;
const MAX_HEX: usize = 64 * 1024;

/// VRs whose values are text.
pub fn is_string(vr: &str) -> bool {
    matches!(
        vr,
        "AE" | "AS"
            | "CS"
            | "DA"
            | "DS"
            | "DT"
            | "IS"
            | "LO"
            | "LT"
            | "PN"
            | "SH"
            | "ST"
            | "TM"
            | "UI"
            | "UT"
            | "UC"
            | "UR"
    )
}

fn is_binary(vr: &str) -> bool {
    matches!(vr, "OB" | "OW" | "OF" | "OD" | "OL" | "OV" | "UN")
}

/// Explicit VR elements with a 4-byte length (PS3.5 7.1.2).
fn long_length(vr: &str) -> bool {
    matches!(
        vr,
        "OB" | "OW" | "OF" | "OD" | "OL" | "OV" | "SQ" | "UN" | "UC" | "UR" | "UT" | "SV" | "UV"
    )
}

/// The VR of tags the services here use, for Implicit VR datasets; anything else decodes as UN.
pub fn dictionary_vr(tag: u32) -> &'static str {
    if tag & 0xFFFF == 0 {
        return "UL";
    }
    match tag {
        0x0000_0002 | 0x0000_0003 | 0x0000_1000 | 0x0000_1001 => "UI",
        0x0000_0100
        | 0x0000_0110
        | 0x0000_0120
        | 0x0000_0700
        | 0x0000_0800
        | 0x0000_0900
        | 0x0000_1020..=0x0000_1023
        | 0x0000_1031 => "US",
        0x0000_0600 | 0x0000_1030 => "AE",
        0x0000_0901 => "AT",
        0x0000_0902 => "LO",
        0x0008_0005 | 0x0008_0052 | 0x0008_0060 | 0x0008_0061 | 0x0010_0040 | 0x0018_0015
        | 0x0028_0004 | 0x0008_0008 => "CS",
        0x0008_0016 | 0x0008_0018 | 0x0008_1150 | 0x0008_1155 | 0x0020_000D | 0x0020_000E
        | 0x0008_0062 | 0x0020_0052 => "UI",
        0x0008_0020 | 0x0008_0021 | 0x0008_0022 | 0x0008_0023 | 0x0010_0030 => "DA",
        0x0008_0030 | 0x0008_0031 | 0x0008_0032 | 0x0008_0033 => "TM",
        0x0008_0050 | 0x0020_0010 => "SH",
        0x0008_0054 => "AE",
        0x0008_0070 | 0x0008_0080 | 0x0008_1030 | 0x0008_103E | 0x0010_0020 | 0x0008_1090 => "LO",
        0x0008_0090 | 0x0010_0010 | 0x0008_1050 => "PN",
        0x0008_1110 | 0x0008_1115 | 0x0008_1140 | 0x0008_1199 => "SQ",
        0x0010_1010 => "AS",
        0x0018_0050 | 0x0028_0030 => "DS",
        0x0020_0011 | 0x0020_0013 | 0x0020_1200 | 0x0020_1202 | 0x0020_1204 | 0x0020_1206
        | 0x0020_1208 | 0x0020_1209 => "IS",
        0x0028_0002 | 0x0028_0010 | 0x0028_0011 | 0x0028_0100 | 0x0028_0101 | 0x0028_0102
        | 0x0028_0103 => "US",
        0x7FE0_0010 => "OW",
        _ => "UN",
    }
}

pub fn tag_key(tag: u32) -> String {
    format!("{tag:08X}")
}

pub fn parse_key(k: &str) -> Result<u32> {
    ensure!(k.len() == 8, "DICOM JSON keys are 8 hex digits");
    Ok(u32::from_str_radix(k, 16)?)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    explicit: bool,
    elements: usize,
}

impl Reader<'_> {
    fn u16(&mut self) -> Result<u16> {
        let v = self
            .b
            .get(self.pos..self.pos + 2)
            .context("truncated dataset")?;
        self.pos += 2;
        Ok(u16::from_le_bytes([v[0], v[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let v = self
            .b
            .get(self.pos..self.pos + 4)
            .context("truncated dataset")?;
        self.pos += 4;
        Ok(u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
    }
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let v = self
            .b
            .get(self.pos..self.pos + n)
            .context("element longer than the dataset")?;
        self.pos += n;
        Ok(v)
    }
    fn tag(&mut self) -> Result<u32> {
        let g = self.u16()? as u32;
        let e = self.u16()? as u32;
        Ok(g << 16 | e)
    }

    /// Elements until `end` (exclusive) or an item delimiter; the flag says which ended it.
    fn dataset(&mut self, end: usize, depth: usize) -> Result<(Map<String, Value>, bool)> {
        ensure!(depth <= MAX_DEPTH, "sequences nest too deeply");
        let mut out = Map::new();
        while self.pos < end {
            let start = self.pos;
            let tag = self.tag()?;
            if tag == 0xFFFE_E00D {
                self.u32()?;
                return Ok((out, true));
            }
            self.elements += 1;
            ensure!(self.elements <= MAX_ELEMENTS, "too many elements");
            let (vr, len) = if self.explicit && tag >> 16 != 0x0000 {
                let vr_b = self.take(2)?;
                let vr = std::str::from_utf8(vr_b)
                    .ok()
                    .filter(|v| v.bytes().all(|b| b.is_ascii_uppercase()))
                    .context("invalid VR")?
                    .to_owned();
                let len = if long_length(&vr) {
                    self.u16()?;
                    self.u32()?
                } else {
                    self.u16()? as u32
                };
                (vr, len)
            } else {
                (dictionary_vr(tag).to_owned(), self.u32()?)
            };
            let value = self.value(tag, &vr, len, depth)?;
            let _ = start;
            out.insert(tag_key(tag), value);
        }
        Ok((out, false))
    }

    fn value(&mut self, tag: u32, vr: &str, len: u32, depth: usize) -> Result<Value> {
        let undefined = len == 0xFFFF_FFFF;
        if vr == "SQ" || (undefined && vr == "UN") {
            let mut items = Vec::new();
            let end = if undefined {
                self.b.len()
            } else {
                self.pos
                    .checked_add(len as usize)
                    .filter(|e| *e <= self.b.len())
                    .context("sequence longer than the dataset")?
            };
            // An undefined length ends only at its Sequence Delimitation Item (PS3.5 7.5.2).
            let mut delimited = !undefined;
            while self.pos < end {
                let t = self.tag()?;
                let ilen = self.u32()?;
                match t {
                    0xFFFE_E0DD if undefined => {
                        delimited = true;
                        break;
                    }
                    0xFFFE_E000 => {
                        let iend = if ilen == 0xFFFF_FFFF {
                            self.b.len()
                        } else {
                            self.pos
                                .checked_add(ilen as usize)
                                .filter(|e| *e <= end)
                                .context("item longer than its sequence")?
                        };
                        let (item, closed) = self.dataset(iend, depth + 1)?;
                        ensure!(
                            closed || ilen != 0xFFFF_FFFF,
                            "item without its delimiter in {}",
                            tag_key(tag)
                        );
                        items.push(Value::Object(item));
                    }
                    _ => bail!("expected an item in sequence {}", tag_key(tag)),
                }
            }
            ensure!(delimited, "sequence {} without its delimiter", tag_key(tag));
            return Ok(json!({"vr": "SQ", "Value": items}));
        }
        if undefined {
            // Encapsulated pixel data: skip fragments up to the sequence delimiter.
            let mut total = 0usize;
            loop {
                let t = self.tag()?;
                let flen = self.u32()? as usize;
                if t == 0xFFFE_E0DD {
                    break;
                }
                ensure!(t == 0xFFFE_E000, "malformed encapsulated value");
                self.take(flen)?;
                total += flen;
            }
            return Ok(json!({"vr": vr, "length": total, "encapsulated": true}));
        }
        let raw = self.take(len as usize)?;
        Ok(decode_value(vr, raw))
    }
}

fn decode_value(vr: &str, raw: &[u8]) -> Value {
    if is_binary(vr) {
        return json!({"vr": vr, "length": raw.len()});
    }
    if is_string(vr) {
        let text = String::from_utf8(raw.to_vec())
            .unwrap_or_else(|_| raw.iter().map(|&b| b as char).collect());
        let text = text.trim_end_matches(['\0', ' ']);
        let parts: Vec<&str> = if matches!(vr, "LT" | "ST" | "UT" | "UR") {
            vec![text]
        } else {
            text.split('\\').collect()
        };
        if text.is_empty() {
            return json!({"vr": vr});
        }
        let values: Vec<Value> = parts
            .iter()
            .map(|p| {
                let p = p.trim();
                match vr {
                    "PN" => json!({"Alphabetic": p}),
                    "IS" => p
                        .parse::<i64>()
                        .map(|n| json!(n))
                        .unwrap_or_else(|_| json!(p)),
                    "DS" => p
                        .parse::<f64>()
                        .map(|n| json!(n))
                        .unwrap_or_else(|_| json!(p)),
                    _ => json!(p),
                }
            })
            .collect();
        return json!({"vr": vr, "Value": values});
    }
    let num = |size: usize, f: &dyn Fn(&[u8]) -> Value| -> Vec<Value> {
        raw.chunks_exact(size).map(f).collect()
    };
    let values = match vr {
        "US" => num(2, &|c| json!(u16::from_le_bytes([c[0], c[1]]))),
        "SS" => num(2, &|c| json!(i16::from_le_bytes([c[0], c[1]]))),
        "UL" => num(4, &|c| json!(u32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
        "SL" => num(4, &|c| json!(i32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
        "FL" => num(4, &|c| json!(f32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
        "FD" => num(8, &|c| {
            json!(f64::from_le_bytes(c.try_into().expect("8 bytes")))
        }),
        "AT" => num(4, &|c| {
            json!(format!(
                "{:04X}{:04X}",
                u16::from_le_bytes([c[0], c[1]]),
                u16::from_le_bytes([c[2], c[3]])
            ))
        }),
        _ => return json!({"vr": vr, "length": raw.len()}),
    };
    if values.is_empty() {
        json!({"vr": vr})
    } else {
        json!({"vr": vr, "Value": values})
    }
}

/// Decode a dataset in the given transfer syntax into DICOM JSON.
pub fn decode(bytes: &[u8], transfer_syntax: &str) -> Result<Map<String, Value>> {
    ensure!(bytes.len() <= MAX_DATASET, "dataset over 16 MiB");
    let explicit = match transfer_syntax {
        IMPLICIT_LE => false,
        EXPLICIT_LE => true,
        other => bail!("unsupported transfer syntax {other}"),
    };
    let mut r = Reader {
        b: bytes,
        pos: 0,
        explicit,
        elements: 0,
    };
    let (m, stray) = r.dataset(bytes.len(), 0)?;
    ensure!(!stray, "item delimiter outside a sequence");
    Ok(m)
}

fn pad(mut v: Vec<u8>, vr: &str) -> Vec<u8> {
    if v.len() % 2 == 1 {
        v.push(if vr == "UI" || vr == "OB" { 0 } else { b' ' });
    }
    v
}

fn encode_value(vr: &str, v: &Value, explicit: bool, depth: usize) -> Result<Vec<u8>> {
    ensure!(depth <= MAX_DEPTH, "sequences nest too deeply");
    let values = v
        .get("Value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if vr == "SQ" {
        let mut out = Vec::new();
        for item in &values {
            let body = encode_map(
                item.as_object().context("a sequence item is an object")?,
                explicit,
                depth + 1,
            )?;
            out.extend_from_slice(&0xFFFEu16.to_le_bytes());
            out.extend_from_slice(&0xE000u16.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend(body);
        }
        return Ok(out);
    }
    if is_binary(vr) {
        let hex_text = v.get("hex").and_then(Value::as_str).unwrap_or("");
        ensure!(
            hex_text.len() <= MAX_HEX * 2,
            "hex values are limited to 64 KiB"
        );
        return Ok(pad(hex::decode(hex_text).context("hex is not valid")?, vr));
    }
    if is_string(vr) {
        let parts: Vec<String> = values
            .iter()
            .map(|x| match (vr, x) {
                ("PN", Value::Object(o)) => o
                    .get("Alphabetic")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                (_, Value::String(s)) => s.clone(),
                (_, Value::Number(n)) => n.to_string(),
                _ => String::new(),
            })
            .collect();
        let text = parts.join("\\");
        ensure!(
            !text
                .chars()
                .any(|c| c.is_control() && !matches!(vr, "LT" | "ST" | "UT")),
            "string values carry no control characters"
        );
        return Ok(pad(text.into_bytes(), vr));
    }
    let mut out = Vec::new();
    for x in &values {
        match vr {
            "US" => out.extend_from_slice(&(x.as_u64().context("US value")? as u16).to_le_bytes()),
            "SS" => out.extend_from_slice(&(x.as_i64().context("SS value")? as i16).to_le_bytes()),
            "UL" => out.extend_from_slice(&(x.as_u64().context("UL value")? as u32).to_le_bytes()),
            "SL" => out.extend_from_slice(&(x.as_i64().context("SL value")? as i32).to_le_bytes()),
            "FL" => out.extend_from_slice(&(x.as_f64().context("FL value")? as f32).to_le_bytes()),
            "FD" => out.extend_from_slice(&x.as_f64().context("FD value")?.to_le_bytes()),
            "AT" => {
                let t = parse_key(x.as_str().context("AT value")?)?;
                out.extend_from_slice(&((t >> 16) as u16).to_le_bytes());
                out.extend_from_slice(&((t & 0xFFFF) as u16).to_le_bytes());
            }
            other => bail!("unsupported VR {other}"),
        }
    }
    Ok(out)
}

fn encode_map(m: &Map<String, Value>, explicit: bool, depth: usize) -> Result<Vec<u8>> {
    let mut tags: Vec<(u32, &Value)> = m
        .iter()
        .map(|(k, v)| Ok((parse_key(k)?, v)))
        .collect::<Result<_>>()?;
    tags.sort_by_key(|(t, _)| *t);
    let mut out = Vec::new();
    for (tag, v) in tags {
        let vr = v
            .get("vr")
            .and_then(Value::as_str)
            .unwrap_or_else(|| dictionary_vr(tag));
        ensure!(
            vr.len() == 2 && vr.bytes().all(|b| b.is_ascii_uppercase()),
            "invalid VR in {}",
            tag_key(tag)
        );
        let body = encode_value(vr, v, explicit, depth)?;
        out.extend_from_slice(&((tag >> 16) as u16).to_le_bytes());
        out.extend_from_slice(&((tag & 0xFFFF) as u16).to_le_bytes());
        if explicit && tag >> 16 != 0 {
            out.extend_from_slice(vr.as_bytes());
            if long_length(vr) {
                out.extend_from_slice(&[0, 0]);
                out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            } else {
                ensure!(
                    body.len() <= 0xFFFF,
                    "value of {} too long for its VR",
                    tag_key(tag)
                );
                out.extend_from_slice(&(body.len() as u16).to_le_bytes());
            }
        } else {
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        }
        out.extend(body);
    }
    Ok(out)
}

/// Encode a DICOM JSON dataset in the given transfer syntax.
pub fn encode(m: &Map<String, Value>, transfer_syntax: &str) -> Result<Vec<u8>> {
    let explicit = match transfer_syntax {
        IMPLICIT_LE => false,
        EXPLICIT_LE => true,
        other => bail!("unsupported transfer syntax {other}"),
    };
    let out = encode_map(m, explicit, 0)?;
    ensure!(out.len() <= MAX_DATASET, "dataset over 16 MiB");
    Ok(out)
}

/// The first value of an element as text (PN alphabetic, numbers formatted).
pub fn text(m: &Map<String, Value>, tag: u32) -> Option<String> {
    let v = m.get(&tag_key(tag))?.get("Value")?.as_array()?.first()?;
    Some(match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("Alphabetic")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        other => other.to_string(),
    })
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let (p, v): (Vec<char>, Vec<char>) = (pattern.chars().collect(), value.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && (p[i] == '?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// PS3.4 C.2.2.2 matching of one key: universal (empty), list of UIDs, range (DA/TM/DT),
/// wildcard (* and ?), or single value. Sequence keys match universally.
fn key_matches(vr: &str, key: &Value, record: Option<&Value>) -> bool {
    let wanted: Vec<String> = key
        .get("Value")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("Alphabetic")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    if vr == "SQ" || wanted.is_empty() || wanted.iter().all(|w| w.is_empty() || w == "*") {
        return true;
    }
    let have: Vec<String> = record
        .and_then(|r| r.get("Value"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| match x {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("Alphabetic")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    other => other.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    if have.is_empty() {
        return false;
    }
    if vr == "UI" {
        return have.iter().any(|h| wanted.contains(h));
    }
    let w = &wanted[0];
    if matches!(vr, "DA" | "TM" | "DT") && w.contains('-') {
        let (lo, hi) = w.split_once('-').unwrap_or((w, ""));
        return have
            .iter()
            .any(|h| (lo.is_empty() || h.as_str() >= lo) && (hi.is_empty() || h.as_str() <= hi));
    }
    if w.contains(['*', '?']) {
        return have.iter().any(|h| wildcard(w, h));
    }
    have.iter().any(|h| h == w)
}

/// Whether `record` matches every key of the C-FIND identifier.
pub fn matches(identifier: &Map<String, Value>, record: &Map<String, Value>) -> bool {
    identifier.iter().all(|(k, key)| {
        if k == "00080052" || k == "00080005" {
            return true;
        }
        let vr = key.get("vr").and_then(Value::as_str).unwrap_or("UN");
        key_matches(vr, key, record.get(k))
    })
}

/// The response identifier: every requested key with the record's value (empty when the
/// record lacks it), plus the query level and character set.
pub fn project(identifier: &Map<String, Value>, record: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, key) in identifier {
        let value = match record.get(k) {
            Some(v) if k != "00080052" => v.clone(),
            _ => key.clone(),
        };
        out.insert(k.clone(), value);
    }
    out
}
