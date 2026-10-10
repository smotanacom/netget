//! The D-Bus wire protocol (D-Bus Specification 0.43), shared by NetGet's server and client:
//! the SASL handshake that precedes messages, and messages themselves — the fixed header, the
//! header fields, and a body marshalled by its type signature. Bodies are converted to and from
//! JSON so the model never sees bytes: a signature says how a JSON value is encoded.
//!
//! NetGet writes little-endian (`l`) and reads both byte orders. Two bounds are enforced before
//! anything is allocated or recursed into: a message is at most [`MAX_MESSAGE_BYTES`], and
//! containers — arrays, structs, dict entries and **variants** — nest at most [`MAX_DEPTH`]
//! deep (half the specification's 64; see the constant). Variants matter: a signature is at most 255 bytes, but a variant carries its own
//! signature inside the data, so without a counter seven bytes per level nest without limit.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt};

/// The most a message may be, header and body. The specification allows 128 MiB.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;
/// Container nesting, variants included. The specification allows 64 (32 arrays + 32
/// structs); NetGet allows 32, because a decoded value becomes JSON inside an event, and the
/// event and handler pipeline bounds JSON at 64 levels: a 64-deep value plus its event's own
/// wrapping does not fit, and was refused there instead of here.
pub const MAX_DEPTH: usize = 32;
/// One SASL line, and how many lines the handshake may take.
pub const MAX_AUTH_LINE: usize = 16 * 1024;
pub const MAX_AUTH_LINES: usize = 32;

pub const METHOD_CALL: u8 = 1;
pub const METHOD_RETURN: u8 = 2;
pub const ERROR: u8 = 3;
pub const SIGNAL: u8 = 4;
pub const NO_REPLY_EXPECTED: u8 = 0x1;

pub const BUS_NAME: &str = "org.freedesktop.DBus";
pub const BUS_PATH: &str = "/org/freedesktop/DBus";

pub fn type_name(kind: u8) -> &'static str {
    match kind {
        METHOD_CALL => "method_call",
        METHOD_RETURN => "method_return",
        ERROR => "error",
        SIGNAL => "signal",
        _ => "unknown",
    }
}

/// One complete type.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Byte,
    Bool,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Double,
    Str,
    Path,
    Sig,
    Fd,
    Var,
    Array(Box<Ty>),
    Dict(Box<Ty>, Box<Ty>),
    Struct(Vec<Ty>),
}

impl Ty {
    fn align(&self) -> usize {
        match self {
            Ty::Byte | Ty::Sig | Ty::Var => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::Bool
            | Ty::I32
            | Ty::U32
            | Ty::Str
            | Ty::Path
            | Ty::Fd
            | Ty::Array(_)
            | Ty::Dict(..) => 4,
            Ty::I64 | Ty::U64 | Ty::Double | Ty::Struct(_) => 8,
        }
    }

    fn is_basic(&self) -> bool {
        !matches!(self, Ty::Var | Ty::Array(_) | Ty::Dict(..) | Ty::Struct(_))
    }

    pub fn signature(&self) -> String {
        match self {
            Ty::Byte => "y".into(),
            Ty::Bool => "b".into(),
            Ty::I16 => "n".into(),
            Ty::U16 => "q".into(),
            Ty::I32 => "i".into(),
            Ty::U32 => "u".into(),
            Ty::I64 => "x".into(),
            Ty::U64 => "t".into(),
            Ty::Double => "d".into(),
            Ty::Str => "s".into(),
            Ty::Path => "o".into(),
            Ty::Sig => "g".into(),
            Ty::Fd => "h".into(),
            Ty::Var => "v".into(),
            Ty::Array(t) => format!("a{}", t.signature()),
            Ty::Dict(k, v) => format!("a{{{}{}}}", k.signature(), v.signature()),
            Ty::Struct(ts) => format!("({})", ts.iter().map(Ty::signature).collect::<String>()),
        }
    }
}

/// Parse a signature into its complete types. At most 255 bytes, 32 nested arrays and 32
/// nested structs, as the specification requires.
pub fn parse_signature(sig: &str) -> Result<Vec<Ty>> {
    ensure!(sig.len() <= 255, "a signature is at most 255 bytes");
    let bytes = sig.as_bytes();
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        out.push(parse_one(bytes, &mut pos, 0, 0)?);
    }
    Ok(out)
}

