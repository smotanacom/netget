//! The AMQP 1.0 type system (OASIS AMQP 1.0, part 1): primitive encodings, lists, maps, arrays
//! and described types, decoded with depth and size bounds.
use anyhow::{bail, ensure, Context, Result};

const MAX_DEPTH: usize = 32;
const MAX_ITEMS: usize = 100_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Ubyte(u8),
    Ushort(u16),
    Uint(u32),
    Ulong(u64),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Char(char),
    Timestamp(i64),
    Uuid([u8; 16]),
    Binary(Vec<u8>),
    String(String),
    Symbol(String),
    Decimal(Vec<u8>),
    List(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Array(Vec<Value>),
    Described(Box<Value>, Box<Value>),
}

impl Value {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Ubyte(n) => Some(*n as u64),
            Value::Ushort(n) => Some(*n as u64),
            Value::Uint(n) => Some(*n as u64),
            Value::Ulong(n) => Some(*n),
            Value::Byte(n) if *n >= 0 => Some(*n as u64),
            Value::Short(n) if *n >= 0 => Some(*n as u64),
            Value::Int(n) if *n >= 0 => Some(*n as u64),
            Value::Long(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::Symbol(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    /// A described value's descriptor as a numeric code (symbolic descriptors mapped).
    pub fn descriptor(&self) -> Option<u64> {
        match self {
            Value::Described(d, _) => d.as_u64().or_else(|| d.as_str().and_then(symbolic_code)),
            _ => None,
        }
    }
    /// The fields of a described list (missing trailing fields are absent).
    pub fn fields(&self) -> &[Value] {
        match self {
            Value::Described(_, inner) => match inner.as_ref() {
                Value::List(f) => f,
                _ => &[],
            },
            _ => &[],
        }
    }
    pub fn described(code: u64, fields: Vec<Value>) -> Value {
        Value::Described(Box::new(Value::Ulong(code)), Box::new(Value::List(fields)))
    }
    pub fn sym(s: &str) -> Value {
        Value::Symbol(s.to_owned())
    }
    pub fn str(s: &str) -> Value {
        Value::String(s.to_owned())
    }
}

/// Field `i` of a described list, or Null.
pub fn field(v: &Value, i: usize) -> &Value {
    v.fields().get(i).unwrap_or(&Value::Null)
}

fn symbolic_code(s: &str) -> Option<u64> {
    Some(match s {
        "amqp:open:list" => 0x10,
        "amqp:begin:list" => 0x11,
        "amqp:attach:list" => 0x12,
        "amqp:flow:list" => 0x13,
        "amqp:transfer:list" => 0x14,
        "amqp:disposition:list" => 0x15,
        "amqp:detach:list" => 0x16,
        "amqp:end:list" => 0x17,
        "amqp:close:list" => 0x18,
        "amqp:error:list" => 0x1d,
        "amqp:received:list" => 0x23,
        "amqp:accepted:list" => 0x24,
        "amqp:rejected:list" => 0x25,
        "amqp:released:list" => 0x26,
        "amqp:modified:list" => 0x27,
        "amqp:source:list" => 0x28,
        "amqp:target:list" => 0x29,
        "amqp:sasl-mechanisms:list" => 0x40,
        "amqp:sasl-init:list" => 0x41,
        "amqp:sasl-outcome:list" => 0x44,
        "amqp:header:list" => 0x70,
        "amqp:delivery-annotations:map" => 0x71,
        "amqp:message-annotations:map" => 0x72,
        "amqp:properties:list" => 0x73,
        "amqp:application-properties:map" => 0x74,
        "amqp:data:binary" => 0x75,
        "amqp:amqp-sequence:list" => 0x76,
        "amqp:amqp-value:*" => 0x77,
        "amqp:footer:map" => 0x78,
        _ => return None,
    })
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    items: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let v = self
            .b
            .get(self.pos..self.pos.checked_add(n).context("length overflow")?)
            .context("truncated AMQP value")?;
        self.pos += n;
        Ok(v)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into()?)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        let code = self.u8()?;
        self.with_code(code, depth)
    }

