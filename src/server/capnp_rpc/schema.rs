//! A Cap'n Proto schema loaded at startup from `capnp compile -o-` output (a
//! CodeGeneratorRequest), and the JSON mapping it drives: a struct's fields by name, enums by
//! enumerant name, unions as the one member that is set, groups as nested objects, Data as
//! `{"$hex": …}`. The layout offsets of schema.capnp itself are written out below, read from
//! `capnp compile -ocapnp schema.capnp`.
use super::layout::{
    Builder, ElementSize, ListReader, Message, StructBuilder, StructReader, Target,
};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Largest compiled schema accepted.
pub const MAX_SCHEMA_BYTES: usize = 4 * 1024 * 1024;
/// Most elements in one list the codec writes from JSON.
pub const MAX_LIST: usize = 65_536;

#[derive(Clone, Debug)]
pub enum Type {
    Void,
    Bool,
    Int(u8),
    UInt(u8),
    Float32,
    Float64,
    Text,
    Data,
    List(Box<Type>),
    Enum(u64),
    Struct(u64),
    /// Interfaces and AnyPointer: not mapped (read as null, written as null).
    Opaque,
}

#[derive(Clone, Debug)]
pub enum FieldKind {
    Slot { offset: u32, ty: Type, default: u64 },
    Group(u64),
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    /// Set for a member of the struct's union.
    pub discriminant: Option<u16>,
    pub kind: FieldKind,
}

#[derive(Clone, Debug)]
pub struct StructInfo {
    pub name: String,
    pub data_words: u16,
    pub ptr_count: u16,
    pub discriminant_offset: u32,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub struct Method {
    pub name: String,
    pub id: u16,
    pub params: u64,
    pub results: u64,
}

#[derive(Clone, Debug)]
pub struct InterfaceInfo {
    pub name: String,
    pub id: u64,
    pub methods: Vec<Method>,
    pub superclasses: Vec<u64>,
}

#[derive(Debug, Default)]
pub struct Schema {
    pub structs: HashMap<u64, StructInfo>,
    pub enums: HashMap<u64, Vec<String>>,
    pub interfaces: HashMap<u64, InterfaceInfo>,
}

fn short_name(display: &str, prefix: u32) -> String {
    display
        .get(prefix as usize..)
        .unwrap_or(display)
        .to_string()
}

fn read_type(t: StructReader<'_>) -> Result<Type> {
    Ok(match t.u16(0) {
        0 => Type::Void,
        1 => Type::Bool,
        2 => Type::Int(8),
        3 => Type::Int(16),
        4 => Type::Int(32),
        5 => Type::Int(64),
        6 => Type::UInt(8),
        7 => Type::UInt(16),
        8 => Type::UInt(32),
        9 => Type::UInt(64),
        10 => Type::Float32,
        11 => Type::Float64,
        12 => Type::Text,
        13 => Type::Data,
        14 => Type::List(Box::new(read_type(
            t.struct_field(0)?.context("list without element type")?,
        )?)),
        15 => Type::Enum(t.u64(1)),
        16 => Type::Struct(t.u64(1)),
        _ => Type::Opaque,
    })
}

/// The raw bits of a primitive default, so a field reads as `stored ^ default`.
fn read_default(v: Option<StructReader<'_>>, ty: &Type) -> u64 {
    let Some(v) = v else { return 0 };
    match ty {
        Type::Bool => v.bits(16, 1),
        Type::Int(8) | Type::UInt(8) => v.bits(16, 8),
        Type::Int(16) | Type::UInt(16) | Type::Enum(_) => v.bits(16, 16),
        Type::Int(32) | Type::UInt(32) | Type::Float32 => v.bits(32, 32),
        Type::Int(64) | Type::UInt(64) | Type::Float64 => v.u64(1),
        _ => 0,
    }
}

impl Schema {
    /// Parse a CodeGeneratorRequest.
    pub fn load(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= MAX_SCHEMA_BYTES, "compiled schema too large");
        let segments = frame_segments(bytes)?;
        let msg = Message::new(segments);
        let request = msg.root_struct()?;
        let nodes = request.list_field(0)?.context("no nodes in the schema")?;
        let mut schema = Schema::default();
        for i in 0..nodes.len {
            let node = nodes.struct_at(i)?;
            let id = node.u64(0);
            let name = short_name(&node.text(0)?.unwrap_or_default(), node.u32(2));
            match node.u16(6) {
                1 => {
                    let mut fields = Vec::new();
                    if let Some(list) = node.list_field(3)? {
                        for f in 0..list.len {
                            let f = list.struct_at(f)?;
                            let discriminant = match f.u16(1) ^ 0xffff {
                                0xffff => None,
                                d => Some(d),
                            };
                            let kind = match f.u16(4) {
                                0 => {
                                    let ty = read_type(
                                        f.struct_field(2)?.context("slot without type")?,
                                    )?;
                                    let default = read_default(f.struct_field(3)?, &ty);
                                    FieldKind::Slot {
                                        offset: f.u32(1),
                                        ty,
                                        default,
                                    }
                                }
                                _ => FieldKind::Group(f.u64(2)),
                            };
                            fields.push(Field {
                                name: f.text(0)?.unwrap_or_default(),
                                discriminant,
                                kind,
                            });
                        }
                    }
                    schema.structs.insert(
                        id,
                        StructInfo {
                            name,
                            data_words: node.u16(7),
                            ptr_count: node.u16(12),
                            discriminant_offset: node.u32(8),
                            fields,
                        },
                    );
                }
                2 => {
                    let mut names = Vec::new();
                    if let Some(list) = node.list_field(3)? {
                        for e in 0..list.len {
                            names.push(list.struct_at(e)?.text(0)?.unwrap_or_default());
                        }
                    }
                    schema.enums.insert(id, names);
                }
                3 => {
                    let mut methods = Vec::new();
                    if let Some(list) = node.list_field(3)? {
                        for m in 0..list.len {
                            let m_reader = list.struct_at(m)?;
                            methods.push(Method {
                                name: m_reader.text(0)?.unwrap_or_default(),
                                id: m as u16,
                                params: m_reader.u64(1),
                                results: m_reader.u64(2),
                            });
                        }
                    }
                    let mut superclasses = Vec::new();
                    if let Some(list) = node.list_field(4)? {
                        for s in 0..list.len {
                            superclasses.push(list.struct_at(s)?.u64(0));
                        }
                    }
                    schema.interfaces.insert(
                        id,
                        InterfaceInfo {
                            name,
                            id,
                            methods,
                            superclasses,
                        },
                    );
                }
                _ => {}
            }
        }
        Ok(schema)
    }

