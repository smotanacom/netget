//! Redact credentials from JSON before it is displayed or logged.
//!
//! Startup parameters and actions are echoed in two places an operator reads: the
//! `open_client` / `open_server` summary pushed to the dashboard and MCP status stream, and the
//! executor's per-action DEBUG line in `netget.log`. Both printed the RADIUS client's shared
//! `secret`, the MQTT client's `password` and anything else of that kind verbatim. The protocol
//! still receives the real value; only what is shown is replaced.
//!
//! Matching is by key name, case-insensitively, on a substring, so `secret`, `shared_secret`,
//! `password`, `bind_password` and `api_key` are all caught. A false positive costs a line of
//! display; a false negative prints a credential.

use serde_json::Value;

/// Substrings of a key name that mark its value as a credential.
pub const SENSITIVE_KEY_PARTS: &[&str] = &[
    "secret",
    "password",
    "passphrase",
    "passwd",
    "api_key",
    "apikey",
    "private_key",
    "credential",
];

/// What a redacted value is shown as.
pub const REDACTED: &str = "<redacted>";

/// Whether a key names a credential.
pub fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SENSITIVE_KEY_PARTS.iter().any(|part| key.contains(part))
}

/// Deepest nesting walked. `serde_json` refuses to parse past 128 levels, so a parsed value
/// never reaches this; a value built in code might, and past it the subtree is shown as
/// [`REDACTED`] whole rather than walked on a shrinking stack.
pub const MAX_REDACT_DEPTH: usize = 64;

/// A copy of `value` with every credential-named key's value replaced by [`REDACTED`], at any
/// depth up to [`MAX_REDACT_DEPTH`].
pub fn redact_sensitive(value: &Value) -> Value {
    redact_at(value, 0)
}

fn redact_at(value: &Value, depth: usize) -> Value {
    if depth > MAX_REDACT_DEPTH {
        return Value::String(REDACTED.to_string());
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let shown = if is_sensitive_key(k) && !v.is_null() {
                        Value::String(REDACTED.to_string())
                    } else {
                        redact_at(v, depth + 1)
                    };
                    (k.clone(), shown)
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| redact_at(v, depth + 1)).collect())
        }
        other => other.clone(),
    }
}
