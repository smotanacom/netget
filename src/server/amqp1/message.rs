//! AMQP 1.0 messages (part 3) to and from JSON: properties, application properties and the body
//! (amqp-value, data or amqp-sequence). Binary data is reported as text when it is UTF-8 and by
//! length otherwise; nothing raw reaches the handler.
use super::types::{self, Value};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value as Json};

const HEADER: u64 = 0x70;
const MESSAGE_ANNOTATIONS: u64 = 0x72;
const PROPERTIES: u64 = 0x73;
const APPLICATION_PROPERTIES: u64 = 0x74;
const DATA: u64 = 0x75;
const SEQUENCE: u64 = 0x76;
const VALUE: u64 = 0x77;
pub const MAX_MESSAGE: usize = 1024 * 1024;

const PROPERTY_NAMES: &[&str] = &[
    "message_id",
    "user_id",
    "to",
    "subject",
    "reply_to",
    "correlation_id",
    "content_type",
    "content_encoding",
    "absolute_expiry_time",
    "creation_time",
    "group_id",
    "group_sequence",
    "reply_to_group_id",
];

/// An AMQP value as JSON. Integers and floats become numbers, strings and symbols strings,
/// lists and arrays arrays, maps objects (keys stringified), binary is text or {"binary_length"}.
pub fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Ubyte(n) => json!(n),
        Value::Ushort(n) => json!(n),
        Value::Uint(n) => json!(n),
        Value::Ulong(n) => json!(n),
        Value::Byte(n) => json!(n),
        Value::Short(n) => json!(n),
        Value::Int(n) => json!(n),
        Value::Long(n) => json!(n),
        Value::Float(f) => json!(f),
        Value::Double(f) => json!(f),
        Value::Char(c) => json!(c.to_string()),
        Value::Timestamp(t) => json!({"timestamp_ms": t}),
        Value::Uuid(u) => json!(uuid::Uuid::from_bytes(*u).to_string()),
        Value::Binary(b) => match std::str::from_utf8(b) {
            Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => json!(s),
            _ => json!({"binary_length": b.len()}),
        },
        Value::String(s) | Value::Symbol(s) => json!(s),
        Value::Decimal(b) => json!({"decimal_bytes": b.len()}),
        Value::List(items) | Value::Array(items) => {
            Json::Array(items.iter().map(to_json).collect())
        }
        Value::Map(pairs) => Json::Object(
            pairs
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| to_json(k).to_string()),
                        to_json(v),
                    )
                })
                .collect(),
        ),
        Value::Described(_, inner) => to_json(inner),
    }
}

/// JSON as an AMQP value: integers become long (or ulong beyond i64), floats double, strings
/// string, arrays list, objects map with string keys.
pub fn from_json(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Value::Long(i),
            (None, Some(u)) => Value::Ulong(u),
            _ => Value::Double(n.as_f64().unwrap_or(0.0)),
        },
        Json::String(s) => Value::String(s.clone()),
        Json::Array(a) => Value::List(a.iter().map(from_json).collect()),
        Json::Object(m) => Value::Map(
            m.iter()
                .map(|(k, v)| (Value::String(k.clone()), from_json(v)))
                .collect(),
        ),
    }
}

