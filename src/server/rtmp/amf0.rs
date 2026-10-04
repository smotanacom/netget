//! AMF0 (Adobe AMF0 specification) to and from JSON, as RTMP command and data messages use it.
//! Numbers are f64; objects and ECMA arrays become JSON objects (key order kept); strict
//! arrays become arrays; null and undefined become null; dates become {"date": ms}.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

const MAX_DEPTH: usize = 16;
const MAX_ITEMS: usize = 10_000;

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    items: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let v = self
            .b
            .get(self.pos..self.pos + n)
            .context("truncated AMF0")?;
        self.pos += n;
        Ok(v)
    }
    fn u16(&mut self) -> Result<usize> {
        let v = self.take(2)?;
        Ok(u16::from_be_bytes([v[0], v[1]]) as usize)
    }
    fn u32(&mut self) -> Result<usize> {
        let v = self.take(4)?;
        Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as usize)
    }
    fn utf8(&mut self, n: usize) -> Result<String> {
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    /// Properties until the 0x00 0x00 0x09 object end.
    fn properties(&mut self, depth: usize) -> Result<Map<String, Value>> {
        let mut m = Map::new();
        loop {
            let n = self.u16()?;
            if n == 0 {
                ensure!(self.take(1)? == [0x09], "object end marker missing");
                return Ok(m);
            }
            let key = self.utf8(n)?;
            let v = self.value(depth + 1)?;
            m.insert(key, v);
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        ensure!(depth <= MAX_DEPTH, "AMF0 nests too deeply");
        self.items += 1;
        ensure!(self.items <= MAX_ITEMS, "too many AMF0 values");
        let marker = self.take(1)?[0];
        Ok(match marker {
            0x00 => {
                let v = self.take(8)?;
                json!(f64::from_be_bytes(v.try_into()?))
            }
            0x01 => json!(self.take(1)?[0] != 0),
            0x02 => {
                let n = self.u16()?;
                json!(self.utf8(n)?)
            }
            0x03 => Value::Object(self.properties(depth)?),
            0x05 | 0x06 => Value::Null,
            0x08 => {
                self.u32()?;
                Value::Object(self.properties(depth)?)
            }
            0x0A => {
                let n = self.u32()?;
                ensure!(n <= MAX_ITEMS, "strict array too long");
                let mut a = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    a.push(self.value(depth + 1)?);
                }
                Value::Array(a)
            }
            0x0B => {
                let ms = f64::from_be_bytes(self.take(8)?.try_into()?);
                self.take(2)?;
                json!({"date": ms})
            }
            0x0C => {
                let n = self.u32()?;
                json!(self.utf8(n)?)
            }
            0x11 => bail!("AMF3 values are not supported"),
            other => bail!("unsupported AMF0 marker 0x{other:02x}"),
        })
    }
}

/// Every value in `b`, in order.
pub fn decode_all(b: &[u8]) -> Result<Vec<Value>> {
    let mut r = Reader {
        b,
        pos: 0,
        items: 0,
    };
    let mut out = Vec::new();
    while r.pos < b.len() {
        out.push(r.value(0)?);
    }
    Ok(out)
}

fn encode_value(v: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    ensure!(depth <= MAX_DEPTH, "AMF0 nests too deeply");
    match v {
        Value::Null => out.push(0x05),
        Value::Bool(b) => out.extend([0x01, *b as u8]),
        Value::Number(n) => {
            out.push(0x00);
            out.extend(n.as_f64().unwrap_or(0.0).to_be_bytes());
        }
        Value::String(s) if s.len() <= 0xFFFF => {
            out.push(0x02);
            out.extend((s.len() as u16).to_be_bytes());
            out.extend(s.as_bytes());
        }
        Value::String(s) => {
            ensure!(s.len() <= u32::MAX as usize, "string too long");
            out.push(0x0C);
            out.extend((s.len() as u32).to_be_bytes());
            out.extend(s.as_bytes());
        }
        Value::Array(a) => {
            out.push(0x0A);
            out.extend((a.len() as u32).to_be_bytes());
            for x in a {
                encode_value(x, out, depth + 1)?;
            }
        }
        Value::Object(m) => {
            out.push(0x03);
            for (k, x) in m {
                ensure!(k.len() <= 0xFFFF && !k.is_empty(), "object key length");
                out.extend((k.len() as u16).to_be_bytes());
                out.extend(k.as_bytes());
                encode_value(x, out, depth + 1)?;
            }
            out.extend([0x00, 0x00, 0x09]);
        }
    }
    Ok(())
}

/// Encode values in sequence (a command: name, transaction ID, command object, arguments).
pub fn encode(values: &[Value]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for v in values {
        encode_value(v, &mut out, 0)?;
    }
    Ok(out)
}

/// An ECMA array, as onMetaData is conventionally written.
pub fn encode_ecma(m: &Map<String, Value>) -> Result<Vec<u8>> {
    let mut out = vec![0x08];
    out.extend((m.len() as u32).to_be_bytes());
    for (k, x) in m {
        out.extend((k.len() as u16).to_be_bytes());
        out.extend(k.as_bytes());
        encode_value(x, &mut out, 1)?;
    }
    out.extend([0x00, 0x00, 0x09]);
    Ok(out)
}
