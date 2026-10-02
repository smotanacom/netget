//! Checked conversion of JSON action values to protocol fields.

use anyhow::{Context, Result};
use serde_json::Value;

/// Preserve an absent/null optional field's default; reject malformed or overflowing values.
pub fn number<T: TryFrom<u64> + Copy>(data: &Value, key: &str, default: T) -> Result<T> {
    match data.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => {
            let raw = value
                .as_u64()
                .with_context(|| format!("'{key}' must be an unsigned integer"))?;
            T::try_from(raw)
                .map_err(|_| anyhow::anyhow!("'{key}' value {raw} exceeds its wire field"))
        }
    }
}

/// A wire byte array cannot silently omit invalid elements or wrap values modulo 256.
pub fn bytes(values: &[Value]) -> Result<Vec<u8>> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_u64()
                .and_then(|value| u8::try_from(value).ok())
                .with_context(|| {
                    format!("byte array element {index} must be an integer in 0..=255")
                })
        })
        .collect()
}
