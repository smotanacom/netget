//! Carbon plaintext: UTF-8 metric path, finite value and timestamp, terminated by LF.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub const MAX_LINE_BYTES: usize = 4096;
pub const MAX_BATCH_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_RECORDS: usize = 256;
pub const READ_BYTES: usize = 8192;
pub const DEFAULT_LLM_FALLBACK: bool = false;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub path: String,
    pub value: f64,
    pub timestamp: f64,
}
impl Metric {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.path.is_empty()
                && !self
                    .path
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control()),
            "metric path must be nonempty without whitespace or control characters"
        );
        ensure!(self.value.is_finite(), "metric value must be finite");
        ensure!(
            self.timestamp.is_finite() && (self.timestamp >= 0.0 || self.timestamp == -1.0),
            "timestamp must be nonnegative UNIX seconds, or -1 for receiver time"
        );
        Ok(())
    }
}
pub fn parse_line(line: &[u8]) -> Result<Metric> {
    ensure!(
        !line.is_empty() && line.len() <= MAX_LINE_BYTES,
        "invalid metric line length"
    );
    let text = std::str::from_utf8(line).context("metric line must be UTF-8")?;
    let mut fields = text.split_ascii_whitespace();
    let metric = Metric {
        path: fields.next().context("missing metric path")?.into(),
        value: fields
            .next()
            .context("missing value")?
            .parse()
            .context("invalid metric value")?,
        timestamp: fields
            .next()
            .context("missing timestamp")?
            .parse()
            .context("invalid timestamp")?,
    };
    ensure!(fields.next().is_none(), "extra fields in metric line");
    metric.validate()?;
    Ok(metric)
}
pub fn encode_batch(metrics: &[Metric]) -> Result<Vec<u8>> {
    ensure!(
        !metrics.is_empty() && metrics.len() <= MAX_BATCH_RECORDS,
        "metric batch must contain 1..256 records"
    );
    let mut out = Vec::new();
    for metric in metrics {
        metric.validate()?;
        let line = format!("{} {} {}\n", metric.path, metric.value, metric.timestamp);
        ensure!(
            line.len() - 1 <= MAX_LINE_BYTES,
            "metric line exceeds bound"
        );
        ensure!(
            out.len() + line.len() <= MAX_BATCH_BYTES,
            "metric batch exceeds bound"
        );
        out.extend_from_slice(line.as_bytes());
    }
    Ok(out)
}

/// TCP has no batch boundaries. Drain at most 256 complete lines from one bounded
/// read before dispatching; retain the partial next line for the following read.
#[derive(Default)]
pub struct Decoder {
    pending: Vec<u8>,
}
impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            self.pending.len() + bytes.len() <= MAX_LINE_BYTES + READ_BYTES,
            "pending stream buffer exceeds bound"
        );
        self.pending.extend_from_slice(bytes);
        Ok(())
    }
    pub fn next_batch(&mut self) -> Result<Vec<Metric>> {
        let mut records = Vec::new();
        let mut consumed = 0;
        for line in self.pending.split_inclusive(|b| *b == b'\n') {
            if !line.ends_with(b"\n") {
                ensure!(
                    line.len() <= MAX_LINE_BYTES,
                    "unterminated metric line exceeds bound"
                );
                break;
            }
            records.push(parse_line(&line[..line.len() - 1])?);
            consumed += line.len();
            if records.len() == MAX_BATCH_RECORDS {
                break;
            }
        }
        self.pending.drain(..consumed);
        Ok(records)
    }
    pub fn finish(&self) -> Result<()> {
        ensure!(self.pending.is_empty(), "EOF before metric line terminator");
        Ok(())
    }
}
