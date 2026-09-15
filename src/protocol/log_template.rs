//! Log template system for standardized event and action logging
//!
//! Provides a template-based logging system where protocols define log formats
//! at INFO/DEBUG/TRACE levels. Templates use `{field}` interpolation syntax
//! to extract values from event/action data.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// Log level for template rendering
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Get the log level prefix for TUI display
    pub fn prefix(&self) -> &'static str {
        match self {
            LogLevel::Info => "[INFO]",
            LogLevel::Debug => "[DEBUG]",
            LogLevel::Trace => "[TRACE]",
        }
    }
}

/// Log template for generating log messages from event/action data
///
/// Templates use `{field}` syntax to interpolate values from JSON data.
/// Each level (info/debug/trace) can have its own template format.
///
/// # Example
/// ```rust,ignore
/// let template = LogTemplate::new()
///     .with_info("{client_ip} {method} {path} -> {status}")
///     .with_debug("HTTP {method} {path} from {client_ip}:{client_port}")
///     .with_trace("HTTP request: {json_pretty(.)}");
/// ```
#[derive(Clone, Debug, Default)]
pub struct LogTemplate {
    /// INFO level: One-liner access log style (protocol-specific)
    /// Example: "{client_ip} {method} {path} -> {status} ({response_bytes}B, {duration_ms}ms)"
    pub info: Option<String>,

    /// DEBUG level: Extra details + timing
    /// Example: "HTTP {method} {path} from {client_ip}:{client_port}, headers: {headers_len}"
    pub debug: Option<String>,

    /// TRACE level: Full request/response details
    /// Example: "Full HTTP request: {json_pretty(.)}"
    pub trace: Option<String>,
}

impl LogTemplate {
    /// Create a new empty log template
    pub fn new() -> Self {
        Self::default()
    }

    /// Set INFO level template
    ///
    /// Used for one-liner access log style messages.
    /// Example: "{client_ip} {method} {path} -> {status}"
    pub fn with_info(mut self, template: impl Into<String>) -> Self {
        self.info = Some(template.into());
        self
    }

    /// Set DEBUG level template
    ///
    /// Used for extra details including timing.
    /// Example: "HTTP {method} {path} from {client_ip}:{client_port}"
    pub fn with_debug(mut self, template: impl Into<String>) -> Self {
        self.debug = Some(template.into());
        self
    }

    /// Set TRACE level template
    ///
    /// Used for full payload details.
    /// Example: "Full request: {json_pretty(.)}"
    pub fn with_trace(mut self, template: impl Into<String>) -> Self {
        self.trace = Some(template.into());
        self
    }

    /// Get the template string for a given log level
    pub fn get_template(&self, level: LogLevel) -> Option<&str> {
        match level {
            LogLevel::Info => self.info.as_deref(),
            LogLevel::Debug => self.debug.as_deref(),
            LogLevel::Trace => self.trace.as_deref(),
        }
    }

    /// Render the template for a given log level with event/action data
    ///
    /// Returns None if no template is defined for the level.
    ///
    /// # Arguments
    /// * `level` - The log level to render
    /// * `data` - JSON data to interpolate into the template
    ///
    /// # Template Syntax
    /// - `{field}` - Simple field access
    /// - `{field.subfield}` - Nested field access (dot notation)
    /// - `{json(field)}` - Compact JSON of field
    /// - `{json_pretty(.)}` - Pretty-print entire data object
    /// - `{hex(field)}` - Hex-encode string field
    /// - `{field_len}` - Length of array/string/object field
    /// - `{preview(field)}` - First 100 chars with ellipsis
    /// - `{preview(field,50)}` - First N chars with ellipsis
    pub fn render(&self, level: LogLevel, data: &Value) -> Option<String> {
        self.get_template(level).map(|t| render_template(t, data))
    }

    /// Check if any template is defined
    pub fn has_any(&self) -> bool {
        self.info.is_some() || self.debug.is_some() || self.trace.is_some()
    }
}

/// Regex for matching template placeholders
static PLACEHOLDER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([^}]+)\}").expect("Invalid regex"));