    /// The interface named `name` (its short name, or its full display name).
    pub fn interface(&self, name: &str) -> Result<&InterfaceInfo> {
        let mut found = self
            .interfaces
            .values()
            .filter(|i| i.name == name || i.name.rsplit(['.', ':']).next() == Some(name));
        let first = found
            .next()
            .with_context(|| format!("the schema has no interface named {name}"))?;
        ensure!(found.next().is_none(), "interface name {name} is ambiguous");
        Ok(first)
    }

    /// Every method `iface` answers, its own and its superclasses', as (interface id, method).
    pub fn methods_of(&self, iface: u64) -> Vec<(u64, &InterfaceInfo, &Method)> {
        let mut out = Vec::new();
        let mut pending = vec![iface];
        let mut seen = Vec::new();
        while let Some(id) = pending.pop() {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            if let Some(i) = self.interfaces.get(&id) {
                out.extend(i.methods.iter().map(|m| (id, i, m)));
                pending.extend(i.superclasses.iter().copied());
            }
        }
        out
    }

    fn info(&self, id: u64) -> Result<&StructInfo> {
        self.structs
            .get(&id)
            .with_context(|| format!("struct {id:#x} is not in the schema"))
    }

    pub fn struct_size(&self, id: u64) -> Result<(u16, u16)> {
        let s = self.info(id)?;
        Ok((s.data_words, s.ptr_count))
    }

    /// A struct as JSON.
    pub fn to_json(&self, id: u64, s: StructReader<'_>) -> Result<Value> {
        let info = self.info(id)?;
        let active = s.u16(u64::from(info.discriminant_offset));
        let mut out = Map::new();
        for f in &info.fields {
            if f.discriminant.is_some_and(|d| d != active) {
                continue;
            }
            let v = match &f.kind {
                FieldKind::Group(gid) => self.to_json(*gid, s)?,
                FieldKind::Slot {
                    offset,
                    ty,
                    default,
                } => self.slot_to_json(s, *offset, ty, *default)?,
            };
            out.insert(f.name.clone(), v);
        }
        Ok(Value::Object(out))
    }

