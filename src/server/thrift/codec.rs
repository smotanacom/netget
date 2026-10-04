//! Thrift's binary (strict, and the old non-strict form on read) and compact protocols over a
//! byte buffer, to and from a typed value tree. Parsing reports `Incomplete` when the buffer ends
//! early, so the unframed transport can read more.
use anyhow::{anyhow, bail, ensure, Result};

pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;
const MAX_DEPTH: usize = 64;

pub const T_STOP: u8 = 0;
pub const T_BOOL: u8 = 2;
pub const T_BYTE: u8 = 3;
pub const T_DOUBLE: u8 = 4;
pub const T_I16: u8 = 6;
pub const T_I32: u8 = 8;
pub const T_I64: u8 = 10;
pub const T_STRING: u8 = 11;
pub const T_STRUCT: u8 = 12;
pub const T_MAP: u8 = 13;
pub const T_SET: u8 = 14;
pub const T_LIST: u8 = 15;
pub const T_UUID: u8 = 16;

pub const CALL: u8 = 1;
pub const REPLY: u8 = 2;
pub const EXCEPTION: u8 = 3;
pub const ONEWAY: u8 = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum Tv {
    Bool(bool),
    Byte(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    Double(f64),
    Bin(Vec<u8>),
    Uuid([u8; 16]),
    Struct(Vec<(i16, Tv)>),
    List(u8, Vec<Tv>),
    Set(u8, Vec<Tv>),
    Map(u8, u8, Vec<(Tv, Tv)>),
}

impl Tv {
    pub fn ttype(&self) -> u8 {
        match self {
            Tv::Bool(_) => T_BOOL,
            Tv::Byte(_) => T_BYTE,
            Tv::I16(_) => T_I16,
            Tv::I32(_) => T_I32,
            Tv::I64(_) => T_I64,
            Tv::Double(_) => T_DOUBLE,
            Tv::Bin(_) => T_STRING,
            Tv::Uuid(_) => T_UUID,
            Tv::Struct(_) => T_STRUCT,
            Tv::List(..) => T_LIST,
            Tv::Set(..) => T_SET,
            Tv::Map(..) => T_MAP,
        }
    }
    pub fn field(&self, id: i16) -> Option<&Tv> {
        match self {
            Tv::Struct(f) => f.iter().find(|(i, _)| *i == id).map(|(_, v)| v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Protocol {
    Binary,
    Compact,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub name: String,
    pub kind: u8,
    pub seqid: i32,
    pub body: Tv,
}

/// The buffer ended before the message did.
#[derive(Debug)]
pub struct Incomplete;
impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("incomplete message")
    }
}
impl std::error::Error for Incomplete {}

struct R<'a> {
    b: &'a [u8],
    pos: usize,
    items: usize,
}

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        ensure!(n <= MAX_MESSAGE, "length {n} is over the message bound");
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow!("length overflow"))?;
        let v = self
            .b
            .get(self.pos..end)
            .ok_or_else(|| anyhow!(Incomplete))?;
        self.pos = end;
        Ok(v)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into()?))
    }
    fn count(&mut self, n: i64, min_each: usize) -> Result<usize> {
        ensure!(n >= 0, "negative container size");
        let n = n as usize;
        self.items += n;
        ensure!(self.items <= 4_000_000, "too many container elements");
        // Every element takes at least `min_each` bytes, so a size beyond what remains cannot be honest.
        if n.saturating_mul(min_each) > self.b.len() - self.pos {
            if self.b.len() >= MAX_MESSAGE {
                bail!("container size {n} cannot fit in the message");
            }
            return Err(anyhow!(Incomplete));
        }
        Ok(n)
    }
    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.u8()?;
            ensure!(shift < 64, "varint too long");
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        bail!("varint too long")
    }
    fn zigzag(&mut self) -> Result<i64> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    // Binary protocol.
    fn bin_value(&mut self, t: u8, depth: usize) -> Result<Tv> {
        ensure!(depth <= MAX_DEPTH, "values nest too deeply");
        Ok(match t {
            T_BOOL => Tv::Bool(self.u8()? != 0),
            T_BYTE => Tv::Byte(self.u8()? as i8),
            T_I16 => Tv::I16(self.i16()?),
            T_I32 => Tv::I32(self.i32()?),
            T_I64 => Tv::I64(self.i64()?),
            T_DOUBLE => Tv::Double(f64::from_bits(self.i64()? as u64)),
            T_STRING => {
                let n = self.i32()?;
                ensure!(n >= 0, "negative string length");
                Tv::Bin(self.take(n as usize)?.to_vec())
            }
            T_UUID => Tv::Uuid(self.take(16)?.try_into()?),
            T_STRUCT => {
                let mut fields = Vec::new();
                loop {
                    let ft = self.u8()?;
                    if ft == T_STOP {
                        break;
                    }
                    let id = self.i16()?;
                    fields.push((id, self.bin_value(ft, depth + 1)?));
                }
                Tv::Struct(fields)
            }
            T_LIST | T_SET => {
                let et = self.u8()?;
                let n = self.i32()?;
                let n = self.count(n as i64, 1)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.bin_value(et, depth + 1)?);
                }
                if t == T_LIST {
                    Tv::List(et, items)
                } else {
                    Tv::Set(et, items)
                }
            }
            T_MAP => {
                let (kt, vt) = (self.u8()?, self.u8()?);
                let n = self.i32()?;
                let n = self.count(n as i64, 2)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let k = self.bin_value(kt, depth + 1)?;
                    let v = self.bin_value(vt, depth + 1)?;
                    items.push((k, v));
                }
                Tv::Map(kt, vt, items)
            }
            other => bail!("unknown Thrift type {other}"),
        })
    }

    // Compact protocol.
    fn compact_type(t: u8) -> Result<u8> {
        Ok(match t {
            1 | 2 => T_BOOL,
            3 => T_BYTE,
            4 => T_I16,
            5 => T_I32,
            6 => T_I64,
            7 => T_DOUBLE,
            8 => T_STRING,
            9 => T_LIST,
            10 => T_SET,
            11 => T_MAP,
            12 => T_STRUCT,
            13 => T_UUID,
            other => bail!("unknown compact type {other}"),
        })
    }
    fn compact_value(&mut self, t: u8, depth: usize) -> Result<Tv> {
        ensure!(depth <= MAX_DEPTH, "values nest too deeply");
        Ok(match t {
            T_BOOL => Tv::Bool(self.u8()? == 1),
            T_BYTE => Tv::Byte(self.u8()? as i8),
            T_I16 => Tv::I16(i16::try_from(self.zigzag()?)?),
            T_I32 => Tv::I32(i32::try_from(self.zigzag()?)?),
            T_I64 => Tv::I64(self.zigzag()?),
            T_DOUBLE => Tv::Double(f64::from_le_bytes(self.take(8)?.try_into()?)),
            T_STRING => {
                let n = self.varint()?;
                Tv::Bin(self.take(usize::try_from(n)?)?.to_vec())
            }
            T_UUID => Tv::Uuid(self.take(16)?.try_into()?),
            T_STRUCT => {
                let mut fields = Vec::new();
                let mut last: i16 = 0;
                loop {
                    let h = self.u8()?;
                    if h == T_STOP {
                        break;
                    }
                    let delta = (h >> 4) as i16;
                    let id = if delta == 0 {
                        i16::try_from(self.zigzag()?)?
                    } else {
                        last.checked_add(delta)
                            .ok_or_else(|| anyhow!("field id overflow"))?
                    };
                    last = id;
                    let ct = h & 0x0f;
                    let v = if ct == 1 || ct == 2 {
                        Tv::Bool(ct == 1)
                    } else {
                        self.compact_value(Self::compact_type(ct)?, depth + 1)?
                    };
                    fields.push((id, v));
                }
                Tv::Struct(fields)
            }
            T_LIST | T_SET => {
                let h = self.u8()?;
                let et = Self::compact_type(h & 0x0f)?;
                let n = if h >> 4 == 0x0f {
                    self.varint()? as i64
                } else {
                    (h >> 4) as i64
                };
                let n = self.count(n, 1)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.compact_value(et, depth + 1)?);
                }
                if t == T_LIST {
                    Tv::List(et, items)
                } else {
                    Tv::Set(et, items)
                }
            }
            T_MAP => {
                let n = self.varint()? as i64;
                if n == 0 {
                    return Ok(Tv::Map(T_STRING, T_STRING, vec![]));
                }
                let kv = self.u8()?;
                let (kt, vt) = (Self::compact_type(kv >> 4)?, Self::compact_type(kv & 0x0f)?);
                let n = self.count(n, 2)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let k = self.compact_value(kt, depth + 1)?;
                    let v = self.compact_value(vt, depth + 1)?;
                    items.push((k, v));
                }
                Tv::Map(kt, vt, items)
            }
            other => bail!("unknown Thrift type {other}"),
        })
    }
}