    fn with_code(&mut self, code: u8, depth: usize) -> Result<Value> {
        ensure!(depth <= MAX_DEPTH, "AMQP value nests too deeply");
        self.items += 1;
        ensure!(self.items <= MAX_ITEMS, "too many AMQP values");
        Ok(match code {
            0x00 => {
                let d = self.value(depth + 1)?;
                let v = self.value(depth + 1)?;
                Value::Described(Box::new(d), Box::new(v))
            }
            0x40 => Value::Null,
            0x41 => Value::Bool(true),
            0x42 => Value::Bool(false),
            0x56 => Value::Bool(self.u8()? != 0),
            0x50 => Value::Ubyte(self.u8()?),
            0x60 => Value::Ushort(u16::from_be_bytes(self.arr()?)),
            0x70 => Value::Uint(self.u32()?),
            0x52 => Value::Uint(self.u8()? as u32),
            0x43 => Value::Uint(0),
            0x80 => Value::Ulong(u64::from_be_bytes(self.arr()?)),
            0x53 => Value::Ulong(self.u8()? as u64),
            0x44 => Value::Ulong(0),
            0x51 => Value::Byte(self.u8()? as i8),
            0x61 => Value::Short(i16::from_be_bytes(self.arr()?)),
            0x71 => Value::Int(i32::from_be_bytes(self.arr()?)),
            0x54 => Value::Int(self.u8()? as i8 as i32),
            0x81 => Value::Long(i64::from_be_bytes(self.arr()?)),
            0x55 => Value::Long(self.u8()? as i8 as i64),
            0x72 => Value::Float(f32::from_be_bytes(self.arr()?)),
            0x82 => Value::Double(f64::from_be_bytes(self.arr()?)),
            0x74 => Value::Decimal(self.take(4)?.to_vec()),
            0x84 => Value::Decimal(self.take(8)?.to_vec()),
            0x94 => Value::Decimal(self.take(16)?.to_vec()),
            0x73 => Value::Char(char::from_u32(self.u32()?).context("invalid char")?),
            0x83 => Value::Timestamp(i64::from_be_bytes(self.arr()?)),
            0x98 => Value::Uuid(self.arr()?),
            0xa0 | 0xb0 => {
                let n = if code == 0xa0 {
                    self.u8()? as usize
                } else {
                    self.u32()? as usize
                };
                Value::Binary(self.take(n)?.to_vec())
            }
            0xa1 | 0xb1 | 0xa3 | 0xb3 => {
                let n = if code & 0xf0 == 0xa0 {
                    self.u8()? as usize
                } else {
                    self.u32()? as usize
                };
                let s = std::str::from_utf8(self.take(n)?)
                    .context("string is not UTF-8")?
                    .to_owned();
                if code & 0x0f == 1 {
                    Value::String(s)
                } else {
                    Value::Symbol(s)
                }
            }
            0x45 => Value::List(vec![]),
            0xc0 | 0xd0 | 0xc1 | 0xd1 => {
                let wide = code & 0xf0 == 0xd0;
                let size = if wide {
                    self.u32()? as usize
                } else {
                    self.u8()? as usize
                };
                let end = self
                    .pos
                    .checked_add(size)
                    .filter(|e| *e <= self.b.len())
                    .context("compound longer than its buffer")?;
                let count = if wide {
                    self.u32()? as usize
                } else {
                    self.u8()? as usize
                };
                ensure!(count <= MAX_ITEMS, "compound count too large");
                let mut items = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    items.push(self.value(depth + 1)?);
                }
                ensure!(self.pos == end, "compound size does not match its contents");
                if code & 0x0f == 0 {
                    Value::List(items)
                } else {
                    ensure!(count % 2 == 0, "map with an odd count");
                    let mut m = Vec::with_capacity(items.len() / 2);
                    let mut it = items.into_iter();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        m.push((k, v));
                    }
                    Value::Map(m)
                }
            }
            0xe0 | 0xf0 => {
                let wide = code == 0xf0;
                let size = if wide {
                    self.u32()? as usize
                } else {
                    self.u8()? as usize
                };
                let end = self
                    .pos
                    .checked_add(size)
                    .filter(|e| *e <= self.b.len())
                    .context("array longer than its buffer")?;
                let count = if wide {
                    self.u32()? as usize
                } else {
                    self.u8()? as usize
                };
                ensure!(count <= MAX_ITEMS, "array count too large");
                let mut ctor = self.u8()?;
                let mut descriptor = None;
                if ctor == 0x00 {
                    descriptor = Some(self.value(depth + 1)?);
                    ctor = self.u8()?;
                }
                let mut items = Vec::with_capacity(count.min(1024));
                for _ in 0..count {
                    let v = self.with_code(ctor, depth + 1)?;
                    items.push(match &descriptor {
                        Some(d) => Value::Described(Box::new(d.clone()), Box::new(v)),
                        None => v,
                    });
                }
                ensure!(self.pos == end, "array size does not match its contents");
                Value::Array(items)
            }
            other => bail!("unknown AMQP type code 0x{other:02x}"),
        })
    }
}

/// Decode one value from the front of `b`; the value and the bytes it used.
pub fn decode(b: &[u8]) -> Result<(Value, usize)> {
    let mut r = Reader {
        b,
        pos: 0,
        items: 0,
    };
    let v = r.value(0)?;
    Ok((v, r.pos))
}

/// Decode every value in `b` (message sections follow one another).
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

fn sized(out: &mut Vec<u8>, short: u8, long: u8, body: &[u8], count: usize) {
    if body.len() < 255 && count < 256 {
        out.push(short);
        out.push((body.len() + 1) as u8);
        out.push(count as u8);
    } else {
        out.push(long);
        out.extend(((body.len() + 4) as u32).to_be_bytes());
        out.extend((count as u32).to_be_bytes());
    }
    out.extend(body);
}

