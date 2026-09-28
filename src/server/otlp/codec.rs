//! OTLP/HTTP payloads: decoding an export request into the summary the model sees, and encoding
//! every response the receiver sends.
//!
//! Pure functions, shared by the server, the tests and nothing else. **The model never sees a
//! payload**: it gets counts, the service name, a few resource attributes and the first few span
//! names, metric names or log bodies, each truncated. It never writes one either: every
//! response body — the empty full-success message, a partial success, a `google.rpc.Status` — is
//! encoded here, in the encoding the request used.
//!
//! Protobuf is decoded with the OpenTelemetry project's own generated types
//! (`opentelemetry-proto`, prost). prost stops at 100 levels of nesting, so a depth bomb in an
//! `AnyValue` is a decode error, not a stack overflow. JSON is parsed as a `serde_json::Value`
//! (recursion limit 128) and walked by hand rather than through the crate's serde types, because
//! OTLP/JSON senders differ in ways a strict typed decode would refuse (64-bit integers as
//! numbers or strings, enum values as numbers or names); keys are accepted in lowerCamelCase, as
//! the specification writes them, and in the proto's own snake_case. Neither walk recurses into
//! attribute values: an array or map is summarised by its length.

use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsPartialSuccess, ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use prost::Message;
use serde_json::{json, Map, Value};
use std::io::Read;

/// The largest request body the receiver reads, after decompression: 4 MiB, the default
/// message limit of the gRPC servers OTLP receivers are built on and what SDK exporters batch
/// under. A gzip body is held to the same number once inflated, so a small compressed body
/// cannot expand past it.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// How much of an export the model is shown.
pub const MAX_NAMES: usize = 10;
pub const MAX_LOG_BODIES: usize = 5;
pub const MAX_ATTRIBUTES: usize = 20;
pub const MAX_SERVICES: usize = 5;
const NAME_BYTES: usize = 100;
const VALUE_BYTES: usize = 200;

/// The three signals and their paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Metrics,
    Logs,
}

impl Signal {
    pub fn from_path(path: &str) -> Option<Signal> {
        match path {
            "/v1/traces" => Some(Signal::Traces),
            "/v1/metrics" => Some(Signal::Metrics),
            "/v1/logs" => Some(Signal::Logs),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Metrics => "metrics",
            Signal::Logs => "logs",
        }
    }

    /// What a partial success counts as rejected: spans, data points or log records.
    pub fn items(self) -> &'static str {
        match self {
            Signal::Traces => "spans",
            Signal::Metrics => "data points",
            Signal::Logs => "log records",
        }
    }
}

/// The two encodings OTLP/HTTP defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Protobuf,
    Json,
}

impl Encoding {
    /// From a `Content-Type` header, parameters ignored.
    pub fn from_content_type(value: &str) -> Option<Encoding> {
        match value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "application/x-protobuf" => Some(Encoding::Protobuf),
            "application/json" => Some(Encoding::Json),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Encoding::Protobuf => "application/x-protobuf",
            Encoding::Json => "application/json",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Protobuf => "protobuf",
            Encoding::Json => "json",
        }
    }
}

/// Why a body could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// Inflated past [`MAX_BODY_BYTES`].
    TooLarge,
    /// Not valid gzip.
    BadGzip,
}

/// Inflate a gzip body, refusing it as soon as it passes `limit` bytes. Never allocates more than
/// `limit + 1` bytes of output.
pub fn gunzip_bounded(body: &[u8], limit: usize) -> Result<Vec<u8>, BodyError> {
    let mut out = Vec::new();
    let mut decoder = flate2::read::MultiGzDecoder::new(body).take(limit as u64 + 1);
    decoder
        .read_to_end(&mut out)
        .map_err(|_| BodyError::BadGzip)?;
    if out.len() > limit {
        return Err(BodyError::TooLarge);
    }
    Ok(out)
}

/// What the model is told about one export.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub resource_count: usize,
    pub services: Vec<String>,
    pub resource_attributes: Map<String, Value>,
    /// Spans, data points or log records.
    pub item_count: usize,
    /// Metrics, for the metrics signal.
    pub metric_count: usize,
    /// Spans whose status is ERROR, or log records at severity ERROR or above.
    pub error_count: usize,
    /// Span names, metric names or log bodies, in order.
    pub names: Vec<String>,
}

impl Summary {
    fn add_resource(&mut self, attributes: Vec<(String, String)>) {
        self.resource_count += 1;
        for (key, value) in &attributes {
            if key == "service.name"
                && !self.services.contains(value)
                && self.services.len() < MAX_SERVICES
            {
                self.services.push(value.clone());
            }
        }
        if self.resource_count == 1 {
            for (key, value) in attributes.into_iter().take(MAX_ATTRIBUTES) {
                self.resource_attributes.insert(
                    crate::utils::truncate_for_llm(&key, NAME_BYTES),
                    json!(value),
                );
            }
        }
    }