/// Parse one message from the front of `b`: the message, its protocol and the bytes it used.
pub fn decode(b: &[u8]) -> Result<(Message, Protocol, usize)> {
    let mut r = R {
        b,
        pos: 0,
        items: 0,
    };
    let first = *b.first().ok_or_else(|| anyhow!(Incomplete))?;
    let (name, kind, seqid, protocol) = if first == 0x82 {
        r.u8()?;
        let vt = r.u8()?;
        ensure!(
            vt & 0x1f == 1,
            "unsupported compact protocol version {}",
            vt & 0x1f
        );
        let seqid = r.varint()? as u32 as i32;
        let n = r.varint()?;
        let name = String::from_utf8(r.take(usize::try_from(n)?)?.to_vec())?;
        (name, vt >> 5, seqid, Protocol::Compact)
    } else if first & 0x80 != 0 {
        let v = r.i32()? as u32;
        ensure!(
            v & 0xffff_0000 == 0x8001_0000,
            "bad binary protocol version 0x{v:08x}"
        );
        let n = r.i32()?;
        ensure!(n >= 0, "negative method name length");
        let name = String::from_utf8(r.take(n as usize)?.to_vec())?;
        let seqid = r.i32()?;
        (name, (v & 0xff) as u8, seqid, Protocol::Binary)
    } else {
        // The old non-strict binary form: the name first.
        let n = r.i32()?;
        ensure!((0..=4096).contains(&n), "not a Thrift message");
        let name = String::from_utf8(r.take(n as usize)?.to_vec())?;
        let kind = r.u8()?;
        let seqid = r.i32()?;
        (name, kind, seqid, Protocol::Binary)
    };
    ensure!(
        name.len() <= 256 && (CALL..=ONEWAY).contains(&kind),
        "malformed message header"
    );
    let body = match protocol {
        Protocol::Binary => r.bin_value(T_STRUCT, 0)?,
        Protocol::Compact => r.compact_value(T_STRUCT, 0)?,
    };
    Ok((
        Message {
            name,
            kind,
            seqid,
            body,
        },
        protocol,
        r.pos,
    ))
}

