//! Thrift values to and from JSON, by their IDL types: structs are objects keyed by field name,
//! enums their labels, maps with string keys objects (others `[[k, v], ...]`), binary as text
//! when UTF-8 and by length otherwise.
use super::codec::*;
use super::idl::{Field, Idl, Type};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value as Json};

pub fn ttype(t: &Type) -> u8 {
    match t {
        Type::Bool => T_BOOL,
        Type::Byte => T_BYTE,
        Type::I16 => T_I16,
        Type::I32 | Type::Enum(_) => T_I32,
        Type::I64 => T_I64,
        Type::Double => T_DOUBLE,
        Type::String | Type::Binary => T_STRING,
        Type::Uuid => T_UUID,
        Type::List(_) => T_LIST,
        Type::Set(_) => T_SET,
        Type::Map(..) => T_MAP,
        Type::Struct(_) => T_STRUCT,
    }
}

/// A wire value as JSON, read by its declared type (or generically when the type disagrees).
pub fn to_json(idl: &Idl, t: Option<&Type>, v: &Tv) -> Json {
    match (t, v) {
        (_, Tv::Bool(b)) => json!(b),
        (_, Tv::Byte(n)) => json!(n),
        (_, Tv::I16(n)) => json!(n),
        (Some(Type::Enum(e)), Tv::I32(n)) => {
            idl.enum_name(e, *n).map(|l| json!(l)).unwrap_or(json!(n))
        }
        (_, Tv::I32(n)) => json!(n),
        (_, Tv::I64(n)) => json!(n),
        (_, Tv::Double(f)) => json!(f),
        (_, Tv::Uuid(u)) => json!(uuid::Uuid::from_bytes(*u).to_string()),
        (Some(Type::Binary), Tv::Bin(b)) => match std::str::from_utf8(b) {
            Ok(s) => json!(s),
            Err(_) => json!({"binary_length": b.len()}),
        },
        (_, Tv::Bin(b)) => json!(String::from_utf8_lossy(b)),
        (t, Tv::Struct(fields)) => {
            let decl = match t {
                Some(Type::Struct(name)) => idl.structs.get(name),
                _ => None,
            };
            struct_json(idl, decl.map(|d| d.as_slice()).unwrap_or(&[]), fields)
        }
        (t, Tv::List(_, items) | Tv::Set(_, items)) => {
            let et = match t {
                Some(Type::List(e) | Type::Set(e)) => Some(e.as_ref()),
                _ => None,
            };
            Json::Array(items.iter().map(|i| to_json(idl, et, i)).collect())
        }
        (t, Tv::Map(_, _, items)) => {
            let (kt, vt) = match t {
                Some(Type::Map(k, v)) => (Some(k.as_ref()), Some(v.as_ref())),
                _ => (None, None),
            };
            if items.iter().all(|(k, _)| matches!(k, Tv::Bin(_)))
                && !matches!(kt, Some(Type::Binary))
            {
                Json::Object(
                    items
                        .iter()
                        .map(|(k, v)| {
                            (
                                to_json(idl, kt, k).as_str().unwrap_or_default().to_owned(),
                                to_json(idl, vt, v),
                            )
                        })
                        .collect(),
                )
            } else {
                Json::Array(
                    items
                        .iter()
                        .map(|(k, v)| json!([to_json(idl, kt, k), to_json(idl, vt, v)]))
                        .collect(),
                )
            }
        }
    }
}

/// A struct's fields as an object keyed by declared name ("_<id>" for undeclared ids).
pub fn struct_json(idl: &Idl, decl: &[Field], fields: &[(i16, Tv)]) -> Json {
    let mut m = Map::new();
    for (id, v) in fields {
        match decl.iter().find(|f| f.id == *id) {
            Some(f) => m.insert(f.name.clone(), to_json(idl, Some(&f.ty), v)),
            None => m.insert(format!("_{id}"), to_json(idl, None, v)),
        };
    }
    Json::Object(m)
}

