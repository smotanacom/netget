//! Bounded conversion of the field-name JSON representation used by both gRPC peers.
//!
//! These bounds also apply to values constructed in memory, independently of
//! serde/prost parser recursion limits. All inputs are borrowed; rejected input
//! remains owned by its caller.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use prost_reflect::{DynamicMessage, FieldDescriptor, Kind, MapKey, MessageDescriptor, Value};
use serde_json::Value as Json;

/// Root depth is zero; every message field, map value, or list element adds one.
pub const MAX_VALUE_DEPTH: usize = 32;
pub const MAX_VALUE_NODES: usize = 100_000;
/// Retained-content accounting, including node/key overhead; not serialized wire size.
pub const MAX_VALUE_BYTES: usize = 8 * 1024 * 1024;
pub const VALUE_NODE_OVERHEAD_BYTES: usize = 64;
const MAP_KEY_OVERHEAD_BYTES: usize = 32;

/// A resource refusal, distinct from a value that does not match the schema.
#[derive(Debug)]
pub struct ValueLimitExceeded {
    pub dimension: &'static str,
    pub limit: usize,
}
impl std::fmt::Display for ValueLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "gRPC value exceeds {} limit {}",
            self.dimension, self.limit
        )
    }
}
impl std::error::Error for ValueLimitExceeded {}

#[derive(Default)]
struct Budget {
    nodes: usize,
    bytes: usize,
}
impl Budget {
    fn node(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_VALUE_DEPTH {
            return Err(ValueLimitExceeded {
                dimension: "depth",
                limit: MAX_VALUE_DEPTH,
            }
            .into());
        }
        self.children(1)?;
        self.nodes += 1;
        self.bytes(VALUE_NODE_OVERHEAD_BYTES)
    }

    fn children(&self, count: usize) -> Result<()> {
        if count > MAX_VALUE_NODES.saturating_sub(self.nodes) {
            return Err(ValueLimitExceeded {
                dimension: "nodes",
                limit: MAX_VALUE_NODES,
            }
            .into());
        }
        Ok(())
    }

    fn bytes(&mut self, count: usize) -> Result<()> {
        if count > MAX_VALUE_BYTES.saturating_sub(self.bytes) {
            return Err(ValueLimitExceeded {
                dimension: "bytes",
                limit: MAX_VALUE_BYTES,
            }
            .into());
        }
        self.bytes += count;
        Ok(())
    }

    fn key(&mut self, key: &str) -> Result<()> {
        self.bytes(MAP_KEY_OVERHEAD_BYTES)?;
        self.bytes(key.len())
    }
}

/// Convert one complete message, refusing invalid values rather than substituting defaults.
pub fn json_to_dynamic_message(
    json: &Json,
    descriptor: &MessageDescriptor,
) -> Result<DynamicMessage> {
    json_to_message(json, descriptor, 0, &mut Budget::default())
}

/// Convert the complete message without relying on the protobuf decoder's recursion cap.
pub fn dynamic_message_to_json(message: &DynamicMessage) -> Result<Json> {
    message_to_json(message, 0, &mut Budget::default())
}

/// Convert a reflected value with the same limits used for complete messages.
pub fn proto_value_to_json(value: &Value) -> Result<Json> {
    value_to_json(value, 0, &mut Budget::default())
}

fn json_to_message(
    json: &Json,
    descriptor: &MessageDescriptor,
    depth: usize,
    budget: &mut Budget,
) -> Result<DynamicMessage> {
    budget.node(depth)?;
    let object = json
        .as_object()
        .with_context(|| format!("message {} requires a JSON object", descriptor.full_name()))?;
    budget.children(object.len())?;
    let mut message = DynamicMessage::new(descriptor.clone());
    let mut oneofs = std::collections::HashSet::new();
    for (name, value) in object {
        budget.key(name)?;
        let field = descriptor.get_field_by_name(name).with_context(|| {
            format!(
                "unknown field {name:?} in message {}",
                descriptor.full_name()
            )
        })?;
        if let Some(oneof) = field.containing_oneof() {
            if !oneofs.insert(oneof.full_name().to_owned()) {
                bail!("multiple values supplied for oneof {}", oneof.full_name());
            }
        }
        let value = json_to_field_value(value, &field, depth + 1, budget)?;
        message
            .try_set_field(&field, value)
            .with_context(|| format!("invalid value for field {}", field.full_name()))?;
    }
    Ok(message)
}