    fn slot_to_json(
        &self,
        s: StructReader<'_>,
        offset: u32,
        ty: &Type,
        default: u64,
    ) -> Result<Value> {
        let o = u64::from(offset);
        Ok(match ty {
            Type::Void => Value::Null,
            Type::Bool => json!(s.bits(o, 1) ^ default != 0),
            Type::Int(w) | Type::UInt(w) => {
                let raw = s.bits(o * u64::from(*w), u64::from(*w)) ^ default;
                integer(ty, raw)
            }
            Type::Float32 => json!(f32::from_bits((s.bits(o * 32, 32) ^ default) as u32)),
            Type::Float64 => json!(f64::from_bits(s.bits(o * 64, 64) ^ default)),
            Type::Enum(eid) => self.enum_name(*eid, (s.bits(o * 16, 16) ^ default) as u16),
            Type::Text => s.text(offset as u16)?.map_or(Value::Null, Value::String),
            Type::Data => s
                .data(offset as u16)?
                .map_or(Value::Null, |b| json!({"$hex": hex::encode(b)})),
            Type::Struct(sid) => match s.struct_field(offset as u16)? {
                Some(c) => self.to_json(*sid, c)?,
                None => Value::Null,
            },
            Type::List(el) => match s.list_field(offset as u16)? {
                Some(l) => self.list_to_json(el, l)?,
                None => Value::Null,
            },
            Type::Opaque => Value::Null,
        })
    }

    fn enum_name(&self, id: u64, v: u16) -> Value {
        match self.enums.get(&id).and_then(|n| n.get(v as usize)) {
            Some(n) => json!(n),
            None => json!(v),
        }
    }

    fn list_to_json(&self, el: &Type, l: ListReader<'_>) -> Result<Value> {
        let mut out = Vec::with_capacity(l.len.min(4096) as usize);
        for i in 0..l.len {
            out.push(match el {
                Type::Struct(sid) => self.to_json(*sid, l.struct_at(i)?)?,
                Type::Text | Type::Data | Type::List(_) | Type::Opaque => match l.pointer_at(i)? {
                    Target::Null => Value::Null,
                    Target::List(inner) => match el {
                        Type::Text => json!(inner.text()?),
                        Type::Data => json!({"$hex": hex::encode(inner.bytes()?)}),
                        Type::List(e) => self.list_to_json(e, inner)?,
                        _ => Value::Null,
                    },
                    _ => Value::Null,
                },
                Type::Void => Value::Null,
                Type::Bool => json!(l.primitive(i)? != 0),
                Type::Int(_) | Type::UInt(_) => integer(el, l.primitive(i)?),
                Type::Float32 => json!(f32::from_bits(l.primitive(i)? as u32)),
                Type::Float64 => json!(f64::from_bits(l.primitive(i)?)),
                Type::Enum(eid) => self.enum_name(*eid, l.primitive(i)? as u16),
            });
        }
        Ok(Value::Array(out))
    }

    /// Fill struct `s` (already allocated with this struct's size) from a JSON object. Unknown
    /// keys are refused, so a misspelt field is an error rather than silently dropped.
    pub fn from_json(&self, b: &mut Builder, id: u64, s: StructBuilder, v: &Value) -> Result<()> {
        let info = self.info(id)?;
        let obj = match v {
            Value::Null => return self.set_defaults(b, info, s),
            Value::Object(o) => o,
            _ => bail!("{} must be a JSON object", info.name),
        };
        for key in obj.keys() {
            ensure!(
                info.fields.iter().any(|f| &f.name == key),
                "{} has no field {key}",
                info.name
            );
        }
        let members: Vec<&Field> = info
            .fields
            .iter()
            .filter(|f| f.discriminant.is_some() && obj.contains_key(&f.name))
            .collect();
        ensure!(
            members.len() <= 1,
            "{}: set at most one union member",
            info.name
        );
        self.set_defaults(b, info, s)?;
        if let Some(d) = members.first().and_then(|f| f.discriminant) {
            b.set_u16(s, u64::from(info.discriminant_offset), d);
        }
        for f in &info.fields {
            let Some(value) = obj.get(&f.name) else {
                continue;
            };
            let at = |e: anyhow::Error| e.context(format!("{}.{}", info.name, f.name));
            match &f.kind {
                FieldKind::Group(gid) => self.from_json(b, *gid, s, value).map_err(at)?,
                FieldKind::Slot {
                    offset,
                    ty,
                    default,
                } => self
                    .set_slot(b, s, *offset, ty, *default, value)
                    .map_err(at)?,
            }
        }
        Ok(())
    }

    /// Write every primitive default, so an absent field reads as its declared default.
    fn set_defaults(&self, b: &mut Builder, info: &StructInfo, s: StructBuilder) -> Result<()> {
        // Stored bits are value ^ default, so a zero data section already reads as every
        // default. Nothing to write.
        let _ = (b, info, s);
        Ok(())
    }