fn parse_one(b: &[u8], pos: &mut usize, arrays: usize, structs: usize) -> Result<Ty> {
    let c = *b.get(*pos).context("the signature ends inside a type")?;
    *pos += 1;
    Ok(match c {
        b'y' => Ty::Byte,
        b'b' => Ty::Bool,
        b'n' => Ty::I16,
        b'q' => Ty::U16,
        b'i' => Ty::I32,
        b'u' => Ty::U32,
        b'x' => Ty::I64,
        b't' => Ty::U64,
        b'd' => Ty::Double,
        b's' => Ty::Str,
        b'o' => Ty::Path,
        b'g' => Ty::Sig,
        b'h' => Ty::Fd,
        b'v' => Ty::Var,
        b'a' => {
            ensure!(
                arrays < 32,
                "arrays nest more than 32 deep in the signature"
            );
            if b.get(*pos) == Some(&b'{') {
                *pos += 1;
                let k = parse_one(b, pos, arrays + 1, structs)?;
                ensure!(k.is_basic(), "a dict key must be a basic type");
                let v = parse_one(b, pos, arrays + 1, structs)?;
                ensure!(
                    b.get(*pos) == Some(&b'}'),
                    "a dict entry has exactly two types"
                );
                *pos += 1;
                Ty::Dict(Box::new(k), Box::new(v))
            } else {
                Ty::Array(Box::new(parse_one(b, pos, arrays + 1, structs)?))
            }
        }
        b'(' => {
            ensure!(
                structs < 32,
                "structs nest more than 32 deep in the signature"
            );
            let mut fields = Vec::new();
            while b.get(*pos) != Some(&b')') {
                fields.push(parse_one(b, pos, arrays, structs + 1)?);
            }
            *pos += 1;
            ensure!(!fields.is_empty(), "a struct has at least one field");
            Ty::Struct(fields)
        }
        other => bail!("{:?} is not a D-Bus type code", other as char),
    })
}

pub fn valid_object_path(p: &str) -> bool {
    p == "/"
        || (p.starts_with('/')
            && !p.ends_with('/')
            && p[1..].split('/').all(|e| {
                !e.is_empty() && e.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            }))
}