/// Decode a message's sections.
pub fn decode(b: &[u8]) -> Result<Json> {
    ensure!(
        b.len() <= MAX_MESSAGE,
        "message over {} KiB",
        MAX_MESSAGE / 1024
    );
    let mut out = json!({});
    let mut data: Vec<u8> = Vec::new();
    let mut data_sections = 0;
    let mut sequence: Vec<Json> = Vec::new();
    for section in types::decode_all(b)? {
        let Value::Described(_, inner) = &section else {
            bail!("a message section is not a described value")
        };
        match section.descriptor() {
            Some(HEADER) => {
                let f = section.fields();
                out["header"] = json!({"durable": f.first().and_then(Value::as_bool), "priority": f.get(1).and_then(Value::as_u64), "ttl_ms": f.get(2).and_then(Value::as_u64)});
            }
            Some(PROPERTIES) => {
                let mut p = Map::new();
                for (name, v) in PROPERTY_NAMES.iter().zip(section.fields()) {
                    if !v.is_null() {
                        p.insert((*name).into(), to_json(v));
                    }
                }
                out["properties"] = Json::Object(p);
            }
            Some(APPLICATION_PROPERTIES) => out["application_properties"] = to_json(inner),
            Some(MESSAGE_ANNOTATIONS) => out["message_annotations"] = to_json(inner),
            Some(DATA) => {
                let Value::Binary(chunk) = inner.as_ref() else {
                    bail!("data section is not binary")
                };
                data.extend(chunk);
                data_sections += 1;
            }
            Some(SEQUENCE) => {
                if let Value::List(items) = inner.as_ref() {
                    sequence.extend(items.iter().map(to_json));
                }
            }
            Some(VALUE) => out["body"] = to_json(inner),
            _ => {}
        }
    }
    if data_sections > 0 {
        out["body_type"] = json!("data");
        out["body"] = to_json(&Value::Binary(data));
    } else if !sequence.is_empty() {
        out["body_type"] = json!("sequence");
        out["body"] = Json::Array(sequence);
    } else {
        out["body_type"] = json!("value");
    }
    Ok(out)
}

/// Encode a message from its JSON form: optional `properties`, `application_properties` (string
/// keys, simple values) and `body`, which is an amqp-value unless `body_type` is "data" (the
/// body text as UTF-8 bytes).
pub fn encode(m: &Json) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(p) = m.get("properties").filter(|p| !p.is_null()) {
        let p = p.as_object().context("properties is an object")?;
        for k in p.keys() {
            ensure!(
                PROPERTY_NAMES.contains(&k.as_str()),
                "unknown property {k:?}; known: {}",
                PROPERTY_NAMES.join(", ")
            );
        }
        let fields: Vec<Value> = PROPERTY_NAMES
            .iter()
            .map(|n| match p.get(*n) {
                None | Some(Json::Null) => Value::Null,
                Some(v) if matches!(*n, "absolute_expiry_time" | "creation_time") => {
                    Value::Timestamp(v.as_i64().unwrap_or(0))
                }
                Some(v) if *n == "group_sequence" => Value::Uint(v.as_u64().unwrap_or(0) as u32),
                Some(v) if *n == "user_id" => {
                    Value::Binary(v.as_str().unwrap_or_default().as_bytes().to_vec())
                }
                Some(v) if matches!(*n, "content_type" | "content_encoding") => {
                    Value::Symbol(v.as_str().unwrap_or_default().to_owned())
                }
                Some(v) => from_json(v),
            })
            .collect();
        let last = fields
            .iter()
            .rposition(|f| !f.is_null())
            .map(|i| i + 1)
            .unwrap_or(0);
        types::encode_into(
            &Value::described(PROPERTIES, fields[..last].to_vec()),
            &mut out,
        );
    }
    if let Some(a) = m.get("application_properties").filter(|a| !a.is_null()) {
        let a = a
            .as_object()
            .context("application_properties is an object")?;
        ensure!(
            a.values().all(|v| !v.is_array() && !v.is_object()),
            "application_properties values are simple (no lists or maps)"
        );
        types::encode_into(
            &Value::Described(
                Box::new(Value::Ulong(APPLICATION_PROPERTIES)),
                Box::new(from_json(&Json::Object(a.clone()))),
            ),
            &mut out,
        );
    }
    let body = m.get("body").unwrap_or(&Json::Null);
    match m.get("body_type").and_then(Json::as_str).unwrap_or("value") {
        "data" => {
            let text = body.as_str().context("a data body is text")?;
            types::encode_into(
                &Value::Described(
                    Box::new(Value::Ulong(DATA)),
                    Box::new(Value::Binary(text.as_bytes().to_vec())),
                ),
                &mut out,
            );
        }
        "value" => types::encode_into(
            &Value::Described(Box::new(Value::Ulong(VALUE)), Box::new(from_json(body))),
            &mut out,
        ),
        other => bail!("body_type is value or data, not {other:?}"),
    }
    ensure!(
        out.len() <= MAX_MESSAGE,
        "message over {} KiB",
        MAX_MESSAGE / 1024
    );
    Ok(out)
}
