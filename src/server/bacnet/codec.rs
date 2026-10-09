//! Unsegmented BACnet/IP services. Typed application values, no object storage.
use crate::server::ics_support::number;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
pub const MAX_FRAME: usize = 480;
pub const DEFAULT_DEVICE_ID: u32 = 1234;
pub fn wrap(a: &[u8]) -> Vec<u8> {
    let mut b = vec![0x81, 0x0a, 0, 0, 1, 0];
    b.extend(a);
    let n = b.len() as u16;
    b[2..4].copy_from_slice(&n.to_be_bytes());
    b
}
pub fn unwrap(b: &[u8]) -> Result<&[u8]> {
    ensure!(
        b.len() >= 7
            && b.len() <= MAX_FRAME
            && b[0] == 0x81
            && matches!(b[1], 10 | 11)
            && u16::from_be_bytes([b[2], b[3]]) as usize == b.len(),
        "BVLC framing"
    );
    ensure!(
        b[4] == 1 && matches!(b[5], 0 | 4),
        "only local BACnet/IP NPDU supported"
    );
    Ok(&b[6..])
}
fn uint(n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    b[b.iter().position(|v| *v != 0).unwrap_or(7)..].to_vec()
}
fn tag(n: u8, context: bool, data: &[u8]) -> Result<Vec<u8>> {
    ensure!(n < 15 && data.len() <= 255, "tag bound");
    let mut b = vec![
        (n << 4) | if context { 8 } else { 0 } | if data.len() <= 4 { data.len() as u8 } else { 5 },
    ];
    if data.len() > 4 {
        b.push(data.len() as u8);
    }
    b.extend(data);
    Ok(b)
}
fn context(n: u8, value: u64) -> Result<Vec<u8>> {
    tag(n, true, &uint(value))
}
struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Reader<'a> {
    fn value(&mut self, n: u8, ctx: bool) -> Result<&'a [u8]> {
        ensure!(self.p < self.b.len(), "missing tag");
        let t = self.b[self.p];
        self.p += 1;
        ensure!(
            t >> 4 == n && (t & 8 != 0) == ctx && t & 7 <= 5,
            "unexpected tag"
        );
        let len = if t & 7 == 5 {
            ensure!(self.p < self.b.len(), "missing extended length");
            let l = self.b[self.p] as usize;
            self.p += 1;
            l
        } else {
            (t & 7) as usize
        };
        ensure!(self.p + len <= self.b.len(), "truncated tag");
        let b = &self.b[self.p..self.p + len];
        self.p += len;
        Ok(b)
    }
    fn num(&mut self, n: u8, ctx: bool) -> Result<u64> {
        let b = self.value(n, ctx)?;
        ensure!(!b.is_empty() && b.len() <= 4, "integer width");
        Ok(b.iter().fold(0, |v, b| (v << 8) | *b as u64))
    }
    fn marker(&mut self, b: u8) -> Result<()> {
        ensure!(self.b.get(self.p) == Some(&b), "opening/closing tag");
        self.p += 1;
        Ok(())
    }
}
pub fn encode_value(v: &Value) -> Result<Vec<u8>> {
    match v["value_type"].as_str() {
        Some("null") => Ok(vec![0]),
        Some("boolean") => Ok(vec![
            0x10 | u8::from(v["value"].as_bool().context("boolean")?),
        ]),
        Some("unsigned") => tag(2, false, &uint(number(v, "value", u32::MAX as u64)?)),
        Some("enumerated") => tag(9, false, &uint(number(v, "value", u32::MAX as u64)?)),
        Some("signed") => {
            let n = v["value"].as_i64().context("signed integer")?;
            ensure!(i32::try_from(n).is_ok(), "signed range");
            tag(3, false, &(n as i32).to_be_bytes())
        }
        Some("real") => {
            let n = v["value"].as_f64().context("real")?;
            ensure!(n.is_finite() && (n as f32).is_finite(), "real range");
            tag(4, false, &(n as f32).to_be_bytes())
        }
        Some("string") => {
            let s = v["value"].as_str().context("string")?;
            ensure!(s.len() <= 200, "string bound");
            let mut b = vec![0];
            b.extend(s.as_bytes());
            tag(7, false, &b)
        }
        Some("object_identifier") => {
            let t = number(&v["value"], "object_type", 1023)?;
            let i = number(&v["value"], "instance", 4194303)?;
            tag(12, false, &(((t << 22) | i) as u32).to_be_bytes())
        }
        _ => bail!("unsupported application value"),
    }
}
fn decode_value(r: &mut Reader) -> Result<Value> {
    let t = *r.b.get(r.p).context("missing application tag")?;
    ensure!(t & 8 == 0, "expected application value");
    if t >> 4 == 1 {
        ensure!(t & 7 <= 1, "boolean tag");
        r.p += 1;
        return Ok(json!({"value_type":"boolean","value":t&1!=0}));
    }
    let b = r.value(t >> 4, false)?;
    let n = || -> Result<u32> {
        ensure!(!b.is_empty() && b.len() <= 4, "integer width");
        Ok(b.iter().fold(0, |v, b| (v << 8) | *b as u32))
    };
    let (kind, value) = match t >> 4 {
        0 => {
            ensure!(b.is_empty(), "null length");
            ("null", Value::Null)
        }
        2 => ("unsigned", json!(n()?)),
        9 => ("enumerated", json!(n()?)),
        3 => {
            let mut out = if b.first().is_some_and(|n| n & 128 != 0) {
                [255; 4]
            } else {
                [0; 4]
            };
            ensure!(!b.is_empty() && b.len() <= 4, "signed width");
            out[4 - b.len()..].copy_from_slice(b);
            ("signed", json!(i32::from_be_bytes(out)))
        }
        4 => {
            ensure!(b.len() == 4, "real width");
            let n = f32::from_be_bytes(b.try_into()?);
            ensure!(n.is_finite(), "nonfinite real");
            ("real", json!(n))
        }
        7 => {
            ensure!(b.first() == Some(&0), "UTF8 character set required");
            ("string", json!(std::str::from_utf8(&b[1..])?))
        }
        12 => {
            ensure!(b.len() == 4, "object id width");
            let v = n()?;
            (
                "object_identifier",
                json!({"object_type":v>>22,"instance":v&4194303}),
            )
        }
        _ => bail!("unsupported application tag"),
    };
    Ok(json!({"value_type":kind,"value":value}))
}
fn fields(r: &mut Reader) -> Result<Value> {
    let o = r.num(0, true)? as u32;
    let property = r.num(1, true)?;
    let mut v = json!({"object_type":o>>22,"instance":o&4194303,"property":property});
    if r.b.get(r.p).is_some_and(|v| v >> 4 == 2) {
        v["array_index"] = json!(r.num(2, true)?);
    }
    Ok(v)
}
fn encode_fields(v: &Value) -> Result<Vec<u8>> {
    let t = number(v, "object_type", 1023)?;
    let i = number(v, "instance", 4194303)?;
    let mut b = tag(0, true, &(((t << 22) | i) as u32).to_be_bytes())?;
    b.extend(context(1, number(v, "property", 4194303)?)?);
    if v.get("array_index").is_some() {
        b.extend(context(2, number(v, "array_index", u32::MAX as u64)?)?);
    }
    Ok(b)
}
pub fn receive(a: &[u8], device: u32) -> Result<(Vec<u8>, Option<Value>)> {
    ensure!(!a.is_empty(), "APDU empty");
    if a[0] == 0x10 {
        ensure!(a.len() >= 2, "unconfirmed service");
        if a[1] != 8 {
            return Ok((vec![], None));
        }
        let mut r = Reader { b: &a[2..], p: 0 };
        if !r.b.is_empty() {
            let low = r.num(0, true)?;
            let high = r.num(1, true)?;
            ensure!(r.p == r.b.len() && low <= high, "WhoIs range");
            if !(low..=high).contains(&(device as u64)) {
                return Ok((vec![], None));
            }
        }
        let mut b = vec![0x10, 0];
        b.extend(tag(12, false, &((8u32 << 22) | device).to_be_bytes())?);
        b.extend(tag(2, false, &uint(MAX_FRAME as u64))?);
        b.extend(tag(9, false, &[3])?);
        b.extend(tag(2, false, &[0])?);
        return Ok((wrap(&b), None));
    }
    ensure!(a[0] >> 4 == 0 && a.len() >= 4, "confirmed request framing");
    let invoke = a[2];
    if a[0] & 8 != 0 {
        return Ok((wrap(&[0x71, invoke, 4]), None));
    }
    let service = a[3];
    if !matches!(service, 12 | 15) {
        return Ok((wrap(&[0x60, invoke, 9]), None));
    }
    let mut r = Reader { b: &a[4..], p: 0 };
    let mut v = fields(&mut r)?;
    v["operation"] = json!(if service == 12 { "read" } else { "write" });
    v["invoke_id"] = json!(invoke);
    v["service"] = json!(service);
    if service == 15 {
        r.marker(0x3e)?;
        let value = decode_value(&mut r)?;
        r.marker(0x3f)?;
        v["value_type"] = value["value_type"].clone();
        v["value"] = value["value"].clone();
        if r.p < r.b.len() {
            v["priority"] = json!(r.num(4, true)?);
            ensure!(
                v["priority"]
                    .as_u64()
                    .is_some_and(|n| (1..=16).contains(&n)),
                "priority range"
            );
        }
    }
    ensure!(r.p == r.b.len(), "trailing request data");
    Ok((vec![], Some(v)))
}
pub fn answer(r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
    let invoke = number(r, "invoke_id", 255)? as u8;
    let service = number(r, "service", 255)? as u8;
    let Some(a) = a.filter(|a| a["type"] == "bacnet_reply") else {
        return error(invoke, service, 2, 40);
    };
    if a.get("error_code").is_some() {
        return error(
            invoke,
            service,
            number(a, "error_class", 7)?,
            number(a, "error_code", 65535)?,
        );
    }
    if service == 15 {
        if a["accepted"] == true {
            return Ok(wrap(&[0x20, invoke, service]));
        }
        return error(invoke, service, 2, 40);
    }
    let mut b = vec![0x30, invoke, service];
    b.extend(encode_fields(r)?);
    b.push(0x3e);
    b.extend(encode_value(a)?);
    b.push(0x3f);
    ensure!(b.len() + 6 <= MAX_FRAME, "response bound");
    Ok(wrap(&b))
}
fn error(invoke: u8, service: u8, class: u64, code: u64) -> Result<Vec<u8>> {
    let mut b = vec![0x50, invoke, service];
    b.extend(tag(9, false, &uint(class))?);
    b.extend(tag(9, false, &uint(code))?);
    Ok(wrap(&b))
}
pub fn validate(v: &Value) -> Result<()> {
    match v["type"].as_str() {
        Some("bacnet_discover") => {}
        Some("bacnet_read") => {
            encode_fields(v)?;
        }
        Some("bacnet_write") => {
            encode_fields(v)?;
            encode_value(v)?;
            if v.get("priority").is_some() {
                ensure!(
                    (1..=16).contains(&number(v, "priority", 16)?),
                    "priority range"
                );
            }
        }
        _ => bail!("unknown BACnet action"),
    };
    Ok(())
}
pub fn request(v: &Value, invoke: u8) -> Result<Vec<u8>> {
    validate(v)?;
    if v["type"] == "bacnet_discover" {
        return Ok(wrap(&[0x10, 8]));
    }
    let service = if v["type"] == "bacnet_read" { 12 } else { 15 };
    let mut b = vec![0, 5, invoke, service];
    b.extend(encode_fields(v)?);
    if service == 15 {
        b.push(0x3e);
        b.extend(encode_value(v)?);
        b.push(0x3f);
        if v.get("priority").is_some() {
            b.extend(context(4, number(v, "priority", 16)?)?);
        }
    }
    Ok(wrap(&b))
}
pub fn response(a: &[u8], v: &Value, invoke: u8) -> Result<Value> {
    if v["type"] == "bacnet_discover" {
        ensure!(a.len() >= 2 && a[..2] == [0x10, 0], "expected IAm");
        let mut r = Reader { b: &a[2..], p: 0 };
        let o = r.num(12, false)?;
        ensure!(o >> 22 == 8, "IAm device id");
        let max = r.num(2, false)?;
        let seg = r.num(9, false)?;
        let vendor = r.num(2, false)?;
        ensure!(r.p == r.b.len(), "IAm trailing data");
        return Ok(
            json!({"success":true,"device_id":o&4194303,"max_apdu":max,"segmentation":seg,"vendor_id":vendor}),
        );
    }
    ensure!(a.len() >= 3 && a[1] == invoke, "invoke correlation");
    let service = if v["type"] == "bacnet_read" { 12 } else { 15 };
    match a[0] >> 4 {
        2 => {
            ensure!(
                a.len() == 3 && a[2] == service && service == 15,
                "simple ack mismatch"
            );
            Ok(json!({"success":true}))
        }
        3 => {
            ensure!(
                a[0] & 8 == 0 && a[2] == 12 && service == 12,
                "complex ack mismatch"
            );
            let mut r = Reader { b: &a[3..], p: 0 };
            let f = fields(&mut r)?;
            for k in ["object_type", "instance", "property", "array_index"] {
                ensure!(f[k] == v[k], "property correlation");
            }
            r.marker(0x3e)?;
            let mut out = decode_value(&mut r)?;
            r.marker(0x3f)?;
            ensure!(r.p == r.b.len(), "ack trailing data");
            out["success"] = json!(true);
            Ok(out)
        }
        5 => {
            ensure!(a[2] == service, "error service correlation");
            let mut r = Reader { b: &a[3..], p: 0 };
            let class = r.num(9, false)?;
            let code = r.num(9, false)?;
            ensure!(r.p == r.b.len(), "error trailing data");
            Ok(json!({"success":false,"error_class":class,"error_code":code}))
        }
        6 | 7 => Ok(json!({"success":false,"reason":a[2]})),
        _ => bail!("unexpected APDU"),
    }
}