pub fn valid_member(m: &str) -> bool {
    !m.is_empty()
        && m.len() <= 255
        && !m.as_bytes()[0].is_ascii_digit()
        && m.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn valid_interface(i: &str) -> bool {
    i.len() <= 255
        && i.split('.').count() >= 2
        && i.split('.').all(|e| {
            !e.is_empty()
                && !e.as_bytes()[0].is_ascii_digit()
                && e.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

pub fn valid_bus_name(n: &str) -> bool {
    if n.len() > 255 || n.split('.').count() < 2 {
        return false;
    }
    let unique = n.starts_with(':');
    let body = n.strip_prefix(':').unwrap_or(n);
    body.split('.').all(|e| {
        !e.is_empty()
            && (unique || !e.as_bytes()[0].is_ascii_digit())
            && e.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    })
}

struct W {
    buf: Vec<u8>,
}

impl W {
    fn pad(&mut self, a: usize) {
        while !self.buf.len().is_multiple_of(a) {
            self.buf.push(0);
        }
    }

    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend(v.to_le_bytes());
    }

    fn string(&mut self, s: &str) -> Result<()> {
        ensure!(!s.contains('\0'), "a D-Bus string cannot contain NUL");
        self.u32(s.len() as u32);
        self.buf.extend(s.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    fn sig(&mut self, s: &str) -> Result<()> {
        ensure!(s.len() <= 255, "a signature is at most 255 bytes");
        self.buf.push(s.len() as u8);
        self.buf.extend(s.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    fn put(&mut self, ty: &Ty, v: &Value, depth: usize) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "values nest more than {MAX_DEPTH} deep");
        ensure!(
            self.buf.len() <= MAX_MESSAGE_BYTES,
            "the message exceeds {MAX_MESSAGE_BYTES} bytes"
        );
        let int = |lo: i128, hi: i128| -> Result<i128> {
            let n = v
                .as_i64()
                .map(i128::from)
                .or_else(|| v.as_u64().map(i128::from))
                .or_else(|| v.as_str().and_then(|s| s.parse::<i128>().ok()))
                .with_context(|| format!("{v} is not a whole number for {}", ty.signature()))?;
            ensure!(
                (lo..=hi).contains(&n),
                "{n} does not fit D-Bus type {}",
                ty.signature()
            );
            Ok(n)
        };
        let text = || {
            v.as_str()
                .with_context(|| format!("{v} is not a string for {}", ty.signature()))
        };
        match ty {
            Ty::Byte => self.buf.push(int(0, 255)? as u8),
            Ty::Bool => {
                let b = v
                    .as_bool()
                    .or_else(|| v.as_u64().filter(|n| *n <= 1).map(|n| n == 1));
                self.u32(u32::from(
                    b.with_context(|| format!("{v} is not a boolean"))?,
                ));
            }
            Ty::I16 => {
                self.pad(2);
                self.buf
                    .extend((int(i16::MIN.into(), i16::MAX.into())? as i16).to_le_bytes());
            }
            Ty::U16 => {
                self.pad(2);
                self.buf
                    .extend((int(0, u16::MAX.into())? as u16).to_le_bytes());
            }
            Ty::I32 => self.u32(int(i32::MIN.into(), i32::MAX.into())? as i32 as u32),
            Ty::U32 => self.u32(int(0, u32::MAX.into())? as u32),
            Ty::I64 => {
                self.pad(8);
                self.buf
                    .extend((int(i64::MIN.into(), i64::MAX.into())? as i64).to_le_bytes());
            }
            Ty::U64 => {
                self.pad(8);
                self.buf
                    .extend((int(0, u64::MAX.into())? as u64).to_le_bytes());
            }
            Ty::Double => {
                self.pad(8);
                let d = v.as_f64().with_context(|| format!("{v} is not a number"))?;
                self.buf.extend(d.to_le_bytes());
            }
            Ty::Str => self.string(text()?)?,
            Ty::Path => {
                let p = text()?;
                ensure!(valid_object_path(p), "{p:?} is not a D-Bus object path");
                self.string(p)?;
            }
            Ty::Sig => {
                let s = text()?;
                parse_signature(s)?;
                self.sig(s)?;
            }
            Ty::Fd => bail!("NetGet does not pass Unix file descriptors"),
            Ty::Var => {
                let (inner, value) = variant_parts(v)?;
                let tys = parse_signature(&inner)?;
                ensure!(
                    tys.len() == 1,
                    "a variant holds exactly one complete type, not {inner:?}"
                );
                self.sig(&inner)?;
                self.put(&tys[0], &value, depth + 1)?;
            }
            Ty::Array(elem) => {
                let items = v
                    .as_array()
                    .with_context(|| format!("{v} is not an array for {}", ty.signature()))?;
                self.container(elem.align(), |w| {
                    for item in items {
                        w.put(elem, item, depth + 1)?;
                    }
                    Ok(())
                })?;
            }
            Ty::Dict(k, val) => {
                let entries: Vec<(Value, Value)> = match v {
                    Value::Object(map) => map
                        .iter()
                        .map(|(key, x)| (key_value(k, key), x.clone()))
                        .collect(),
                    Value::Array(pairs) => pairs
                        .iter()
                        .map(|p| match p.as_array().map(Vec::as_slice) {
                            Some([a, b]) => Ok((a.clone(), b.clone())),
                            _ => bail!("a dict given as an array holds [key, value] pairs"),
                        })
                        .collect::<Result<_>>()?,
                    _ => bail!("{v} is not an object for {}", ty.signature()),
                };
                self.container(8, |w| {
                    for (key, x) in &entries {
                        w.pad(8);
                        w.put(k, key, depth + 1)?;
                        w.put(val, x, depth + 1)?;
                    }
                    Ok(())
                })?;
            }
            Ty::Struct(fields) => {
                let items = v.as_array().with_context(|| {
                    format!("{v} is not an array for struct {}", ty.signature())
                })?;
                ensure!(
                    items.len() == fields.len(),
                    "struct {} takes {} values, not {}",
                    ty.signature(),
                    fields.len(),
                    items.len()
                );
                self.pad(8);
                for (f, item) in fields.iter().zip(items) {
                    self.put(f, item, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    /// An array: its byte length, padding to the element alignment (not counted), elements.
    fn container(
        &mut self,
        elem_align: usize,
        body: impl FnOnce(&mut W) -> Result<()>,
    ) -> Result<()> {
        self.u32(0);
        let len_at = self.buf.len() - 4;
        self.pad(elem_align);
        let start = self.buf.len();
        body(self)?;
        let len = (self.buf.len() - start) as u32;
        self.buf[len_at..len_at + 4].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }
}

/// A JSON object key as the dict's key type wants it.
fn key_value(k: &Ty, key: &str) -> Value {
    match k {
        Ty::Str | Ty::Path | Ty::Sig => json!(key),
        Ty::Bool => json!(key == "true"),
        Ty::Double => key.parse::<f64>().map_or(json!(key), |d| json!(d)),
        _ => key
            .parse::<i64>()
            .map(|n| json!(n))
            .or_else(|_| key.parse::<u64>().map(|n| json!(n)))
            .unwrap_or_else(|_| json!(key)),
    }
}

/// A variant as the model writes it: `{"signature": "s", "value": ...}`, or a plain JSON
/// value whose signature is inferred (string s, bool b, integer i or x or t, number d, array
/// of strings as, other array av, object a{sv}).
pub fn variant_parts(v: &Value) -> Result<(String, Value)> {
    if let Some(map) = v.as_object() {
        if map.len() == 2 {
            if let (Some(Value::String(s)), Some(inner)) = (map.get("signature"), map.get("value"))
            {
                return Ok((s.clone(), inner.clone()));
            }
        }
    }
    Ok((infer_signature(v)?, v.clone()))
}

pub fn infer_signature(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Null => {
            bail!("null has no D-Bus type; give the variant as {{\"signature\", \"value\"}}")
        }
        Value::Bool(_) => "b".into(),
        Value::Number(n) if n.is_i64() => {
            if i32::try_from(n.as_i64().unwrap_or_default()).is_ok() {
                "i".into()
            } else {
                "x".into()
            }
        }
        Value::Number(n) if n.is_u64() => "t".into(),
        Value::Number(_) => "d".into(),
        Value::String(_) => "s".into(),
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => {
            "as".into()
        }
        Value::Array(_) => "av".into(),
        Value::Object(_) => "a{sv}".into(),
    })
}

/// Marshal values by signature; the signature and the bytes.
pub fn marshal(signature: &str, values: &[Value]) -> Result<Vec<u8>> {
    let tys = parse_signature(signature)?;
    ensure!(
        tys.len() == values.len(),
        "signature {signature:?} has {} values, {} were given",
        tys.len(),
        values.len()
    );
    let mut w = W { buf: Vec::new() };
    for (t, v) in tys.iter().zip(values) {
        w.put(t, v, 0)?;
    }
    ensure!(
        w.buf.len() <= MAX_MESSAGE_BYTES,
        "the body exceeds {MAX_MESSAGE_BYTES} bytes"
    );
    Ok(w.buf)
}

struct R<'a> {
    b: &'a [u8],
    pos: usize,
    big: bool,
}

impl R<'_> {
    fn align(&mut self, a: usize) -> Result<()> {
        let to = self.pos.div_ceil(a) * a;
        ensure!(to <= self.b.len(), "the message ends inside padding");
        self.pos = to;
        Ok(())
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .context("the message ends inside a value")?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32> {
        let big = self.big;
        self.align(4)?;
        let s = self.take(4)?;
        let a = [s[0], s[1], s[2], s[3]];
        Ok(if big {
            u32::from_be_bytes(a)
        } else {
            u32::from_le_bytes(a)
        })
    }

    fn u16(&mut self) -> Result<u16> {
        let big = self.big;
        self.align(2)?;
        let s = self.take(2)?;
        Ok(if big {
            u16::from_be_bytes([s[0], s[1]])
        } else {
            u16::from_le_bytes([s[0], s[1]])
        })
    }

    fn u64(&mut self) -> Result<u64> {
        let big = self.big;
        self.align(8)?;
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(if big {
            u64::from_be_bytes(a)
        } else {
            u64::from_le_bytes(a)
        })
    }

    fn string(&mut self) -> Result<String> {
        let len = self.u32()? as usize;
        let s = self.take(len)?.to_vec();
        ensure!(self.take(1)? == [0], "a string is not NUL-terminated");
        String::from_utf8(s).context("a string is not valid UTF-8")
    }

    fn sig(&mut self) -> Result<String> {
        let len = self.take(1)?[0] as usize;
        let s = self.take(len)?.to_vec();
        ensure!(self.take(1)? == [0], "a signature is not NUL-terminated");
        String::from_utf8(s).context("a signature is not ASCII")
    }

    fn get(&mut self, ty: &Ty, depth: usize) -> Result<Value> {
        ensure!(depth <= MAX_DEPTH, "values nest more than {MAX_DEPTH} deep");
        Ok(match ty {
            Ty::Byte => json!(self.take(1)?[0]),
            Ty::Bool => {
                let n = self.u32()?;
                ensure!(n <= 1, "a boolean is {n}");
                json!(n == 1)
            }
            Ty::I16 => json!(self.u16()? as i16),
            Ty::U16 => json!(self.u16()?),
            Ty::I32 => json!(self.u32()? as i32),
            Ty::U32 | Ty::Fd => json!(self.u32()?),
            Ty::I64 => json!(self.u64()? as i64),
            Ty::U64 => json!(self.u64()?),
            Ty::Double => json!(f64::from_bits(self.u64()?)),
            Ty::Str => json!(self.string()?),
            Ty::Path => {
                let p = self.string()?;
                ensure!(valid_object_path(&p), "{p:?} is not an object path");
                json!(p)
            }
            Ty::Sig => {
                let s = self.sig()?;
                parse_signature(&s)?;
                json!(s)
            }
            Ty::Var => {
                let s = self.sig()?;
                let tys = parse_signature(&s)?;
                ensure!(
                    tys.len() == 1,
                    "a variant holds one complete type, not {s:?}"
                );
                json!({"signature": s, "value": self.get(&tys[0], depth + 1)?})
            }
            Ty::Array(elem) => {
                let end = self.array_end(elem.align())?;
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.get(elem, depth + 1)?);
                }
                ensure!(self.pos == end, "an array's elements overrun its length");
                Value::Array(items)
            }
            Ty::Dict(k, v) => {
                let end = self.array_end(8)?;
                let mut map = Map::new();
                while self.pos < end {
                    self.align(8)?;
                    let key = match self.get(k, depth + 1)? {
                        Value::String(s) => s,
                        other => other.to_string(),
                    };
                    let value = self.get(v, depth + 1)?;
                    map.insert(key, value);
                }
                ensure!(self.pos == end, "a dict's entries overrun its length");
                Value::Object(map)
            }
            Ty::Struct(fields) => {
                self.align(8)?;
                let mut items = Vec::with_capacity(fields.len());
                for f in fields {
                    items.push(self.get(f, depth + 1)?);
                }
                Value::Array(items)
            }
        })
    }

    fn array_end(&mut self, elem_align: usize) -> Result<usize> {
        let len = self.u32()? as usize;
        ensure!(
            len <= 64 << 20,
            "an array of {len} bytes exceeds the specification's 64 MiB"
        );
        self.align(elem_align)?;
        self.pos
            .checked_add(len)
            .filter(|e| *e <= self.b.len())
            .context("an array's length overruns the message")
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Message {
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: String,
    /// The body, one JSON value per complete type of `signature`.
    pub body: Vec<Value>,
}

impl Message {
    pub fn call(path: &str, interface: Option<&str>, member: &str) -> Self {
        Message {
            kind: METHOD_CALL,
            path: Some(path.into()),
            interface: interface.map(Into::into),
            member: Some(member.into()),
            ..Default::default()
        }
    }

    /// A METHOD_RETURN for `call`, sent from `sender` (the name it was addressed to).
    pub fn reply_to(call: &Message, signature: &str, body: Vec<Value>) -> Self {
        Message {
            kind: METHOD_RETURN,
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            sender: call.destination.clone(),
            signature: signature.into(),
            body,
            ..Default::default()
        }
    }

    pub fn error_to(call: &Message, name: &str, message: &str) -> Self {
        Message {
            kind: ERROR,
            reply_serial: Some(call.serial),
            error_name: Some(name.into()),
            destination: call.sender.clone(),
            sender: call.destination.clone(),
            signature: "s".into(),
            body: vec![json!(message)],
            ..Default::default()
        }
    }

    pub fn expects_reply(&self) -> bool {
        self.kind == METHOD_CALL && self.flags & NO_REPLY_EXPECTED == 0
    }

    /// The first string argument, as an ERROR's message is.
    pub fn first_string(&self) -> Option<&str> {
        self.body.first().and_then(Value::as_str)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(self.serial != 0, "a message's serial is never 0");
        let body = marshal(&self.signature, &self.body)?;
        let mut w = W {
            buf: vec![b'l', self.kind, self.flags, 1],
        };
        w.u32(body.len() as u32);
        w.u32(self.serial);
        let field = |w: &mut W, code: u8, sig: &str, v: &Value| -> Result<()> {
            w.pad(8);
            w.buf.push(code);
            w.sig(sig)?;
            w.put(&parse_signature(sig)?[0], v, 1)
        };
        w.container(8, |w| {
            if let Some(p) = &self.path {
                field(w, 1, "o", &json!(p))?;
            }
            if let Some(i) = &self.interface {
                ensure!(valid_interface(i), "{i:?} is not a D-Bus interface name");
                field(w, 2, "s", &json!(i))?;
            }
            if let Some(m) = &self.member {
                ensure!(valid_member(m), "{m:?} is not a D-Bus member name");
                field(w, 3, "s", &json!(m))?;
            }
            if let Some(e) = &self.error_name {
                ensure!(valid_interface(e), "{e:?} is not a D-Bus error name");
                field(w, 4, "s", &json!(e))?;
            }
            if let Some(r) = self.reply_serial {
                field(w, 5, "u", &json!(r))?;
            }
            if let Some(d) = &self.destination {
                ensure!(valid_bus_name(d), "{d:?} is not a D-Bus bus name");
                field(w, 6, "s", &json!(d))?;
            }
            if let Some(s) = &self.sender {
                field(w, 7, "s", &json!(s))?;
            }
            if !self.signature.is_empty() {
                field(w, 8, "g", &json!(self.signature))?;
            }
            Ok(())
        })?;
        w.pad(8);
        w.buf.extend(body);
        ensure!(
            w.buf.len() <= MAX_MESSAGE_BYTES,
            "the message exceeds {MAX_MESSAGE_BYTES} bytes"
        );
        Ok(w.buf)
    }
}

/// Read one message. Its announced size is checked against [`MAX_MESSAGE_BYTES`] from the
/// fixed header, before the rest is read. `Ok(None)` at a clean end of stream.
pub async fn read_message<Rd: AsyncRead + Unpin>(r: &mut Rd) -> Result<Option<Message>> {
    let mut head = [0u8; 16];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let big = match head[0] {
        b'l' => false,
        b'B' => true,
        other => bail!("{other:#04x} is not a D-Bus byte-order mark"),
    };
    ensure!(head[3] == 1, "D-Bus protocol version {} is not 1", head[3]);
    let word = |at: usize| {
        let a = [head[at], head[at + 1], head[at + 2], head[at + 3]];
        (if big {
            u32::from_be_bytes(a)
        } else {
            u32::from_le_bytes(a)
        }) as usize
    };
    let (body_len, fields_len) = (word(4), word(12));
    let total = 16usize.saturating_add(fields_len).saturating_add(7) / 8 * 8;
    let total = total.saturating_add(body_len);
    ensure!(
        total <= MAX_MESSAGE_BYTES,
        "the peer announced a {total}-byte message; the bound is {MAX_MESSAGE_BYTES}"
    );
    let mut buf = head.to_vec();
    buf.resize(total, 0);
    r.read_exact(&mut buf[16..]).await?;
    decode(&buf).map(Some)
}

pub fn decode(buf: &[u8]) -> Result<Message> {
    let big = buf[0] == b'B';
    let mut r = R {
        b: buf,
        pos: 4,
        big,
    };
    let body_len = r.u32()? as usize;
    let serial = r.u32()?;
    ensure!(serial != 0, "a message's serial is never 0");
    let mut m = Message {
        kind: buf[1],
        flags: buf[2],
        serial,
        ..Default::default()
    };
    let fields_end = r.array_end(8)?;
    while r.pos < fields_end {
        r.align(8)?;
        let code = r.take(1)?[0];
        let sig = r.sig()?;
        let tys = parse_signature(&sig)?;
        ensure!(tys.len() == 1, "a header field holds one complete type");
        let v = r.get(&tys[0], 1)?;
        let s = || {
            v.as_str()
                .map(str::to_string)
                .context("a header field has the wrong type")
        };
        match (code, sig.as_str()) {
            (1, "o") => m.path = Some(s()?),
            (2, "s") => m.interface = Some(s()?),
            (3, "s") => m.member = Some(s()?),
            (4, "s") => m.error_name = Some(s()?),
            (5, "u") => m.reply_serial = v.as_u64().map(|n| n as u32),
            (6, "s") => m.destination = Some(s()?),
            (7, "s") => m.sender = Some(s()?),
            (8, "g") => m.signature = s()?,
            (9, "u") => ensure!(
                v.as_u64() == Some(0),
                "NetGet does not accept Unix file descriptors"
            ),
            (1..=9, other) => bail!("header field {code} has type {other:?}"),
            _ => {} // unknown fields are ignored, as the specification requires
        }
    }
    ensure!(
        r.pos == fields_end,
        "the header fields overrun their length"
    );
    r.align(8)?;
    ensure!(
        buf.len() - r.pos == body_len,
        "the body is not the announced {body_len} bytes"
    );
    match m.kind {
        METHOD_CALL => ensure!(
            m.path.is_some() && m.member.is_some(),
            "a method call needs a path and a member"
        ),
        METHOD_RETURN => ensure!(
            m.reply_serial.is_some(),
            "a method return needs a reply serial"
        ),
        ERROR => ensure!(
            m.error_name.is_some() && m.reply_serial.is_some(),
            "an error needs a name and a reply serial"
        ),
        SIGNAL => ensure!(
            m.path.is_some() && m.interface.is_some() && m.member.is_some(),
            "a signal needs a path, an interface and a member"
        ),
        _ => {} // unknown types are ignored by the caller, as the specification requires
    }
    for ty in parse_signature(&m.signature)? {
        m.body.push(r.get(&ty, 0)?);
    }
    ensure!(
        r.pos == buf.len(),
        "the body has bytes its signature does not account for"
    );
    Ok(m)
}

/// One CRLF-terminated SASL line, at most [`MAX_AUTH_LINE`] bytes.
pub async fn read_auth_line<B: AsyncBufRead + Unpin>(r: &mut B) -> Result<String> {
    let mut line = Vec::new();
    loop {
        let chunk = r.fill_buf().await?;
        ensure!(
            !chunk.is_empty(),
            "the peer closed the connection during authentication"
        );
        match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => {
                line.extend(&chunk[..=i]);
                r.consume(i + 1);
                break;
            }
            None => {
                let n = chunk.len();
                line.extend_from_slice(chunk);
                r.consume(n);
            }
        }
        ensure!(
            line.len() <= MAX_AUTH_LINE,
            "an authentication line exceeds {MAX_AUTH_LINE} bytes"
        );
    }
    ensure!(
        line.len() <= MAX_AUTH_LINE,
        "an authentication line exceeds {MAX_AUTH_LINE} bytes"
    );
    let text = String::from_utf8(line).context("an authentication line is not ASCII")?;
    Ok(text.trim_end_matches(['\r', '\n']).to_string())
}

/// SASL's hex encoding of a string (EXTERNAL's identity, ANONYMOUS's trace).
pub fn hex_ascii(s: &str) -> String {
    hex::encode(s.as_bytes())
}

pub fn random_guid() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}