pub fn is_incomplete(e: &anyhow::Error) -> bool {
    e.is::<Incomplete>()
}

struct W {
    out: Vec<u8>,
}

impl W {
    fn varint(&mut self, mut v: u64) {
        loop {
            if v < 0x80 {
                self.out.push(v as u8);
                return;
            }
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
    }
    fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn bin(&mut self, v: &Tv) {
        match v {
            Tv::Bool(b) => self.out.push(*b as u8),
            Tv::Byte(n) => self.out.push(*n as u8),
            Tv::I16(n) => self.out.extend(n.to_be_bytes()),
            Tv::I32(n) => self.out.extend(n.to_be_bytes()),
            Tv::I64(n) => self.out.extend(n.to_be_bytes()),
            Tv::Double(f) => self.out.extend(f.to_bits().to_be_bytes()),
            Tv::Bin(b) => {
                self.out.extend((b.len() as i32).to_be_bytes());
                self.out.extend(b);
            }
            Tv::Uuid(u) => self.out.extend(u),
            Tv::Struct(fields) => {
                for (id, f) in fields {
                    self.out.push(f.ttype());
                    self.out.extend(id.to_be_bytes());
                    self.bin(f);
                }
                self.out.push(T_STOP);
            }
            Tv::List(et, items) | Tv::Set(et, items) => {
                self.out.push(*et);
                self.out.extend((items.len() as i32).to_be_bytes());
                for i in items {
                    self.bin(i);
                }
            }
            Tv::Map(kt, vt, items) => {
                self.out.extend([*kt, *vt]);
                self.out.extend((items.len() as i32).to_be_bytes());
                for (k, v) in items {
                    self.bin(k);
                    self.bin(v);
                }
            }
        }
    }
    fn compact_code(t: u8) -> u8 {
        match t {
            T_BOOL => 1,
            T_BYTE => 3,
            T_I16 => 4,
            T_I32 => 5,
            T_I64 => 6,
            T_DOUBLE => 7,
            T_STRING => 8,
            T_LIST => 9,
            T_SET => 10,
            T_MAP => 11,
            T_STRUCT => 12,
            T_UUID => 13,
            _ => 0,
        }
    }
    fn compact(&mut self, v: &Tv) {
        match v {
            Tv::Bool(b) => self.out.push(if *b { 1 } else { 2 }),
            Tv::Byte(n) => self.out.push(*n as u8),
            Tv::I16(n) => self.zigzag(*n as i64),
            Tv::I32(n) => self.zigzag(*n as i64),
            Tv::I64(n) => self.zigzag(*n),
            Tv::Double(f) => self.out.extend(f.to_le_bytes()),
            Tv::Bin(b) => {
                self.varint(b.len() as u64);
                self.out.extend(b);
            }
            Tv::Uuid(u) => self.out.extend(u),
            Tv::Struct(fields) => {
                let mut last: i16 = 0;
                for (id, f) in fields {
                    let code = match f {
                        Tv::Bool(b) => {
                            if *b {
                                1
                            } else {
                                2
                            }
                        }
                        other => Self::compact_code(other.ttype()),
                    };
                    let delta = id.wrapping_sub(last);
                    if (1..=15).contains(&delta) {
                        self.out.push(((delta as u8) << 4) | code);
                    } else {
                        self.out.push(code);
                        self.zigzag(*id as i64);
                    }
                    last = *id;
                    if !matches!(f, Tv::Bool(_)) {
                        self.compact(f);
                    }
                }
                self.out.push(T_STOP);
            }
            Tv::List(et, items) | Tv::Set(et, items) => {
                let code = Self::compact_code(*et);
                if items.len() < 15 {
                    self.out.push(((items.len() as u8) << 4) | code);
                } else {
                    self.out.push(0xf0 | code);
                    self.varint(items.len() as u64);
                }
                for i in items {
                    // Booleans in containers are one byte: 1 true, 2 false, as the Java and
                    // Python implementations write them (readers test for 1).
                    if let Tv::Bool(b) = i {
                        self.out.push(if *b { 1 } else { 2 });
                    } else {
                        self.compact(i);
                    }
                }
            }
            Tv::Map(kt, vt, items) => {
                self.varint(items.len() as u64);
                if !items.is_empty() {
                    self.out
                        .push((Self::compact_code(*kt) << 4) | Self::compact_code(*vt));
                }
                for (k, v) in items {
                    for x in [k, v] {
                        if let Tv::Bool(b) = x {
                            self.out.push(if *b { 1 } else { 2 });
                        } else {
                            self.compact(x);
                        }
                    }
                }
            }
        }
    }
}

pub fn encode(m: &Message, protocol: Protocol) -> Vec<u8> {
    let mut w = W { out: Vec::new() };
    match protocol {
        Protocol::Binary => {
            w.out.extend((0x8001_0000u32 | m.kind as u32).to_be_bytes());
            w.out.extend((m.name.len() as i32).to_be_bytes());
            w.out.extend(m.name.as_bytes());
            w.out.extend(m.seqid.to_be_bytes());
            w.bin(&m.body);
        }
        Protocol::Compact => {
            w.out.push(0x82);
            w.out.push((m.kind << 5) | 1);
            w.varint(m.seqid as u32 as u64);
            w.varint(m.name.len() as u64);
            w.out.extend(m.name.as_bytes());
            w.compact(&m.body);
        }
    }
    w.out
}

/// A TApplicationException body: 1: message, 2: type.
pub fn application_exception(message: &str, kind: i32) -> Tv {
    Tv::Struct(vec![
        (1, Tv::Bin(message.as_bytes().to_vec())),
        (2, Tv::I32(kind)),
    ])
}
pub const UNKNOWN_METHOD: i32 = 1;
pub const INTERNAL_ERROR: i32 = 6;
pub const PROTOCOL_ERROR: i32 = 7;