    fn set_slot(
        &self,
        b: &mut Builder,
        s: StructBuilder,
        offset: u32,
        ty: &Type,
        default: u64,
        v: &Value,
    ) -> Result<()> {
        let o = u64::from(offset);
        if v.is_null() {
            return Ok(());
        }
        match ty {
            Type::Void | Type::Opaque => {}
            Type::Bool => {
                let x = v.as_bool().context("expected a boolean")?;
                b.set_bits(s, o, 1, u64::from(x) ^ default);
            }
            Type::Int(w) | Type::UInt(w) => b.set_bits(
                s,
                o * u64::from(*w),
                u64::from(*w),
                int_bits(ty, v)? ^ default,
            ),
            Type::Float32 => {
                let x = v.as_f64().context("expected a number")? as f32;
                b.set_bits(s, o * 32, 32, u64::from(x.to_bits()) ^ default);
            }
            Type::Float64 => {
                let x = v.as_f64().context("expected a number")?;
                b.set_bits(s, o * 64, 64, x.to_bits() ^ default);
            }
            Type::Enum(eid) => {
                let x = self.enum_value(*eid, v)?;
                b.set_bits(s, o * 16, 16, u64::from(x) ^ default);
            }
            Type::Text => b.set_text(s, offset as u16, v.as_str().context("expected a string")?)?,
            Type::Data => b.set_bytes(s, offset as u16, &data_bytes(v)?)?,
            Type::Struct(sid) => {
                let (dw, pc) = self.struct_size(*sid)?;
                let child = b.init_struct(s, offset as u16, dw, pc)?;
                self.from_json(b, *sid, child, v)?;
            }
            Type::List(el) => {
                let slot = Builder::pointer_slot_of(s, offset as u16)?;
                self.list_from_json(b, slot, el, v)?;
            }
        }
        Ok(())
    }

    fn enum_value(&self, id: u64, v: &Value) -> Result<u16> {
        if let Some(n) = v.as_u64() {
            return u16::try_from(n).context("enum value out of range");
        }
        let name = v.as_str().context("expected an enumerant name")?;
        self.enums
            .get(&id)
            .and_then(|names| names.iter().position(|n| n == name))
            .map(|p| p as u16)
            .with_context(|| format!("no enumerant {name}"))
    }

    fn list_from_json(&self, b: &mut Builder, slot: usize, el: &Type, v: &Value) -> Result<()> {
        let items = v.as_array().context("expected an array")?;
        ensure!(items.len() <= MAX_LIST, "list longer than {MAX_LIST}");
        let len = items.len() as u32;
        match el {
            Type::Struct(sid) => {
                let (dw, pc) = self.struct_size(*sid)?;
                let elements = b.slot_struct_list(slot, len, dw, pc)?;
                for (e, item) in elements.into_iter().zip(items) {
                    self.from_json(b, *sid, e, item)?;
                }
            }
            Type::Text | Type::Data | Type::List(_) | Type::Opaque => {
                let slots = b.slot_pointer_list(slot, len)?;
                for (p, item) in slots.into_iter().zip(items) {
                    if item.is_null() {
                        continue;
                    }
                    match el {
                        Type::Text => {
                            let mut bytes = item
                                .as_str()
                                .context("expected a string")?
                                .as_bytes()
                                .to_vec();
                            bytes.push(0);
                            b.slot_bytes(p, &bytes)?;
                        }
                        Type::Data => b.slot_bytes(p, &data_bytes(item)?)?,
                        Type::List(e) => self.list_from_json(b, p, e, item)?,
                        _ => {}
                    }
                }
            }
            _ => {
                let size = match el {
                    Type::Void => ElementSize::Void,
                    Type::Bool => ElementSize::Bit,
                    Type::Int(8) | Type::UInt(8) => ElementSize::Byte,
                    Type::Int(16) | Type::UInt(16) | Type::Enum(_) => ElementSize::TwoBytes,
                    Type::Int(32) | Type::UInt(32) | Type::Float32 => ElementSize::FourBytes,
                    _ => ElementSize::EightBytes,
                };
                let values = items
                    .iter()
                    .map(|item| {
                        Ok(match el {
                            Type::Void => 0,
                            Type::Bool => u64::from(item.as_bool().context("expected a boolean")?),
                            Type::Int(_) | Type::UInt(_) => int_bits(el, item)?,
                            Type::Float32 => u64::from(
                                (item.as_f64().context("expected a number")? as f32).to_bits(),
                            ),
                            Type::Float64 => item.as_f64().context("expected a number")?.to_bits(),
                            Type::Enum(eid) => u64::from(self.enum_value(*eid, item)?),
                            _ => 0,
                        })
                    })
                    .collect::<Result<Vec<u64>>>()?;
                b.slot_primitive_list(slot, size, &values)?;
            }
        }
        Ok(())
    }

