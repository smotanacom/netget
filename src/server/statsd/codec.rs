//! Bounded StatsD / DogStatsD UDP codec. Unknown extensions fail explicitly.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub const MAX_DATAGRAM_BYTES: usize = 8192;
pub const MAX_RECORDS: usize = 256;
pub const DEFAULT_DIALECT: &str = "dogstatsd";
pub const DEFAULT_LLM_FALLBACK: bool = false;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Statsd,
    Dogstatsd,
}
impl Dialect {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "statsd" => Ok(Self::Statsd),
            "dogstatsd" => Ok(Self::Dogstatsd),
            _ => bail!("dialect must be statsd or dogstatsd"),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Statsd => "statsd",
            Self::Dogstatsd => "dogstatsd",
        }
    }
}

/// Values stay textual: signed gauges are deltas and sets can contain non-numeric members.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    Metric {
        name: String,
        value: String,
        metric_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample_rate: Option<f64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
    },
    Event {
        title: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        aggregation_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alert_type: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
    },
    ServiceCheck {
        name: String,
        status: u8,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

fn field(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.contains(['|', '\n', '\r', '\0']),
        "empty or invalid field delimiter"
    );
    Ok(())
}
fn tags_suffix(tags: &[String], output: &mut String) -> Result<()> {
    if !tags.is_empty() {
        for tag in tags {
            field(tag)?;
            ensure!(!tag.contains(','), "comma in tag");
        }
        output.push_str("|#");
        output.push_str(&tags.join(","));
    }
    Ok(())
}
fn opt_suffix(key: &str, value: &Option<String>, output: &mut String) -> Result<()> {
    if let Some(value) = value {
        field(value)?;
        output.push('|');
        output.push_str(key);
        output.push(':');
        output.push_str(value);
    }
    Ok(())
}
fn escaped(value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\r', '\0']),
        "carriage return or NUL in text"
    );
    Ok(value.replace('\n', "\\n"))
}

pub fn encode_record(record: &Record, dialect: Dialect) -> Result<String> {
    let out = match record {
        Record::Metric {
            name,
            value,
            metric_type,
            sample_rate,
            tags,
        } => {
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.'),
                "metric name must contain ASCII letters, digits, underscores or periods"
            );
            field(value)?;
            ensure!(
                !value.contains(':'),
                "packed metric values are not supported"
            );
            ensure!(
                matches!(metric_type.as_str(), "c" | "g" | "ms" | "s")
                    || dialect == Dialect::Dogstatsd && matches!(metric_type.as_str(), "h" | "d"),
                "unsupported metric type for dialect"
            );
            if metric_type != "s" {
                let number: f64 = value.parse().context("metric value must be numeric")?;
                ensure!(
                    number.is_finite() && !value.chars().any(char::is_whitespace),
                    "metric value must be finite without whitespace"
                );
            }
            ensure!(
                dialect == Dialect::Dogstatsd || tags.is_empty(),
                "tags require dogstatsd"
            );
            let mut out = format!("{name}:{value}|{metric_type}");
            if let Some(rate) = sample_rate {
                ensure!(
                    rate.is_finite() && (0.0..=1.0).contains(rate),
                    "sample rate must be between zero and one"
                );
                ensure!(
                    dialect == Dialect::Dogstatsd || *rate > 0.0,
                    "classic StatsD sample rate must be positive"
                );
                ensure!(
                    matches!(metric_type.as_str(), "c" | "ms" | "h" | "d"),
                    "sampling is only supported on counts, timers, histograms and distributions"
                );
                out.push_str(&format!("|@{rate}"));
            }
            tags_suffix(tags, &mut out)?;
            out
        }
        Record::Event {
            title,
            text,
            timestamp,
            hostname,
            aggregation_key,
            priority,
            source_type,
            alert_type,
            tags,
        } => {
            ensure!(dialect == Dialect::Dogstatsd, "events require dogstatsd");
            ensure!(!title.is_empty(), "event title must not be empty");
            if let Some(priority) = priority {
                ensure!(
                    matches!(priority.as_str(), "low" | "normal"),
                    "invalid event priority"
                );
            }
            if let Some(alert) = alert_type {
                ensure!(
                    matches!(alert.as_str(), "error" | "warning" | "info" | "success"),
                    "invalid event alert type"
                );
            }
            let title = escaped(title)?;
            let text = escaped(text)?;
            let mut out = format!("_e{{{},{}}}:{}|{}", title.len(), text.len(), title, text);
            if let Some(ts) = timestamp {
                out.push_str(&format!("|d:{ts}"));
            }
            opt_suffix("h", hostname, &mut out)?;
            opt_suffix("k", aggregation_key, &mut out)?;
            opt_suffix("p", priority, &mut out)?;
            opt_suffix("s", source_type, &mut out)?;
            opt_suffix("t", alert_type, &mut out)?;
            tags_suffix(tags, &mut out)?;
            out
        }
        Record::ServiceCheck {
            name,
            status,
            timestamp,
            hostname,
            tags,
            message,
        } => {
            ensure!(
                dialect == Dialect::Dogstatsd,
                "service checks require dogstatsd"
            );
            field(name)?;
            ensure!(*status <= 3, "service check status must be 0..3");
            let mut out = format!("_sc|{name}|{status}");
            if let Some(ts) = timestamp {
                out.push_str(&format!("|d:{ts}"));
            }
            opt_suffix("h", hostname, &mut out)?;
            tags_suffix(tags, &mut out)?;
            if let Some(message) = message {
                out.push_str("|m:");
                out.push_str(&escaped(message)?.replace("m:", "m\\:"));
            }
            out
        }
    };
    ensure!(
        out.len() <= MAX_DATAGRAM_BYTES,
        "record exceeds datagram size limit"
    );
    Ok(out)
}

