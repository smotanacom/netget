//! Typed, bounded telemetry builders. No raw protobuf or OTLP JSON action.
use anyhow::{bail, ensure, Context, Result};
use opentelemetry_proto::tonic::{
    collector::{
        logs::v1::ExportLogsServiceRequest, metrics::v1::ExportMetricsServiceRequest,
        trace::v1::ExportTraceServiceRequest,
    },
    common::v1::{any_value, AnyValue, InstrumentationScope, KeyValue},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    metrics::v1::{
        metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
    },
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span, Status},
};
use prost::Message;
use serde_json::Value;
pub const MAX_ITEMS: usize = 128;
pub const MAX_ATTRIBUTES: usize = 32;
pub const MAX_EXPORT_BYTES: usize = 1024 * 1024;
#[derive(Clone, Debug)]
pub enum Export {
    Traces(ExportTraceServiceRequest),
    Metrics(ExportMetricsServiceRequest),
    Logs(ExportLogsServiceRequest),
}
impl Export {
    pub fn signal(&self) -> crate::server::otlp::codec::Signal {
        match self {
            Self::Traces(_) => crate::server::otlp::codec::Signal::Traces,
            Self::Metrics(_) => crate::server::otlp::codec::Signal::Metrics,
            Self::Logs(_) => crate::server::otlp::codec::Signal::Logs,
        }
    }
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Traces(v) => v.encoded_len(),
            Self::Metrics(v) => v.encoded_len(),
            Self::Logs(v) => v.encoded_len(),
        }
    }
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Traces(v) => v.encode_to_vec(),
            Self::Metrics(v) => v.encode_to_vec(),
            Self::Logs(v) => v.encode_to_vec(),
        }
    }
}
#[derive(Default)]
struct Budget(usize);
impl Budget {
    fn charge(&mut self, n: usize) -> Result<()> {
        self.0 = self.0.checked_add(n).context("telemetry size overflow")?;
        ensure!(self.0 <= MAX_EXPORT_BYTES, "typed telemetry exceeds 1 MiB");
        Ok(())
    }
    fn text(&mut self, value: &Value, field: &str, max: usize, required: bool) -> Result<String> {
        let Some(raw) = value.get(field) else {
            ensure!(!required, "missing {field}");
            return Ok(String::new());
        };
        let text = raw
            .as_str()
            .with_context(|| format!("{field} must be a string"))?;
        ensure!(
            text.len() <= max && (!required || !text.is_empty()),
            "{field} must be {}..{max} bytes",
            usize::from(required)
        );
        self.charge(text.len())?;
        Ok(text.to_owned())
    }
    fn attributes(&mut self, value: Option<&Value>) -> Result<Vec<KeyValue>> {
        let Some(value) = value else {
            return Ok(Vec::new());
        };
        let attrs = value
            .as_object()
            .context("attributes must be a flat object")?;
        ensure!(attrs.len() <= MAX_ATTRIBUTES, "at most 32 attributes");
        let mut out = Vec::with_capacity(attrs.len());
        for (key, value) in attrs {
            ensure!(
                !key.is_empty() && key.len() <= 256,
                "attribute keys must be 1..256 bytes"
            );
            self.charge(key.len())?;
            let value = match value {
                Value::String(text) => {
                    ensure!(text.len() <= 1024, "attribute strings exceed 1024 bytes");
                    self.charge(text.len())?;
                    any_value::Value::StringValue(text.clone())
                }
                Value::Bool(value) => any_value::Value::BoolValue(*value),
                Value::Number(value) => {
                    if let Some(n) = value.as_i64() {
                        any_value::Value::IntValue(n)
                    } else {
                        let n = value.as_f64().context("invalid attribute number")?;
                        ensure!(n.is_finite(), "attribute number must be finite");
                        ensure!(!value.is_u64(), "attribute integer exceeds i64");
                        any_value::Value::DoubleValue(n)
                    }
                }
                _ => bail!("attributes accept only string, bool, i64 or finite float"),
            };
            out.push(KeyValue {
                key: key.clone(),
                value: Some(AnyValue { value: Some(value) }),
            });
        }
        Ok(out)
    }
}
fn timestamp(v: &Value, key: &str) -> Result<u64> {
    let n = v[key]
        .as_u64()
        .with_context(|| format!("{key} must be a positive u64 Unix timestamp in nanoseconds"))?;
    ensure!(n > 0, "{key} must be positive");
    Ok(n)
}
fn id(v: &Value, key: &str, bytes: usize, required: bool) -> Result<Vec<u8>> {
    let Some(value) = v.get(key) else {
        ensure!(!required, "missing {key}");
        return Ok(Vec::new());
    };
    let value = value
        .as_str()
        .with_context(|| format!("{key} must be hex"))?;
    ensure!(
        value.len() == bytes * 2,
        "{key} must be {} hex digits",
        bytes * 2
    );
    let data = hex::decode(value).with_context(|| format!("invalid {key}"))?;
    ensure!(data.iter().any(|b| *b != 0), "{key} cannot be all zero");
    Ok(data)
}
fn items<'a>(v: &'a Value, key: &str) -> Result<&'a [Value]> {
    let items = v[key]
        .as_array()
        .with_context(|| format!("{key} must be an array"))?;
    ensure!(
        !items.is_empty() && items.len() <= MAX_ITEMS,
        "{key} must contain 1..128 items"
    );
    ensure!(
        items.iter().all(Value::is_object),
        "{key} items must be objects"
    );
    Ok(items)
}
pub fn build(action: &Value) -> Result<Export> {
    let mut budget = Budget::default();
    let service = budget.text(action, "service_name", 256, true)?;
    let mut attrs = budget.attributes(action.get("resource_attributes"))?;
    ensure!(
        !attrs.iter().any(|a| a.key == "service.name"),
        "use service_name rather than resource_attributes.service.name"
    );
    attrs.push(KeyValue {
        key: "service.name".into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(service)),
        }),
    });
    let resource = Some(Resource {
        attributes: attrs,
        ..Default::default()
    });
    let scope_name = budget.text(action, "scope_name", 256, false)?;
    let scope = Some(InstrumentationScope {
        name: if scope_name.is_empty() {
            "netget".into()
        } else {
            scope_name
        },
        ..Default::default()
    });
    let export = match action["type"].as_str().context("missing type")? {
        "export_otlp_traces" => {
            let mut spans = Vec::new();
            for v in items(action, "spans")? {
                let start = timestamp(v, "start_time_unix_nano")?;
                let end = timestamp(v, "end_time_unix_nano")?;
                ensure!(end >= start, "span end must not precede start");
                let kind = match v
                    .get("kind")
                    .map(|v| v.as_str().context("kind must be a string"))
                    .transpose()?
                    .unwrap_or("internal")
                {
                    "internal" => 1,
                    "server" => 2,
                    "client" => 3,
                    "producer" => 4,
                    "consumer" => 5,
                    _ => bail!("invalid span kind"),
                };
                let status = match v
                    .get("status")
                    .map(|v| v.as_str().context("status must be a string"))
                    .transpose()?
                    .unwrap_or("unset")
                {
                    "unset" => 0,
                    "ok" => 1,
                    "error" => 2,
                    _ => bail!("invalid span status"),
                };
                spans.push(Span {
                    trace_id: id(v, "trace_id", 16, true)?,
                    span_id: id(v, "span_id", 8, true)?,
                    parent_span_id: id(v, "parent_span_id", 8, false)?,
                    name: budget.text(v, "name", 256, true)?,
                    kind,
                    start_time_unix_nano: start,
                    end_time_unix_nano: end,
                    attributes: budget.attributes(v.get("attributes"))?,
                    status: Some(Status {
                        code: status,
                        message: budget.text(v, "status_message", 512, false)?,
                    }),
                    ..Default::default()
                });
            }
            Export::Traces(ExportTraceServiceRequest {
                resource_spans: vec![ResourceSpans {
                    resource,
                    scope_spans: vec![ScopeSpans {
                        scope,
                        spans,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            })
        }
        "export_otlp_logs" => {
            let mut log_records = Vec::new();
            for v in items(action, "logs")? {
                let severity = v
                    .get("severity_number")
                    .map(|v| v.as_u64().context("severity_number must be 0..24"))
                    .transpose()?
                    .unwrap_or(9);
                ensure!(severity <= 24, "severity_number must be 0..24");
                ensure!(
                    v.get("trace_id").is_some() == v.get("span_id").is_some(),
                    "log trace_id and span_id must be supplied together"
                );
                log_records.push(LogRecord {
                    time_unix_nano: timestamp(v, "time_unix_nano")?,
                    severity_number: severity as i32,
                    severity_text: budget.text(v, "severity_text", 64, false)?,
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue(
                            budget.text(v, "body", 4096, true)?,
                        )),
                    }),
                    attributes: budget.attributes(v.get("attributes"))?,
                    trace_id: id(v, "trace_id", 16, false)?,
                    span_id: id(v, "span_id", 8, false)?,
                    ..Default::default()
                });
            }
            Export::Logs(ExportLogsServiceRequest {
                resource_logs: vec![ResourceLogs {
                    resource,
                    scope_logs: vec![ScopeLogs {
                        scope,
                        log_records,
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            })
        }
        "export_otlp_gauge" => {
            let mut data_points = Vec::new();
            for v in items(action, "data_points")? {
                let number = v["value"]
                    .as_number()
                    .context("gauge value must be a number")?;
                let value = if let Some(n) = number.as_i64() {
                    number_data_point::Value::AsInt(n)
                } else {
                    ensure!(!number.is_u64(), "gauge integer exceeds i64");
                    let n = number.as_f64().context("invalid gauge value")?;
                    ensure!(n.is_finite(), "gauge value must be finite");
                    number_data_point::Value::AsDouble(n)
                };
                data_points.push(NumberDataPoint {
                    time_unix_nano: timestamp(v, "time_unix_nano")?,
                    attributes: budget.attributes(v.get("attributes"))?,
                    value: Some(value),
                    ..Default::default()
                });
            }
            let metric = Metric {
                name: budget.text(action, "name", 256, true)?,
                description: budget.text(action, "description", 256, false)?,
                unit: budget.text(action, "unit", 64, false)?,
                data: Some(metric::Data::Gauge(Gauge { data_points })),
                ..Default::default()
            };
            Export::Metrics(ExportMetricsServiceRequest {
                resource_metrics: vec![ResourceMetrics {
                    resource,
                    scope_metrics: vec![ScopeMetrics {
                        scope,
                        metrics: vec![metric],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
            })
        }
        _ => bail!("unknown OTLP export action"),
    };
    ensure!(
        export.encoded_len() <= MAX_EXPORT_BYTES,
        "encoded export exceeds 1 MiB"
    );
    Ok(export)
}