fn json_to_field_value(
    json: &Json,
    field: &FieldDescriptor,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value> {
    if field.is_map() {
        budget.node(depth)?;
        let Kind::Message(entry) = field.kind() else {
            bail!("map field has no entry message")
        };
        let key_field = entry.get_field(1).context("map entry has no key field")?;
        let value_field = entry.get_field(2).context("map entry has no value field")?;
        let object = json
            .as_object()
            .with_context(|| format!("field {} requires a map object", field.full_name()))?;
        budget.children(object.len())?;
        let mut map = std::collections::HashMap::new();
        for (key, value) in object {
            budget.key(key)?;
            let key = parse_map_key(key, key_field.kind())?;
            if map.contains_key(&key) {
                bail!("duplicate protobuf map key in field {}", field.full_name());
            }
            map.insert(
                key,
                json_to_proto_value(value, &value_field, depth + 1, budget)?,
            );
        }
        return Ok(Value::Map(map));
    }
    if field.is_list() {
        budget.node(depth)?;
        let array = json
            .as_array()
            .with_context(|| format!("field {} requires an array", field.full_name()))?;
        budget.children(array.len())?;
        let mut list = Vec::new();
        for item in array {
            list.push(json_to_proto_value(item, field, depth + 1, budget)?);
        }
        return Ok(Value::List(list));
    }
    json_to_proto_value(json, field, depth, budget)
}

fn json_to_proto_value(
    json: &Json,
    field: &FieldDescriptor,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value> {
    let kind = field.kind();
    if let Kind::Message(descriptor) = kind {
        return Ok(Value::Message(json_to_message(
            json,
            &descriptor,
            depth,
            budget,
        )?));
    }
    budget.node(depth)?;
    let invalid = || {
        format!(
            "value does not match field {} ({:?})",
            field.full_name(),
            field.kind()
        )
    };
    Ok(match kind {
        Kind::Bool => Value::Bool(json.as_bool().with_context(invalid)?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => Value::I32(
            i32::try_from(json.as_i64().with_context(invalid)?)
                .context("integer does not fit int32")?,
        ),
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => {
            Value::I64(json.as_i64().with_context(invalid)?)
        }
        Kind::Uint32 | Kind::Fixed32 => Value::U32(
            u32::try_from(json.as_u64().with_context(invalid)?)
                .context("integer does not fit uint32")?,
        ),
        Kind::Uint64 | Kind::Fixed64 => Value::U64(json.as_u64().with_context(invalid)?),
        Kind::Float => {
            let value = json.as_f64().with_context(invalid)?;
            if !value.is_finite() || value.abs() > f32::MAX as f64 {
                bail!(
                    "field {} is outside the finite float range",
                    field.full_name()
                );
            }
            let narrowed = value as f32;
            if value != 0.0 && narrowed == 0.0 {
                bail!(
                    "field {} underflows the nonzero float range",
                    field.full_name()
                );
            }
            Value::F32(narrowed)
        }
        Kind::Double => {
            let value = json.as_f64().with_context(invalid)?;
            if !value.is_finite() {
                bail!("field {} requires a finite number", field.full_name());
            }
            Value::F64(value)
        }
        Kind::String => {
            let value = json.as_str().with_context(invalid)?;
            budget.bytes(value.len())?;
            Value::String(value.to_owned())
        }
        Kind::Bytes => {
            let value = json.as_str().with_context(invalid)?;
            // Base64 input is at least as large as decoded output. Charge it before decoding.
            budget.bytes(value.len())?;
            Value::Bytes(
                STANDARD
                    .decode(value)
                    .context("invalid protobuf bytes base64")?
                    .into(),
            )
        }
        Kind::Enum(descriptor) => {
            let number = if let Some(number) = json.as_i64() {
                i32::try_from(number).context("enum number does not fit int32")?
            } else if let Some(name) = json.as_str() {
                budget.bytes(name.len())?;
                descriptor
                    .get_value_by_name(name)
                    .with_context(|| {
                        format!("unknown value {name:?} for enum {}", descriptor.full_name())
                    })?
                    .number()
            } else {
                bail!("enum {} requires a name or integer", descriptor.full_name())
            };
            if descriptor.get_value(number).is_none() {
                bail!("unknown value {number} for enum {}", descriptor.full_name());
            }
            Value::EnumNumber(number)
        }
        Kind::Message(_) => unreachable!("messages handled before charging scalar nodes"),
    })
}

fn parse_map_key(key: &str, kind: Kind) -> Result<MapKey> {
    Ok(match kind {
        Kind::String => MapKey::String(key.to_owned()),
        Kind::Bool => MapKey::Bool(key.parse().context("map key is not a boolean")?),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            MapKey::I32(key.parse().context("map key is not int32")?)
        }
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => {
            MapKey::I64(key.parse().context("map key is not int64")?)
        }
        Kind::Uint32 | Kind::Fixed32 => MapKey::U32(key.parse().context("map key is not uint32")?),
        Kind::Uint64 | Kind::Fixed64 => MapKey::U64(key.parse().context("map key is not uint64")?),
        _ => bail!("unsupported protobuf map key kind"),
    })
}

fn message_to_json(message: &DynamicMessage, depth: usize, budget: &mut Budget) -> Result<Json> {
    budget.node(depth)?;
    let mut object = serde_json::Map::new();
    // Iterate populated values only, borrowing children instead of cloning their trees.
    for (field, value) in message.fields() {
        budget.key(field.name())?;
        let json = value_to_json(value, depth + 1, budget)?;
        object.insert(field.name().to_owned(), json);
    }
    Ok(Json::Object(object))
}

fn value_to_json(value: &Value, depth: usize, budget: &mut Budget) -> Result<Json> {
    if let Value::Message(message) = value {
        return message_to_json(message, depth, budget);
    }
    budget.node(depth)?;
    Ok(match value {
        Value::Bool(value) => Json::Bool(*value),
        Value::I32(value) => Json::Number((*value).into()),
        Value::I64(value) => Json::Number((*value).into()),
        Value::U32(value) => Json::Number((*value).into()),
        Value::U64(value) => Json::Number((*value).into()),
        Value::F32(value) => Json::Number(
            serde_json::Number::from_f64(*value as f64)
                .context("non-finite protobuf float cannot be represented in field-name JSON")?,
        ),
        Value::F64(value) => Json::Number(
            serde_json::Number::from_f64(*value)
                .context("non-finite protobuf double cannot be represented in field-name JSON")?,
        ),
        Value::String(value) => {
            budget.bytes(value.len())?;
            Json::String(value.clone())
        }
        Value::Bytes(value) => {
            let length = value
                .len()
                .checked_add(2)
                .and_then(|n| (n / 3).checked_mul(4))
                .ok_or(ValueLimitExceeded {
                    dimension: "bytes",
                    limit: MAX_VALUE_BYTES,
                })?;
            budget.bytes(length)?;
            Json::String(STANDARD.encode(value))
        }
        Value::EnumNumber(value) => Json::Number((*value).into()),
        Value::List(values) => {
            budget.children(values.len())?;
            let mut array = Vec::new();
            for value in values {
                array.push(value_to_json(value, depth + 1, budget)?);
            }
            Json::Array(array)
        }
        Value::Map(values) => {
            budget.children(values.len())?;
            let mut object = serde_json::Map::new();
            for (key, value) in values {
                // Charge string keys before cloning. Numeric keys have a fixed small bound.
                if let MapKey::String(key) = key {
                    budget.key(key)?;
                }
                let key_text = map_key_to_string(key);
                if !matches!(key, MapKey::String(_)) {
                    budget.key(&key_text)?;
                }
                if object.contains_key(&key_text) {
                    bail!("distinct protobuf map keys have the same JSON representation");
                }
                let json = value_to_json(value, depth + 1, budget)?;
                object.insert(key_text, json);
            }
            Json::Object(object)
        }
        Value::Message(_) => unreachable!("messages handled before charging scalar nodes"),
    })
}

fn map_key_to_string(key: &MapKey) -> String {
    match key {
        MapKey::Bool(value) => value.to_string(),
        MapKey::I32(value) => value.to_string(),
        MapKey::I64(value) => value.to_string(),
        MapKey::U32(value) => value.to_string(),
        MapKey::U64(value) => value.to_string(),
        MapKey::String(value) => value.clone(),
    }
}