    fn add_name(&mut self, name: &str, max: usize, bytes: usize) {
        if self.names.len() < max {
            self.names.push(crate::utils::truncate_for_llm(
                &crate::utils::sanitize::line_field(name),
                bytes,
            ));
        }
    }

    /// The event data for `otlp_export`, without `answer_with`.
    pub fn to_event(
        &self,
        signal: Signal,
        encoding: Encoding,
        compressed: bool,
        body_bytes: usize,
    ) -> Value {
        let mut data = json!({
            "signal": signal.as_str(),
            "encoding": encoding.as_str(),
            "compressed": compressed,
            "body_bytes": body_bytes,
            "resource_count": self.resource_count,
            "service_name": self.services.first().cloned().unwrap_or_default(),
            "resource_attributes": Value::Object(self.resource_attributes.clone()),
        });
        if self.services.len() > 1 {
            data["service_names"] = json!(self.services);
        }
        match signal {
            Signal::Traces => {
                data["span_count"] = json!(self.item_count);
                data["error_span_count"] = json!(self.error_count);
                data["span_names"] = json!(self.names);
            }
            Signal::Metrics => {
                data["metric_count"] = json!(self.metric_count);
                data["data_point_count"] = json!(self.item_count);
                data["metric_names"] = json!(self.names);
            }
            Signal::Logs => {
                data["log_record_count"] = json!(self.item_count);
                data["error_log_count"] = json!(self.error_count);
                data["log_bodies"] = json!(self.names);
            }
        }
        data
    }
}

/// An `AnyValue` as one line of text. Arrays, maps and bytes are described, not expanded, so
/// nothing here recurses and no raw bytes reach the model.
fn any_value_text(value: Option<&AnyValue>) -> String {
    match value.and_then(|v| v.value.as_ref()) {
        None => String::new(),
        Some(any_value::Value::StringValue(s)) => s.clone(),
        Some(any_value::Value::BoolValue(b)) => b.to_string(),
        Some(any_value::Value::IntValue(i)) => i.to_string(),
        Some(any_value::Value::DoubleValue(d)) => d.to_string(),
        Some(any_value::Value::ArrayValue(a)) => format!("<array of {}>", a.values.len()),
        Some(any_value::Value::KvlistValue(k)) => format!("<map of {}>", k.values.len()),
        Some(any_value::Value::BytesValue(b)) => format!("<{} bytes>", b.len()),
    }
}

fn attributes(list: &[KeyValue]) -> Vec<(String, String)> {
    list.iter()
        .map(|kv| {
            (
                kv.key.clone(),
                crate::utils::truncate_for_llm(
                    &crate::utils::sanitize::line_field(&any_value_text(kv.value.as_ref())),
                    VALUE_BYTES,
                ),
            )
        })
        .collect()
}

/// Decode a protobuf export request.
pub fn summarize_protobuf(signal: Signal, body: &[u8]) -> Result<Summary, String> {
    let mut summary = Summary::default();
    match signal {
        Signal::Traces => {
            let request = ExportTraceServiceRequest::decode(body).map_err(|e| e.to_string())?;
            for rs in &request.resource_spans {
                summary.add_resource(
                    rs.resource
                        .as_ref()
                        .map(|r| attributes(&r.attributes))
                        .unwrap_or_default(),
                );
                for ss in &rs.scope_spans {
                    for span in &ss.spans {
                        summary.item_count += 1;
                        if span.status.as_ref().is_some_and(|s| s.code == 2) {
                            summary.error_count += 1;
                        }
                        summary.add_name(&span.name, MAX_NAMES, NAME_BYTES);
                    }
                }
            }
        }
        Signal::Metrics => {
            let request = ExportMetricsServiceRequest::decode(body).map_err(|e| e.to_string())?;
            for rm in &request.resource_metrics {
                summary.add_resource(
                    rm.resource
                        .as_ref()
                        .map(|r| attributes(&r.attributes))
                        .unwrap_or_default(),
                );
                for sm in &rm.scope_metrics {
                    for metric in &sm.metrics {
                        summary.metric_count += 1;
                        summary.item_count += match &metric.data {
                            Some(Data::Gauge(g)) => g.data_points.len(),
                            Some(Data::Sum(s)) => s.data_points.len(),
                            Some(Data::Histogram(h)) => h.data_points.len(),
                            Some(Data::ExponentialHistogram(h)) => h.data_points.len(),
                            Some(Data::Summary(s)) => s.data_points.len(),
                            None => 0,
                        };
                        summary.add_name(&metric.name, MAX_NAMES, NAME_BYTES);
                    }
                }
            }
        }
        Signal::Logs => {
            let request = ExportLogsServiceRequest::decode(body).map_err(|e| e.to_string())?;
            for rl in &request.resource_logs {
                summary.add_resource(
                    rl.resource
                        .as_ref()
                        .map(|r| attributes(&r.attributes))
                        .unwrap_or_default(),
                );
                for sl in &rl.scope_logs {
                    for record in &sl.log_records {
                        summary.item_count += 1;
                        if record.severity_number >= 17 {
                            summary.error_count += 1;
                        }
                        summary.add_name(
                            &any_value_text(record.body.as_ref()),
                            MAX_LOG_BODIES,
                            VALUE_BYTES,
                        );
                    }
                }
            }
        }
    }
    Ok(summary)
}