/// The constructor code and encoded body of a value, for array elements (one constructor each).
fn element(v: &Value) -> (u8, Vec<u8>) {
    let mut full = Vec::new();
    encode_into(v, &mut full);
    match v {
        // Arrays need a fixed constructor: use the wide forms for variable sizes.
        Value::String(s) => (
            0xb1,
            [&(s.len() as u32).to_be_bytes()[..], s.as_bytes()].concat(),
        ),
        Value::Symbol(s) => (
            0xb3,
            [&(s.len() as u32).to_be_bytes()[..], s.as_bytes()].concat(),
        ),
        Value::Binary(b) => (0xb0, [&(b.len() as u32).to_be_bytes()[..], b].concat()),
        Value::Uint(n) => (0x70, n.to_be_bytes().to_vec()),
        Value::Ulong(n) => (0x80, n.to_be_bytes().to_vec()),
        Value::Int(n) => (0x71, n.to_be_bytes().to_vec()),
        Value::Long(n) => (0x81, n.to_be_bytes().to_vec()),
        Value::Bool(b) => (0x56, vec![*b as u8]),
        _ => (full[0], full[1..].to_vec()),
    }
}

pub fn encode_into(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0x40),
        Value::Bool(true) => out.push(0x41),
        Value::Bool(false) => out.push(0x42),
        Value::Ubyte(n) => out.extend([0x50, *n]),
        Value::Ushort(n) => {
            out.push(0x60);
            out.extend(n.to_be_bytes());
        }
        Value::Uint(0) => out.push(0x43),
        Value::Uint(n) if *n < 256 => out.extend([0x52, *n as u8]),
        Value::Uint(n) => {
            out.push(0x70);
            out.extend(n.to_be_bytes());
        }
        Value::Ulong(0) => out.push(0x44),
        Value::Ulong(n) if *n < 256 => out.extend([0x53, *n as u8]),
        Value::Ulong(n) => {
            out.push(0x80);
            out.extend(n.to_be_bytes());
        }
        Value::Byte(n) => out.extend([0x51, *n as u8]),
        Value::Short(n) => {
            out.push(0x61);
            out.extend(n.to_be_bytes());
        }
        Value::Int(n) if (-128..128).contains(n) => out.extend([0x54, *n as i8 as u8]),
        Value::Int(n) => {
            out.push(0x71);
            out.extend(n.to_be_bytes());
        }
        Value::Long(n) if (-128..128).contains(n) => out.extend([0x55, *n as i8 as u8]),
        Value::Long(n) => {
            out.push(0x81);
            out.extend(n.to_be_bytes());
        }
        Value::Float(f) => {
            out.push(0x72);
            out.extend(f.to_be_bytes());
        }
        Value::Double(f) => {
            out.push(0x82);
            out.extend(f.to_be_bytes());
        }
        Value::Decimal(b) => {
            out.push(match b.len() {
                4 => 0x74,
                8 => 0x84,
                _ => 0x94,
            });
            out.extend(b);
        }
        Value::Char(c) => {
            out.push(0x73);
            out.extend((*c as u32).to_be_bytes());
        }
        Value::Timestamp(t) => {
            out.push(0x83);
            out.extend(t.to_be_bytes());
        }
        Value::Uuid(u) => {
            out.push(0x98);
            out.extend(u);
        }
        Value::Binary(b) => var(out, 0xa0, 0xb0, b),
        Value::String(s) => var(out, 0xa1, 0xb1, s.as_bytes()),
        Value::Symbol(s) => var(out, 0xa3, 0xb3, s.as_bytes()),
        Value::List(items) if items.is_empty() => out.push(0x45),
        Value::List(items) => {
            let mut body = Vec::new();
            for i in items {
                encode_into(i, &mut body);
            }
            sized(out, 0xc0, 0xd0, &body, items.len());
        }
        Value::Map(pairs) => {
            let mut body = Vec::new();
            for (k, v) in pairs {
                encode_into(k, &mut body);
                encode_into(v, &mut body);
            }
            sized(out, 0xc1, 0xd1, &body, pairs.len() * 2);
        }
        Value::Array(items) => {
            let (ctor, mut body) = match items.first() {
                None => (0x40, Vec::new()),
                Some(first) => (element(first).0, Vec::new()),
            };
            for i in items {
                body.extend(element(i).1);
            }
            let mut all = vec![ctor];
            all.extend(body);
            // array8/array32: size covers count and constructor and elements.
            if all.len() < 255 && items.len() < 256 {
                out.push(0xe0);
                out.push((all.len() + 1) as u8);
                out.push(items.len() as u8);
            } else {
                out.push(0xf0);
                out.extend(((all.len() + 4) as u32).to_be_bytes());
                out.extend((items.len() as u32).to_be_bytes());
            }
            out.extend(all);
        }
        Value::Described(d, v) => {
            out.push(0x00);
            encode_into(d, out);
            encode_into(v, out);
        }
    }
}

fn var(out: &mut Vec<u8>, short: u8, long: u8, b: &[u8]) {
    if b.len() < 256 {
        out.extend([short, b.len() as u8]);
    } else {
        out.push(long);
        out.extend((b.len() as u32).to_be_bytes());
    }
    out.extend(b);
}

pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(v, &mut out);
    out
}
