//! Typed model boundary; binary protobuf and JSON bytes never enter an event.
use super::{codec, proto::gnmi as pb};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write};
use tonic::Status;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Path {
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub elem: Vec<Element>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Element {
    pub name: String,
    #[serde(default)]
    pub key: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Value {
    String(String),
    Int(String),
    Uint(String),
    Bool(bool),
    Double(f64),
    Decimal(Decimal),
    Leaflist(Vec<Value>),
    Json(serde_json::Value),
    JsonIetf(serde_json::Value),
    Ascii(String),
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Decimal {
    pub digits: String,
    pub precision: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub path: Path,
    pub value: Value,
    #[serde(default)]
    pub duplicates: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub timestamp: String,
    #[serde(default)]
    pub prefix: Path,
    #[serde(default)]
    pub update: Vec<Update>,
    #[serde(default)]
    pub delete: Vec<Path>,
    #[serde(default)]
    pub atomic: bool,
}
fn limit(ok: bool) -> Result<(), Status> {
    if ok {
        Ok(())
    } else {
        Err(Status::resource_exhausted("gNMI typed value/path bound"))
    }
}
fn text(value: &str, max: usize) -> Result<(), Status> {
    limit(value.len() <= max)?;
    if value.contains('\0') {
        return Err(Status::invalid_argument("NUL in gNMI text"));
    }
    Ok(())
}
impl Path {
    pub fn check(&self) -> Result<(), Status> {
        text(&self.origin, 256)?;
        text(&self.target, 256)?;
        limit(self.elem.len() <= 32)?;
        for elem in &self.elem {
            text(&elem.name, 128)?;
            if elem.name.is_empty() {
                return Err(Status::invalid_argument("empty path element name"));
            }
            limit(elem.key.len() <= 8)?;
            for (key, value) in &elem.key {
                text(key, 128)?;
                text(value, 1024)?;
                if key.is_empty() {
                    return Err(Status::invalid_argument("empty path key"));
                }
            }
        }
        Ok(())
    }
    pub fn into_proto(self) -> Result<pb::Path, Status> {
        self.check()?;
        Ok(pb::Path {
            element: vec![],
            origin: self.origin,
            target: self.target,
            elem: self
                .elem
                .into_iter()
                .map(|e| pb::PathElem {
                    name: e.name,
                    key: e.key.into_iter().collect(),
                })
                .collect(),
        })
    }
    pub fn from_proto(path: Option<pb::Path>) -> Result<Self, Status> {
        let path = path.unwrap_or_default();
        if !path.element.is_empty() {
            return Err(Status::unimplemented(
                "deprecated string paths are excluded; use Path.elem",
            ));
        }
        let result = Self {
            origin: path.origin,
            target: path.target,
            elem: path
                .elem
                .into_iter()
                .map(|e| Element {
                    name: e.name,
                    key: e.key.into_iter().collect(),
                })
                .collect(),
        };
        result.check()?;
        Ok(result)
    }
}
struct Writer(Vec<u8>);
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > codec::MAX_VALUE_BYTES {
            return Err(std::io::Error::other("JSON bound"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn json_bytes(value: &serde_json::Value) -> Result<Vec<u8>, Status> {
    let mut writer = Writer(Vec::new());
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| Status::resource_exhausted("gNMI JSON exceeds 64 KiB"))?;
    super::json::parse(&writer.0)?;
    Ok(writer.0)
}
impl Value {
    pub fn into_proto(self) -> Result<pb::TypedValue, Status> {
        use pb::typed_value::Value as V;
        let value = match self {
            Self::String(v) => {
                limit(v.len() <= codec::MAX_VALUE_BYTES)?;
                V::StringVal(v)
            }
            Self::Ascii(v) => {
                limit(v.len() <= codec::MAX_VALUE_BYTES)?;
                if !v.is_ascii() {
                    return Err(Status::invalid_argument("ASCII value is not ASCII"));
                }
                V::AsciiVal(v)
            }
            Self::Int(v) => V::IntVal(v.parse().map_err(|_| {
                Status::invalid_argument("int value must be an i64 decimal string")
            })?),
            Self::Uint(v) => V::UintVal(v.parse().map_err(|_| {
                Status::invalid_argument("uint value must be a u64 decimal string")
            })?),
            Self::Bool(v) => V::BoolVal(v),
            Self::Double(v) => {
                if !v.is_finite() {
                    return Err(Status::invalid_argument("nonfinite value"));
                }
                V::DoubleVal(v)
            }
            Self::Decimal(v) => {
                if v.precision > 18 {
                    return Err(Status::invalid_argument("decimal precision exceeds 18"));
                }
                V::DecimalVal(pb::Decimal64 {
                    digits: v.digits.parse().map_err(|_| {
                        Status::invalid_argument("decimal digits must be an i64 string")
                    })?,
                    precision: v.precision,
                })
            }
            Self::Json(v) => V::JsonVal(json_bytes(&v)?),
            Self::JsonIetf(v) => V::JsonIetfVal(json_bytes(&v)?),
            Self::Leaflist(values) => {
                limit(values.len() <= 64)?;
                if values
                    .iter()
                    .any(|v| matches!(v, Self::Leaflist(_) | Self::Json(_) | Self::JsonIetf(_)))
                {
                    return Err(Status::invalid_argument(
                        "leaf-list elements must be scalars",
                    ));
                }
                V::LeaflistVal(pb::ScalarArray {
                    element: values
                        .into_iter()
                        .map(Self::into_proto)
                        .collect::<Result<_, _>>()?,
                })
            }
        };
        Ok(pb::TypedValue { value: Some(value) })
    }
    pub fn from_proto(value: pb::TypedValue) -> Result<Self, Status> {
        use pb::typed_value::Value as V;
        let result = match value
            .value
            .ok_or_else(|| Status::invalid_argument("missing TypedValue member"))?
        {
            V::StringVal(v) => Self::String(v),
            V::AsciiVal(v) => Self::Ascii(v),
            V::IntVal(v) => Self::Int(v.to_string()),
            V::UintVal(v) => Self::Uint(v.to_string()),
            V::BoolVal(v) => Self::Bool(v),
            V::DoubleVal(v) => Self::Double(v),
            V::FloatVal(v) => Self::Double(f64::from(v)),
            V::DecimalVal(v) => Self::Decimal(Decimal {
                digits: v.digits.to_string(),
                precision: v.precision,
            }),
            V::JsonVal(v) => Self::Json(super::json::parse(&v)?),
            V::JsonIetfVal(v) => Self::JsonIetf(super::json::parse(&v)?),
            V::LeaflistVal(v) => Self::Leaflist(
                v.element
                    .into_iter()
                    .map(Self::from_proto)
                    .collect::<Result<_, _>>()?,
            ),
            V::BytesVal(_) | V::AnyVal(_) | V::ProtoBytes(_) => {
                return Err(Status::unimplemented("opaque values are excluded"))
            }
        };
        // Enforce semantic constraints even for programmatically constructed messages.
        result.clone().into_proto()?;
        Ok(result)
    }
}
impl Update {
    pub fn into_proto(self) -> Result<pb::Update, Status> {
        Ok(pb::Update {
            path: Some(self.path.into_proto()?),
            value: None,
            val: Some(self.value.into_proto()?),
            duplicates: self.duplicates,
        })
    }
    pub fn from_proto(v: pb::Update) -> Result<Self, Status> {
        if v.value.is_some() {
            return Err(Status::unimplemented("legacy values are excluded"));
        }
        Ok(Self {
            path: Path::from_proto(v.path)?,
            value: Value::from_proto(
                v.val
                    .ok_or_else(|| Status::invalid_argument("missing update value"))?,
            )?,
            duplicates: v.duplicates,
        })
    }
}
impl Notification {
    pub fn into_proto(self) -> Result<pb::Notification, Status> {
        limit(self.update.len() <= 256 && self.delete.len() <= 256)?;
        Ok(pb::Notification {
            timestamp: self.timestamp.parse().map_err(|_| {
                Status::invalid_argument("timestamp must be i64 nanosecond decimal string")
            })?,
            prefix: Some(self.prefix.into_proto()?),
            update: self
                .update
                .into_iter()
                .map(Update::into_proto)
                .collect::<Result<_, _>>()?,
            delete: self
                .delete
                .into_iter()
                .map(Path::into_proto)
                .collect::<Result<_, _>>()?,
            atomic: self.atomic,
        })
    }
    pub fn from_proto(v: pb::Notification) -> Result<Self, Status> {
        limit(v.update.len() <= 256 && v.delete.len() <= 256)?;
        Ok(Self {
            timestamp: v.timestamp.to_string(),
            prefix: Path::from_proto(v.prefix)?,
            update: v
                .update
                .into_iter()
                .map(Update::from_proto)
                .collect::<Result<_, _>>()?,
            delete: v
                .delete
                .into_iter()
                .map(|p| Path::from_proto(Some(p)))
                .collect::<Result<_, _>>()?,
            atomic: v.atomic,
        })
    }
}
pub fn encoding(name: &str) -> Result<i32, Status> {
    Ok(match name {
        "JSON" => 0,
        "PROTO" => 2,
        "ASCII" => 3,
        "JSON_IETF" => 4,
        _ => return Err(Status::unimplemented("unsupported gNMI encoding")),
    })
}
pub fn encoding_name(value: i32) -> Result<&'static str, Status> {
    match value {
        0 => Ok("JSON"),
        2 => Ok("PROTO"),
        3 => Ok("ASCII"),
        4 => Ok("JSON_IETF"),
        _ => Err(Status::unimplemented("unsupported gNMI encoding")),
    }
}
pub fn notification_encoding(notification: &pb::Notification, encoding: i32) -> Result<(), Status> {
    encoding_name(encoding)?;
    for update in &notification.update {
        use pb::typed_value::Value as V;
        let value = update
            .val
            .as_ref()
            .and_then(|v| v.value.as_ref())
            .ok_or_else(|| Status::invalid_argument("missing typed value"))?;
        if !match encoding {
            0 => matches!(value, V::JsonVal(_)),
            4 => matches!(value, V::JsonIetfVal(_)),
            3 => matches!(value, V::AsciiVal(_)),
            2 => !matches!(
                value,
                V::JsonVal(_)
                    | V::JsonIetfVal(_)
                    | V::AsciiVal(_)
                    | V::BytesVal(_)
                    | V::AnyVal(_)
                    | V::ProtoBytes(_)
            ),
            _ => false,
        } {
            return Err(Status::invalid_argument(
                "notification value does not match requested encoding",
            ));
        }
    }
    Ok(())
}
pub fn checked<T: prost::Message + codec::WireName>(v: T) -> Result<T, Status> {
    if v.encoded_len() > codec::MAX_MESSAGE_BYTES {
        return Err(Status::resource_exhausted("gNMI message exceeds 1 MiB"));
    }
    codec::validate_wire(T::NAME, &v.encode_to_vec())?;
    Ok(v)
}