/// `obj[camel]`, else `obj[snake]`.
fn field<'a>(obj: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    obj.get(camel).or_else(|| obj.get(snake))
}

/// A list field: absent or null is empty; anything but an array is an error.
fn list<'a>(obj: &'a Value, camel: &str, snake: &str) -> Result<&'a [Value], String> {
    match field(obj, camel, snake) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(a)) => Ok(a),
        Some(_) => Err(format!("{camel} is not an array")),
    }
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Value, String> {
    if value.is_object() {
        Ok(value)
    } else {
        Err(format!("{what} is not an object"))
    }
}

/// An OTLP/JSON `AnyValue` as one line of text, without recursing.
fn json_any_value_text(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_object) else {
        return String::new();
    };
    let scalar = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if let Some(v) = value
        .get("stringValue")
        .or_else(|| value.get("string_value"))
    {
        return scalar(v);
    }
    for key in [
        "boolValue",
        "bool_value",
        "intValue",
        "int_value",
        "doubleValue",
        "double_value",
    ] {
        if let Some(v) = value.get(key) {
            return scalar(v);
        }
    }
    let len = |v: &Value| {
        v.get("values")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0)
    };
    if let Some(v) = value.get("arrayValue").or_else(|| value.get("array_value")) {
        return format!("<array of {}>", len(v));
    }
    if let Some(v) = value
        .get("kvlistValue")
        .or_else(|| value.get("kvlist_value"))
    {
        return format!("<map of {}>", len(v));
    }
    if let Some(Value::String(b)) = value.get("bytesValue").or_else(|| value.get("bytes_value")) {
        // Base64: three bytes per four characters.
        return format!("<{} bytes>", b.trim_end_matches('=').len() * 3 / 4);
    }
    String::new()
}

fn json_attributes(resource: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    let Some(resource) = resource.filter(|r| !r.is_null()) else {
        return Ok(Vec::new());
    };
    let resource = object(resource, "resource")?;
    list(resource, "attributes", "attributes")?
        .iter()
        .map(|kv| {
            let kv = object(kv, "attribute")?;
            let key = kv
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let value = crate::utils::truncate_for_llm(
                &crate::utils::sanitize::line_field(&json_any_value_text(kv.get("value"))),
                VALUE_BYTES,
            );
            Ok((key, value))
        })
        .collect()
}