    /// A short description of a method's parameters or results, for the model's prompt. A
    /// struct already being described (a recursive type) is named rather than expanded.
    pub fn describe(&self, id: u64) -> String {
        self.describe_in(id, &mut Vec::new())
    }

    fn describe_in(&self, id: u64, open: &mut Vec<u64>) -> String {
        let Ok(info) = self.info(id) else {
            return String::new();
        };
        open.push(id);
        let out = info
            .fields
            .iter()
            .map(|f| match &f.kind {
                FieldKind::Slot { ty, .. } => format!("{}: {}", f.name, self.type_name(ty, open)),
                FieldKind::Group(g) => format!("{}: {{{}}}", f.name, self.describe_in(*g, open)),
            })
            .collect::<Vec<_>>()
            .join(", ");
        open.pop();
        out
    }

    fn type_name(&self, ty: &Type, open: &mut Vec<u64>) -> String {
        match ty {
            Type::Void => "Void".into(),
            Type::Bool => "Bool".into(),
            Type::Int(w) => format!("Int{w}"),
            Type::UInt(w) => format!("UInt{w}"),
            Type::Float32 => "Float32".into(),
            Type::Float64 => "Float64".into(),
            Type::Text => "Text".into(),
            Type::Data => "Data {\"$hex\"}".into(),
            Type::List(e) => format!("List({})", self.type_name(e, open)),
            Type::Enum(id) => match self.enums.get(id) {
                Some(n) => format!("enum {}", n.join("|")),
                None => "enum".into(),
            },
            Type::Struct(id) if open.contains(id) || open.len() >= 8 => self
                .structs
                .get(id)
                .map_or_else(|| "struct".into(), |s| s.name.clone()),
            Type::Struct(id) => format!("{{{}}}", self.describe_in(*id, open)),
            Type::Opaque => "unsupported".into(),
        }
    }
}

fn integer(ty: &Type, raw: u64) -> Value {
    match ty {
        Type::Int(8) => json!(raw as u8 as i8),
        Type::Int(16) => json!(raw as u16 as i16),
        Type::Int(32) => json!(raw as u32 as i32),
        Type::Int(_) => json!(raw as i64),
        _ => json!(raw),
    }
}

fn int_bits(ty: &Type, v: &Value) -> Result<u64> {
    let (signed, width) = match ty {
        Type::Int(w) => (true, *w),
        Type::UInt(w) => (false, *w),
        _ => bail!("not an integer type"),
    };
    if signed {
        let x = v.as_i64().context("expected an integer")?;
        let (min, max) = if width == 64 {
            (i64::MIN, i64::MAX)
        } else {
            (-(1i64 << (width - 1)), (1i64 << (width - 1)) - 1)
        };
        ensure!((min..=max).contains(&x), "Int{width} out of range");
        Ok(if width == 64 {
            x as u64
        } else {
            (x as u64) & ((1u64 << width) - 1)
        })
    } else {
        let x = v.as_u64().context("expected a non-negative integer")?;
        ensure!(
            width == 64 || x < (1u64 << width),
            "UInt{width} out of range"
        );
        Ok(x)
    }
}

fn data_bytes(v: &Value) -> Result<Vec<u8>> {
    let h = v["$hex"]
        .as_str()
        .context("Data must be {\"$hex\": \"…\"}")?;
    hex::decode(h).context("invalid hex")
}

/// A framed message held in memory.
pub fn frame_segments(bytes: &[u8]) -> Result<Vec<Vec<u64>>> {
    ensure!(bytes.len() >= 8, "truncated message");
    let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize + 1;
    ensure!(count <= super::layout::MAX_SEGMENTS, "too many segments");
    let header = (4 + 4 * count).div_ceil(8) * 8;
    ensure!(bytes.len() >= header, "truncated segment table");
    let mut at = header;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let size = u32::from_le_bytes(bytes[4 + 4 * i..8 + 4 * i].try_into().unwrap()) as usize;
        let end = at
            .checked_add(size.checked_mul(8).context("segment size overflow")?)
            .context("segment size overflow")?;
        ensure!(end <= bytes.len(), "truncated segment");
        out.push(
            bytes[at..end]
                .chunks_exact(8)
                .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                .collect(),
        );
        at = end;
    }
    Ok(out)
}