pub fn encode_datagram(records: &[Record], dialect: Dialect) -> Result<Vec<u8>> {
    ensure!(
        !records.is_empty() && records.len() <= MAX_RECORDS,
        "batch must contain 1..={MAX_RECORDS} records"
    );
    let mut output = Vec::new();
    for record in records {
        let line = encode_record(record, dialect)?;
        let separator = usize::from(!output.is_empty());
        ensure!(
            output.len() + separator + line.len() <= MAX_DATAGRAM_BYTES,
            "batch exceeds {MAX_DATAGRAM_BYTES} bytes"
        );
        if separator != 0 {
            output.push(b'\n');
        }
        output.extend_from_slice(line.as_bytes());
    }
    Ok(output)
}

fn options<'a>(
    suffix: &'a str,
    allowed: &[&str],
) -> Result<std::collections::HashMap<&'a str, &'a str>> {
    let mut out = std::collections::HashMap::new();
    if suffix.is_empty() {
        return Ok(out);
    }
    ensure!(suffix.starts_with('|'), "missing option delimiter");
    for part in suffix[1..].split('|') {
        let (key, value) = if let Some(value) = part.strip_prefix('#') {
            ("#", value)
        } else {
            part.split_once(':').context("malformed option")?
        };
        ensure!(allowed.contains(&key), "unsupported option {key}");
        field(value)?;
        ensure!(out.insert(key, value).is_none(), "duplicate option {key}");
    }
    Ok(out)
}
fn read_tags(value: Option<&&str>) -> Vec<String> {
    value
        .map(|v| v.split(',').map(str::to_owned).collect())
        .unwrap_or_default()
}
fn read_timestamp(value: Option<&&str>) -> Result<Option<u64>> {
    value
        .map(|v| v.parse().context("invalid timestamp"))
        .transpose()
}
fn read_string(value: Option<&&str>) -> Option<String> {
    value.map(|v| (*v).to_owned())
}