fn json_number(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Decode an OTLP/JSON export request.
pub fn summarize_json(signal: Signal, body: &[u8]) -> Result<Summary, String> {
    let root: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let root = object(&root, "the request")?;
    let mut summary = Summary::default();
    match signal {
        Signal::Traces => {
            for rs in list(root, "resourceSpans", "resource_spans")? {
                let rs = object(rs, "resourceSpans entry")?;
                summary.add_resource(json_attributes(rs.get("resource"))?);
                for ss in list(rs, "scopeSpans", "scope_spans")? {
                    for span in list(object(ss, "scopeSpans entry")?, "spans", "spans")? {
                        let span = object(span, "span")?;
                        summary.item_count += 1;
                        let code = span.get("status").and_then(|s| s.get("code"));
                        if json_number(code) == Some(2)
                            || code.and_then(Value::as_str) == Some("STATUS_CODE_ERROR")
                        {
                            summary.error_count += 1;
                        }
                        let name = span.get("name").and_then(Value::as_str).unwrap_or("");
                        summary.add_name(name, MAX_NAMES, NAME_BYTES);
                    }
                }
            }
        }
        Signal::Metrics => {
            for rm in list(root, "resourceMetrics", "resource_metrics")? {
                let rm = object(rm, "resourceMetrics entry")?;
                summary.add_resource(json_attributes(rm.get("resource"))?);
                for sm in list(rm, "scopeMetrics", "scope_metrics")? {
                    for metric in list(object(sm, "scopeMetrics entry")?, "metrics", "metrics")? {
                        let metric = object(metric, "metric")?;
                        summary.metric_count += 1;
                        for (camel, snake) in [
                            ("gauge", "gauge"),
                            ("sum", "sum"),
                            ("histogram", "histogram"),
                            ("exponentialHistogram", "exponential_histogram"),
                            ("summary", "summary"),
                        ] {
                            if let Some(data) = field(metric, camel, snake).filter(|d| !d.is_null())
                            {
                                summary.item_count +=
                                    list(object(data, camel)?, "dataPoints", "data_points")?.len();
                            }
                        }
                        let name = metric.get("name").and_then(Value::as_str).unwrap_or("");
                        summary.add_name(name, MAX_NAMES, NAME_BYTES);
                    }
                }
            }
        }
        Signal::Logs => {
            for rl in list(root, "resourceLogs", "resource_logs")? {
                let rl = object(rl, "resourceLogs entry")?;
                summary.add_resource(json_attributes(rl.get("resource"))?);
                for sl in list(rl, "scopeLogs", "scope_logs")? {
                    for record in list(object(sl, "scopeLogs entry")?, "logRecords", "log_records")?
                    {
                        let record = object(record, "log record")?;
                        summary.item_count += 1;
                        if json_number(field(record, "severityNumber", "severity_number"))
                            .is_some_and(|n| n >= 17)
                        {
                            summary.error_count += 1;
                        }
                        summary.add_name(
                            &json_any_value_text(record.get("body")),
                            MAX_LOG_BODIES,
                            VALUE_BYTES,
                        );
                    }
                }
            }
        }
    }
    Ok(summary)
}

/// Decode a request in either encoding.
pub fn summarize(signal: Signal, encoding: Encoding, body: &[u8]) -> Result<Summary, String> {
    match encoding {
        Encoding::Protobuf => summarize_protobuf(signal, body),
        Encoding::Json => summarize_json(signal, body),
    }
}

/// The export response: full success when `partial` is `None`, else a partial success naming
/// how many items were rejected and why.
pub fn export_response(
    signal: Signal,
    encoding: Encoding,
    partial: Option<(i64, &str)>,
) -> Vec<u8> {
    match encoding {
        Encoding::Protobuf => match signal {
            Signal::Traces => ExportTraceServiceResponse {
                partial_success: partial.map(|(n, m)| ExportTracePartialSuccess {
                    rejected_spans: n,
                    error_message: m.to_string(),
                }),
            }
            .encode_to_vec(),
            Signal::Metrics => ExportMetricsServiceResponse {
                partial_success: partial.map(|(n, m)| ExportMetricsPartialSuccess {
                    rejected_data_points: n,
                    error_message: m.to_string(),
                }),
            }
            .encode_to_vec(),
            Signal::Logs => ExportLogsServiceResponse {
                partial_success: partial.map(|(n, m)| ExportLogsPartialSuccess {
                    rejected_log_records: n,
                    error_message: m.to_string(),
                }),
            }
            .encode_to_vec(),
        },
        Encoding::Json => {
            let body = match partial {
                None => json!({}),
                Some((n, message)) => {
                    let key = match signal {
                        Signal::Traces => "rejectedSpans",
                        Signal::Metrics => "rejectedDataPoints",
                        Signal::Logs => "rejectedLogRecords",
                    };
                    // int64 is a string in protobuf's JSON mapping.
                    json!({"partialSuccess": {key: n.to_string(), "errorMessage": message}})
                }
            };
            body.to_string().into_bytes()
        }
    }
}

/// `google.rpc.Status`, the body of every OTLP/HTTP failure response.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RpcStatus {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// The gRPC code an HTTP failure status corresponds to, per the OTLP specification's mapping.
pub fn grpc_code(http_status: u16) -> i32 {
    match http_status {
        400 => 3,        // INVALID_ARGUMENT
        401 => 16,       // UNAUTHENTICATED
        403 => 7,        // PERMISSION_DENIED
        404 => 5,        // NOT_FOUND
        413 | 429 => 8,  // RESOURCE_EXHAUSTED
        502 | 503 => 14, // UNAVAILABLE
        504 => 4,        // DEADLINE_EXCEEDED
        _ => 13,         // INTERNAL
    }
}

/// The statuses a model may refuse with. 429, 502, 503 and 504 are the ones OTLP clients retry.
pub const REJECT_STATUSES: &[u16] = &[400, 401, 403, 413, 429, 500, 502, 503, 504];

/// Whether an OTLP client retries after this status.
pub fn retryable(http_status: u16) -> bool {
    matches!(http_status, 429 | 502 | 503 | 504)
}

/// A failure body in the request's encoding.
pub fn status_body(encoding: Encoding, http_status: u16, message: &str) -> Vec<u8> {
    let code = grpc_code(http_status);
    match encoding {
        Encoding::Protobuf => RpcStatus {
            code,
            message: message.to_string(),
        }
        .encode_to_vec(),
        Encoding::Json => json!({"code": code, "message": message})
            .to_string()
            .into_bytes(),
    }
}
