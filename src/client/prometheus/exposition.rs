//! Bounded ingestion of classic text 0.0.4 and OpenMetrics text 1.0.0.
//! Samples retain their wire names; family names differ for counters between the formats.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_BODY: usize = 4 * 1024 * 1024;
pub const MAX_SAMPLES: usize = 20_000;
pub const MAX_FAMILIES: usize = 4096;
pub const MAX_LABELS: usize = 64;
pub const MAX_NAME: usize = 256;
pub const MAX_TEXT: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    OpenMetrics,
}
impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::OpenMetrics => "openmetrics",
        }
    }
}
pub fn content_type(header: &str) -> Result<Format> {
    let mut parts = header.split(';');
    let format = match parts
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "text/plain" => Format::Text,
        "application/openmetrics-text" => Format::OpenMetrics,
        _ => bail!("unsupported metrics Content-Type"),
    };
    let mut seen = BTreeSet::new();
    for part in parts {
        let (key, value) = part
            .trim()
            .split_once('=')
            .context("malformed Content-Type parameter")?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches('"');
        ensure!(seen.insert(key.clone()), "duplicate Content-Type parameter");
        match key.as_str() {
            "version" => ensure!(
                value
                    == if format == Format::Text {
                        "0.0.4"
                    } else {
                        "1.0.0"
                    },
                "unsupported metrics format version"
            ),
            "charset" => ensure!(value.eq_ignore_ascii_case("utf-8"), "metrics must be UTF-8"),
            "escaping" => ensure!(value == "underscores", "unsupported metrics name escaping"),
            _ => bail!("unsupported metrics Content-Type parameter {key}"),
        }
    }
    Ok(format)
}
fn name(s: &str, metric: bool) -> Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= MAX_NAME,
        "metric/label name length"
    );
    ensure!(
        s.bytes().enumerate().all(|(i, c)| c.is_ascii_alphabetic()
            || c == b'_'
            || (metric && c == b':')
            || (i > 0 && c.is_ascii_digit())),
        "unsupported metric/label name {s:?}"
    );
    Ok(())
}
fn number(s: &str) -> Result<(f64, Value)> {
    let n = match s.to_ascii_lowercase().as_str() {
        "nan" => f64::NAN,
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        _ => {
            ensure!(
                !s.is_empty()
                    && s.bytes()
                        .all(|c| c.is_ascii_digit() || b"+-.eE".contains(&c)),
                "invalid metric number {s:?}"
            );
            let n = s.parse::<f64>().context("invalid metric number")?;
            ensure!(
                n.is_finite(),
                "metric number overflow; use explicit infinity"
            );
            n
        }
    };
    Ok((
        n,
        if n.is_nan() {
            json!("NaN")
        } else if n == f64::INFINITY {
            json!("+Inf")
        } else if n == f64::NEG_INFINITY {
            json!("-Inf")
        } else {
            json!(n)
        },
    ))
}
fn unescape(s: &str, quote: bool) -> Result<String> {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            result.push(match chars.next() {
                Some('\\') => '\\',
                Some('n') => '\n',
                Some('"') if quote => '"',
                _ => bail!("invalid exposition escape"),
            });
        } else {
            result.push(c);
        }
        ensure!(result.len() <= MAX_TEXT, "exposition text limit");
    }
    Ok(result)
}
struct Cursor<'a> {
    rest: &'a str,
}
impl<'a> Cursor<'a> {
    fn whitespace(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t']);
    }
    fn token(&mut self) -> Result<&'a str> {
        let n = self.rest.find([' ', '\t']).unwrap_or(self.rest.len());
        let token = &self.rest[..n];
        ensure!(!token.is_empty(), "missing exposition token");
        self.rest = &self.rest[n..];
        self.whitespace();
        Ok(token)
    }
    fn labels(&mut self, format: Format) -> Result<Map<String, Value>> {
        let mut result = Map::new();
        ensure!(self.rest.starts_with('{'), "expected labels");
        self.rest = &self.rest[1..];
        self.whitespace();
        if self.rest.starts_with('}') {
            self.rest = &self.rest[1..];
            return Ok(result);
        }
        loop {
            ensure!(result.len() < MAX_LABELS, "label count limit");
            let n = self.rest.find('=').context("missing label equals")?;
            let key = self.rest[..n].trim();
            name(key, false)?;
            ensure!(key != "__name__", "reserved __name__ label");
            self.rest = self.rest[n + 1..].trim_start_matches([' ', '\t']);
            ensure!(self.rest.starts_with('"'), "label value must be quoted");
            self.rest = &self.rest[1..];
            let mut escaped = false;
            let mut end = None;
            for (i, c) in self.rest.char_indices() {
                if c == '"' && !escaped {
                    end = Some(i);
                    break;
                }
                if c == '\\' && !escaped {
                    escaped = true;
                } else {
                    escaped = false;
                }
            }
            let end = end.context("unterminated label value")?;
            let value = unescape(&self.rest[..end], true)?;
            ensure!(
                result.insert(key.into(), json!(value)).is_none(),
                "duplicate label {key}"
            );
            self.rest = &self.rest[end + 1..];
            self.whitespace();
            if self.rest.starts_with('}') {
                self.rest = &self.rest[1..];
                break;
            }
            ensure!(self.rest.starts_with(','), "missing label comma");
            self.rest = &self.rest[1..];
            self.whitespace();
            if self.rest.starts_with('}') {
                ensure!(format == Format::Text, "OpenMetrics trailing label comma");
                self.rest = &self.rest[1..];
                break;
            }
        }
        Ok(result)
    }
}
#[derive(Default)]
struct Family {
    kind: Option<String>,
    help: Option<String>,
    unit: Option<String>,
    samples: Vec<Value>,
}
fn suffixes(kind: &str, format: Format) -> &'static [&'static str] {
    match (kind, format) {
        ("counter", Format::OpenMetrics) => &["_total", "_created"],
        ("histogram", Format::OpenMetrics) => &["_bucket", "_sum", "_count", "_created"],
        ("histogram", _) => &["_bucket", "_sum", "_count"],
        ("gaugehistogram", _) => &["_bucket", "_gsum", "_gcount"],
        ("summary", Format::OpenMetrics) => &["", "_sum", "_count", "_created"],
        ("summary", _) => &["", "_sum", "_count"],
        ("info", _) => &["_info"],
        _ => &[""],
    }
}
#[derive(Default)]
struct HistogramPoint {
    last_bucket: Option<(f64, f64)>,
    count: Option<f64>,
}
fn validate_histogram(family: &Family) -> Result<()> {
    let mut points: BTreeMap<String, HistogramPoint> = BTreeMap::new();
    for sample in &family.samples {
        let suffix = sample["suffix"].as_str().unwrap();
        let mut labels = sample["labels"].as_object().unwrap().clone();
        let bound = labels.remove("le");
        let key = serde_json::to_string(&labels)?;
        let point = points.entry(key).or_default();
        let value = sample["value"]
            .as_f64()
            .or_else(|| {
                sample["value"]
                    .as_str()
                    .and_then(|s| number(s).ok().map(|v| v.0))
            })
            .context("invalid histogram value")?;
        if suffix == "_bucket" {
            let (bound, _) = number(
                bound
                    .as_ref()
                    .and_then(Value::as_str)
                    .context("bucket missing le")?,
            )?;
            ensure!(
                !bound.is_nan() && value.is_finite() && value >= 0.0 && value.fract() == 0.0,
                "histogram bucket count must be a finite nonnegative integer"
            );
            if let Some((previous, count)) = point.last_bucket {
                ensure!(
                    bound > previous && value >= count,
                    "histogram buckets must increase and counts be cumulative"
                );
            }
            point.last_bucket = Some((bound, value));
        } else {
            ensure!(bound.is_none(), "le label on a non-bucket histogram sample");
            if suffix == "_count" || suffix == "_gcount" {
                ensure!(
                    value.is_finite() && value >= 0.0 && value.fract() == 0.0,
                    "histogram count must be a finite nonnegative integer"
                );
                point.count = Some(value);
            }
        }
    }
    for point in points.values() {
        let (bound, count) = point
            .last_bucket
            .context("histogram point has no buckets")?;
        ensure!(
            bound == f64::INFINITY,
            "histogram point missing +Inf bucket"
        );
        if let Some(total) = point.count {
            ensure!(total == count, "histogram +Inf bucket disagrees with count");
        }
    }
    Ok(())
}
pub fn parse(body: &[u8], format: Format) -> Result<Value> {
    ensure!(body.len() <= MAX_BODY, "metrics body limit");
    let text = std::str::from_utf8(body).context("metrics body is not UTF-8")?;
    ensure!(!text.starts_with('\u{feff}'), "metrics byte order mark");
    if format == Format::Text {
        ensure!(
            text.is_empty() || text.ends_with('\n'),
            "text exposition missing final newline"
        );
    }
    if format == Format::OpenMetrics {
        ensure!(!text.contains('\r'), "OpenMetrics carriage return");
    }
    let mut families: BTreeMap<String, Family> = BTreeMap::new();
    let mut aliases: BTreeMap<String, (String, &'static str)> = BTreeMap::new();
    let mut series = BTreeSet::new();
    let mut count = 0;
    let mut eof = false;
    let mut current: Option<String> = None;
    let mut closed = BTreeSet::new();
    for (line_index, line) in text.split_terminator('\n').enumerate() {
        let line = line.trim_matches([' ', '\t']);
        ensure!(!eof, "data after OpenMetrics EOF");
        if line.is_empty() {
            continue;
        }
        if line == "# EOF" {
            ensure!(
                format == Format::OpenMetrics,
                "unexpected EOF in classic text"
            );
            eof = true;
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            let mut cursor = Cursor {
                rest: comment.trim_start_matches([' ', '\t']),
            };
            if cursor.rest.is_empty() {
                ensure!(format == Format::Text, "empty OpenMetrics comment");
                continue;
            }
            let key = cursor.token()?;
            if !["HELP", "TYPE", "UNIT"].contains(&key) {
                ensure!(format == Format::Text, "unsupported OpenMetrics comment");
                continue;
            }
            let metric = cursor.token()?;
            name(metric, true)?;
            ensure!(
                families.contains_key(metric) || families.len() < MAX_FAMILIES,
                "metric family limit"
            );
            let f = families.entry(metric.into()).or_default();
            ensure!(f.samples.is_empty(), "metadata after samples for {metric}");
            match key {
                "TYPE" => {
                    ensure!(f.kind.is_none(), "duplicate TYPE {metric}");
                    let kind = cursor.token()?;
                    let allowed = if format == Format::Text {
                        &["counter", "gauge", "histogram", "summary", "untyped"][..]
                    } else {
                        &[
                            "counter",
                            "gauge",
                            "histogram",
                            "summary",
                            "unknown",
                            "gaugehistogram",
                            "info",
                            "stateset",
                        ][..]
                    };
                    ensure!(
                        allowed.contains(&kind) && cursor.rest.is_empty(),
                        "invalid metric TYPE"
                    );
                    f.kind = Some(kind.into());
                    for suffix in suffixes(kind, format) {
                        ensure!(
                            aliases
                                .insert(format!("{metric}{suffix}"), (metric.into(), suffix))
                                .is_none(),
                            "ambiguous metric family/sample names"
                        );
                    }
                }
                "HELP" => {
                    ensure!(f.help.is_none(), "duplicate HELP {metric}");
                    f.help = Some(unescape(cursor.rest, format == Format::OpenMetrics)?);
                }
                _ => {
                    ensure!(
                        format == Format::OpenMetrics && f.unit.is_none(),
                        "unsupported or duplicate UNIT"
                    );
                    let unit = cursor.rest;
                    if !unit.is_empty() {
                        name(unit, false)?;
                        ensure!(
                            metric.ends_with(&format!("_{unit}")),
                            "UNIT must suffix family name"
                        );
                    }
                    f.unit = Some(unit.into());
                }
            }
            continue;
        }
        count += 1;
        ensure!(count <= MAX_SAMPLES, "metric sample limit");
        let mut cursor = Cursor { rest: line };
        let end = line
            .find(['{', ' ', '\t'])
            .context("sample missing value")?;
        let sample_name = &line[..end];
        name(sample_name, true)?;
        cursor.rest = &line[end..];
        let labels = if cursor.rest.starts_with('{') {
            cursor.labels(format)?
        } else {
            Map::new()
        };
        ensure!(
            cursor.rest.starts_with([' ', '\t']),
            "sample value must be separated"
        );
        cursor.whitespace();
        let (value, value_json) =
            number(cursor.token()?).with_context(|| format!("line {}", line_index + 1))?;
        let mut sample = json!({"name":sample_name,"labels":labels,"value":value_json});
        if !cursor.rest.is_empty() && !cursor.rest.starts_with('#') {
            let timestamp = cursor.token()?;
            if format == Format::Text {
                sample["timestamp_ms"] = json!(timestamp
                    .parse::<i64>()
                    .context("text timestamp must be int64 milliseconds")?);
            } else {
                let (n, v) = number(timestamp)?;
                ensure!(n.is_finite(), "nonfinite OpenMetrics timestamp");
                sample["timestamp_seconds"] = v;
            }
        }
        if !cursor.rest.is_empty() {
            ensure!(
                format == Format::OpenMetrics && cursor.rest.starts_with("# "),
                "unexpected trailing sample content"
            );
            cursor.rest = &cursor.rest[2..];
            let labels = cursor.labels(format)?;
            ensure!(!labels.is_empty(), "empty exemplar labels");
            ensure!(
                labels
                    .iter()
                    .map(|(k, v)| k.len() + v.as_str().unwrap().chars().count())
                    .sum::<usize>()
                    <= 128,
                "exemplar label length"
            );
            ensure!(
                cursor.rest.starts_with(' '),
                "exemplar value must be separated"
            );
            cursor.whitespace();
            let (_, v) = number(cursor.token()?)?;
            let mut exemplar = json!({"labels":labels,"value":v});
            if !cursor.rest.is_empty() {
                let (n, v) = number(cursor.token()?)?;
                ensure!(n.is_finite(), "nonfinite exemplar timestamp");
                exemplar["timestamp_seconds"] = v;
            }
            ensure!(cursor.rest.is_empty(), "trailing exemplar content");
            sample["exemplar"] = exemplar;
        }
        ensure!(
            series.insert(serde_json::to_string(&(sample_name, &sample["labels"]))?),
            "duplicate metric series"
        );
        let (family, suffix) = aliases
            .get(sample_name)
            .cloned()
            .unwrap_or((sample_name.into(), ""));
        ensure!(!closed.contains(&family), "interleaved metric families");
        if current.as_ref() != Some(&family) {
            if let Some(previous) = current.replace(family.clone()) {
                closed.insert(previous);
            }
        }
        ensure!(
            families.contains_key(&family) || families.len() < MAX_FAMILIES,
            "metric family limit"
        );
        let f = families.entry(family).or_default();
        let kind = f.kind.as_deref().unwrap_or(if format == Format::Text {
            "untyped"
        } else {
            "unknown"
        });
        ensure!(
            suffixes(kind, format).contains(&suffix),
            "sample does not match declared metric type"
        );
        sample["suffix"] = json!(suffix);
        if suffix == "_bucket" {
            let le = sample["labels"]["le"]
                .as_str()
                .context("histogram bucket missing le")?;
            let (n, _) = number(le)?;
            ensure!(!n.is_nan(), "NaN bucket bound");
        }
        if kind == "summary" && suffix.is_empty() {
            let q = sample["labels"]["quantile"]
                .as_str()
                .context("summary missing quantile")?;
            let (q, _) = number(q)?;
            ensure!((0.0..=1.0).contains(&q), "quantile outside 0..1");
        }
        if format == Format::OpenMetrics {
            if kind == "info" {
                ensure!(value == 1.0, "info value must be 1");
            }
            if kind == "stateset" {
                ensure!(
                    value == 0.0 || value == 1.0,
                    "stateset value must be boolean"
                );
            }
            if (kind == "counter" && suffix == "_total")
                || ["_bucket", "_count", "_gcount", "_created"].contains(&suffix)
            {
                ensure!(
                    !value.is_nan() && value >= 0.0,
                    "negative/NaN counter/count/created"
                );
            }
            if ["_bucket", "_count", "_gcount"].contains(&suffix) {
                ensure!(
                    value.is_finite() && value.fract() == 0.0,
                    "count must be a finite integer"
                );
            }
            ensure!(
                sample.get("exemplar").is_none()
                    || (kind == "counter" && suffix == "_total")
                    || suffix == "_bucket",
                "exemplar on unsupported sample"
            );
        }
        f.samples.push(sample);
    }
    ensure!(format == Format::Text || eof, "OpenMetrics missing EOF");
    for f in families.values() {
        if matches!(f.kind.as_deref(), Some("histogram" | "gaugehistogram")) {
            validate_histogram(f)?;
        }
        if matches!(f.kind.as_deref(), Some("info" | "stateset")) {
            ensure!(
                f.unit.as_deref().unwrap_or("").is_empty(),
                "info/stateset must have no unit"
            );
        }
    }
    let metrics:Vec<Value>=families.into_iter().map(|(name,f)|json!({"name":name,"type":f.kind.unwrap_or_else(||if format==Format::Text {"untyped"} else {"unknown"}.into()),"help":f.help,"unit":f.unit,"samples":f.samples})).collect();
    Ok(json!({"format":format.name(),"sample_count":count,"metrics":metrics}))
}