/// JSON as a wire value of type `t`.
pub fn from_json(idl: &Idl, t: &Type, j: &Json, depth: usize) -> Result<Tv> {
    ensure!(depth <= 64, "value nests too deeply");
    let int = |j: &Json| {
        j.as_i64()
            .with_context(|| format!("expected an integer, found {j}"))
    };
    Ok(match t {
        Type::Bool => Tv::Bool(
            j.as_bool()
                .with_context(|| format!("expected true or false, found {j}"))?,
        ),
        Type::Byte => Tv::Byte(i8::try_from(int(j)?).context("out of range for byte")?),
        Type::I16 => Tv::I16(i16::try_from(int(j)?).context("out of range for i16")?),
        Type::I32 => Tv::I32(i32::try_from(int(j)?).context("out of range for i32")?),
        Type::I64 => Tv::I64(int(j)?),
        Type::Double => Tv::Double(
            j.as_f64()
                .with_context(|| format!("expected a number, found {j}"))?,
        ),
        Type::String | Type::Binary => Tv::Bin(
            j.as_str()
                .with_context(|| format!("expected a string, found {j}"))?
                .as_bytes()
                .to_vec(),
        ),
        Type::Uuid => Tv::Uuid(
            *uuid::Uuid::parse_str(j.as_str().context("expected a UUID string")?)?.as_bytes(),
        ),
        Type::Enum(e) => Tv::I32(match j {
            Json::String(label) => idl
                .enum_value(e, label)
                .with_context(|| format!("{label:?} is not a value of {e}"))?,
            _ => i32::try_from(int(j)?)?,
        }),
        Type::List(e) | Type::Set(e) => {
            let items = j
                .as_array()
                .with_context(|| format!("expected an array, found {j}"))?;
            let v = items
                .iter()
                .map(|i| from_json(idl, e, i, depth + 1))
                .collect::<Result<Vec<_>>>()?;
            if matches!(t, Type::List(_)) {
                Tv::List(ttype(e), v)
            } else {
                Tv::Set(ttype(e), v)
            }
        }
        Type::Map(k, v) => {
            let pairs: Vec<(Tv, Tv)> = match j {
                Json::Object(m) => m
                    .iter()
                    .map(|(key, val)| {
                        Ok((
                            from_json(idl, k, &Json::String(key.clone()), depth + 1).or_else(
                                |_| {
                                    from_json(
                                        idl,
                                        k,
                                        &key.parse::<Json>().unwrap_or(Json::Null),
                                        depth + 1,
                                    )
                                },
                            )?,
                            from_json(idl, v, val, depth + 1)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
                Json::Array(a) => a
                    .iter()
                    .map(|p| {
                        let p = p
                            .as_array()
                            .filter(|p| p.len() == 2)
                            .context("map entries are [key, value]")?;
                        Ok((
                            from_json(idl, k, &p[0], depth + 1)?,
                            from_json(idl, v, &p[1], depth + 1)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
                _ => bail!("expected an object or [[key, value], ...], found {j}"),
            };
            Tv::Map(ttype(k), ttype(v), pairs)
        }
        Type::Struct(name) => {
            let decl = idl
                .structs
                .get(name)
                .with_context(|| format!("unknown struct {name}"))?;
            let o = j
                .as_object()
                .with_context(|| format!("expected an object for {name}, found {j}"))?;
            struct_from_json(idl, name, decl, o, depth)?
        }
    })
}

pub fn struct_from_json(
    idl: &Idl,
    name: &str,
    decl: &[Field],
    o: &Map<String, Json>,
    depth: usize,
) -> Result<Tv> {
    for k in o.keys() {
        ensure!(
            decl.iter().any(|f| &f.name == k),
            "{name} has no field {k:?}"
        );
    }
    let mut fields = Vec::new();
    for f in decl {
        match o.get(&f.name).filter(|v| !v.is_null()) {
            Some(v) => fields.push((
                f.id,
                from_json(idl, &f.ty, v, depth + 1)
                    .with_context(|| format!("{name}.{}", f.name))?,
            )),
            None => ensure!(!f.required, "{name}.{} is required", f.name),
        }
    }
    if idl.unions.iter().any(|u| u == name) {
        ensure!(fields.len() == 1, "union {name} takes exactly one field");
    }
    Ok(Tv::Struct(fields))
}