pub fn parse_record(line: &str, dialect: Dialect) -> Result<Record> {
    let record = if let Some(rest) = line.strip_prefix("_e{") {
        let (lengths, body) = rest.split_once("}:").context("invalid event header")?;
        let (title_len, text_len) = lengths.split_once(',').context("missing event lengths")?;
        let title_len: usize = title_len.parse().context("invalid title length")?;
        let text_len: usize = text_len.parse().context("invalid text length")?;
        let title = body
            .get(..title_len)
            .context("invalid UTF-8 title byte length")?;
        let rest = body
            .get(title_len..)
            .and_then(|s| s.strip_prefix('|'))
            .context("missing title delimiter")?;
        let text = rest
            .get(..text_len)
            .context("invalid UTF-8 text byte length")?;
        let opts = options(&rest[text_len..], &["d", "h", "k", "p", "s", "t", "#"])?;
        Record::Event {
            title: title.replace("\\n", "\n"),
            text: text.replace("\\n", "\n"),
            timestamp: read_timestamp(opts.get("d"))?,
            hostname: read_string(opts.get("h")),
            aggregation_key: read_string(opts.get("k")),
            priority: read_string(opts.get("p")),
            source_type: read_string(opts.get("s")),
            alert_type: read_string(opts.get("t")),
            tags: read_tags(opts.get("#")),
        }
    } else if let Some(rest) = line.strip_prefix("_sc|") {
        let (name, rest) = rest
            .split_once('|')
            .context("missing service check status")?;
        let (status, suffix) = rest
            .split_once('|')
            .map(|(a, b)| (a, format!("|{b}")))
            .unwrap_or((rest, String::new()));
        // A service-check message consumes the remainder, including any pipe characters.
        let (metadata, message) = suffix
            .split_once("|m:")
            .map(|(a, b)| (a, Some(b.replace("\\n", "\n").replace("m\\:", "m:"))))
            .unwrap_or((&suffix, None));
        let opts = options(metadata, &["d", "h", "#"])?;
        Record::ServiceCheck {
            name: name.to_owned(),
            status: status.parse().context("invalid service check status")?,
            timestamp: read_timestamp(opts.get("d"))?,
            hostname: read_string(opts.get("h")),
            tags: read_tags(opts.get("#")),
            message,
        }
    } else {
        let (name, rest) = line.split_once(':').context("missing metric colon")?;
        let mut parts = rest.split('|');
        let value = parts.next().context("missing metric value")?.to_owned();
        let metric_type = parts.next().context("missing metric type")?.to_owned();
        let mut sample_rate = None;
        let mut tags = None;
        for part in parts {
            if let Some(rate) = part.strip_prefix('@') {
                ensure!(sample_rate.is_none(), "duplicate sample rate");
                sample_rate = Some(rate.parse().context("invalid sample rate")?);
            } else if let Some(value) = part.strip_prefix('#') {
                ensure!(
                    tags.is_none() && !value.is_empty(),
                    "empty or duplicate tags"
                );
                tags = Some(value.split(',').map(str::to_owned).collect());
            } else {
                bail!("unsupported metric option");
            }
        }
        Record::Metric {
            name: name.to_owned(),
            value,
            metric_type,
            sample_rate,
            tags: tags.unwrap_or_default(),
        }
    };
    // One validator for both directions, including dialect and numeric restrictions.
    encode_record(&record, dialect)?;
    Ok(record)
}

/// Entire datagram is rejected if any line is malformed; no partial batch is emitted.
pub fn parse_datagram(bytes: &[u8], dialect: Dialect) -> Result<Vec<Record>> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_DATAGRAM_BYTES,
        "datagram must contain 1..={MAX_DATAGRAM_BYTES} bytes"
    );
    let text = std::str::from_utf8(bytes).context("datagram is not UTF-8")?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let mut records = Vec::new();
    for line in text.split('\n') {
        ensure!(
            records.len() < MAX_RECORDS,
            "datagram exceeds {MAX_RECORDS} records"
        );
        records.push(parse_record(line, dialect)?);
    }
    Ok(records)
}