/// Render a template string with JSON data
///
/// Supports:
/// - `{field}` - Simple field access
/// - `{field.subfield}` - Nested field access
/// - `{json(field)}` - Compact JSON
/// - `{json_pretty(field)}` or `{json_pretty(.)}` - Pretty JSON
/// - `{hex(field)}` - Hex-encode bytes
/// - `{field_len}` - Array/string length
/// - `{preview(field)}` - First 100 chars
/// - `{preview(field,N)}` - First N chars
fn render_template(template: &str, data: &Value) -> String {
    let mut result = template.to_string();

    for cap in PLACEHOLDER_REGEX.captures_iter(template) {
        let full_match = &cap[0];
        let placeholder = &cap[1];

        // The template is written by the protocol and is trusted. The *value* substituted into
        // it is not: it comes from the model's action or from the wire, and this renderer is
        // what puts it on a log line.
        //
        // A control character in it forges a log entry. `\r\nERROR forged` turns one line into
        // two, and the second is indistinguishable from something NetGet wrote — which is the
        // whole point of impersonating a device. LLDP, CDP and HSRP each fixed this locally in
        // their own fields; this is the one place that covers all 140 servers, including the
        // ones nobody has looked at yet.
        //
        // `line_field` substitutes a space rather than deleting, for the reason
        // `utils::sanitize` documents: deleting joins the two sides into one word, which reads
        // as a single legitimate value and is its own small lie. The JSON placeholder forms
        // escape control characters already, so this is a no-op for them.
        let replacement =
            crate::utils::sanitize::line_field(&render_placeholder(placeholder, data));
        result = result.replace(full_match, &replacement);
    }

    result
}

/// Render a single placeholder
fn render_placeholder(placeholder: &str, data: &Value) -> String {
    // Handle special functions
    if placeholder == "." {
        // Entire data object as compact JSON
        return serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    }

    if placeholder.starts_with("json_pretty(") && placeholder.ends_with(')') {
        // Pretty-print JSON
        let field = &placeholder[12..placeholder.len() - 1];
        let value = if field == "." {
            data.clone()
        } else {
            get_nested_value(data, field)
        };
        return serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string());
    }

    if placeholder.starts_with("json(") && placeholder.ends_with(')') {
        // Compact JSON
        let field = &placeholder[5..placeholder.len() - 1];
        let value = if field == "." {
            data.clone()
        } else {
            get_nested_value(data, field)
        };
        return serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    }

    if placeholder.starts_with("hex(") && placeholder.ends_with(')') {
        // Hex encode
        let field = &placeholder[4..placeholder.len() - 1];
        let value = get_nested_value(data, field);
        if let Some(s) = value.as_str() {
            return hex::encode(s.as_bytes());
        }
        return String::new();
    }

    if placeholder.starts_with("preview(") && placeholder.ends_with(')') {
        // Preview with optional length
        let inner = &placeholder[8..placeholder.len() - 1];
        let (field, max_len) = if let Some(comma_pos) = inner.find(',') {
            let field = inner[..comma_pos].trim();
            let len_str = inner[comma_pos + 1..].trim();
            let len = len_str.parse::<usize>().unwrap_or(100);
            (field, len)
        } else {
            (inner, 100)
        };

        let value = get_nested_value(data, field);
        let s = value_to_string(&value);
        // Must cut on a character boundary: `s` is network text or model output, and
        // `s.len()` is a byte count, so `&s[..max_len]` panics whenever the cut lands
        // inside a multi-byte character. This is shared infrastructure — `preview(...)`
        // is used by irc, pop3, xmpp and others, so the panic defeated those protocols'
        // own local fixes for the same class.
        return crate::utils::truncate_for_log(&s, max_len);
    }

    if placeholder.ends_with("_len") {
        // Length of field
        let field = &placeholder[..placeholder.len() - 4];
        let value = get_nested_value(data, field);
        return match &value {
            Value::String(s) => s.len().to_string(),
            Value::Array(a) => a.len().to_string(),
            Value::Object(o) => o.len().to_string(),
            _ => "0".to_string(),
        };
    }

    // Simple field access (supports dot notation)
    let value = get_nested_value(data, placeholder);
    value_to_string(&value)
}

/// Get a nested value from JSON using dot notation
///
/// Example: `get_nested_value(data, "headers.content_type")`
fn get_nested_value(data: &Value, path: &str) -> Value {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = data.clone();

    for part in parts {
        // Handle array index notation like "items[0]"
        if let Some(bracket_pos) = part.find('[') {
            let field = &part[..bracket_pos];
            let index_str = &part[bracket_pos + 1..part.len() - 1];
            if let Ok(index) = index_str.parse::<usize>() {
                current = current
                    .get(field)
                    .and_then(|v| v.get(index))
                    .cloned()
                    .unwrap_or(Value::Null);
                continue;
            }
        }

        current = current.get(part).cloned().unwrap_or(Value::Null);
    }

    current
}

/// Convert a JSON value to a display string
fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| String::new())
        }
    }
}
