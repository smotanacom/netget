//! Published Remote Write 1.0 wire facts; native bounded protobuf and Snappy block.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const DEFAULT_LLM_FALLBACK: bool = false;
pub const DEFAULT_RETRY_429: bool = false;
pub const DEFAULT_PATH: &str = "/api/v1/write";
pub const MAX_BODY_BYTES: usize = 256 * 1024;
pub const MAX_SERIES: usize = 128;
pub const MAX_SAMPLES: usize = 2048;
pub const MAX_LABELS: usize = 32;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_VALUE_BYTES: usize = 2048;
pub const MAX_PROTO_FIELDS: usize = 32768;
pub const STALE_BITS: u64 = 0x7ff0000000000002;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SampleValue {
    Number(f64),
    Special(SpecialValue),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SpecialValue {
    #[serde(rename = "nan")]
    Nan,
    #[serde(rename = "+inf")]
    PositiveInfinity,
    #[serde(rename = "-inf")]
    NegativeInfinity,
    #[serde(rename = "stale")]
    Stale,
}
impl SampleValue {
    pub fn bits(self) -> Result<u64> {
        Ok(match self {
            Self::Number(n) => {
                ensure!(n.is_finite(), "use typed special sample values");
                n.to_bits()
            }
            Self::Special(SpecialValue::Nan) => 0x7ff8000000000000,
            Self::Special(SpecialValue::PositiveInfinity) => f64::INFINITY.to_bits(),
            Self::Special(SpecialValue::NegativeInfinity) => f64::NEG_INFINITY.to_bits(),
            Self::Special(SpecialValue::Stale) => STALE_BITS,
        })
    }
    pub fn from_bits(bits: u64) -> Self {
        let n = f64::from_bits(bits);
        if bits == STALE_BITS {
            Self::Special(SpecialValue::Stale)
        } else if n.is_nan() {
            Self::Special(SpecialValue::Nan)
        } else if n == f64::INFINITY {
            Self::Special(SpecialValue::PositiveInfinity)
        } else if n == f64::NEG_INFINITY {
            Self::Special(SpecialValue::NegativeInfinity)
        } else {
            Self::Number(n)
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub timestamp_ms: i64,
    pub value: SampleValue,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Series {
    pub labels: BTreeMap<String, String>,
    pub samples: Vec<Sample>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WriteBatch {
    pub series: Vec<Series>,
}
#[derive(Clone, Debug, Serialize)]
pub struct DecodedBatch {
    pub series: Vec<Series>,
    pub ignored_fields: usize,
}

pub fn validate_token(token: &str) -> Result<()> {
    ensure!(
        !token.is_empty()
            && token.len() <= 1024
            && token.bytes().all(|b| (0x21..=0x7e).contains(&b)),
        "token printable ASCII byte limit"
    );
    Ok(())
}
pub fn validate_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/')
            && !path.starts_with("//")
            && path.len() <= 1024
            && path.bytes().all(|b| (0x21..=0x7e).contains(&b))
            && !path.contains(['?', '#', '\\']),
        "absolute HTTP path byte/character limit"
    );
    let uri: hyper::Uri = path.parse().context("invalid write path")?;
    ensure!(
        uri.scheme().is_none() && uri.authority().is_none() && uri.query().is_none(),
        "write path only"
    );
    Ok(())
}
fn identifier(name: &str, metric: bool) -> Result<()> {
    let mut bytes = name.bytes();
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_NAME_BYTES
            && bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || (metric && b == b':'))
            && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || (metric && b == b':')),
        "legacy v1 metric/label name character/byte limit"
    );
    Ok(())
}
pub fn validate_batch(batch: &WriteBatch) -> Result<()> {
    ensure!(batch.series.len() <= MAX_SERIES, "series count limit");
    let mut samples = 0usize;
    let mut seen = BTreeSet::new();
    for series in &batch.series {
        ensure!(
            !series.labels.is_empty() && series.labels.len() <= MAX_LABELS,
            "label count limit"
        );
        ensure!(seen.insert(&series.labels), "duplicate series label set");
        for (name, value) in &series.labels {
            identifier(name, false)?;
            ensure!(
                !value.is_empty() && value.len() <= MAX_VALUE_BYTES,
                "label value empty/byte limit"
            );
            if name == "__name__" {
                identifier(value, true)?;
            }
        }
        ensure!(!series.samples.is_empty(), "series requires float samples");
        samples = samples
            .checked_add(series.samples.len())
            .context("sample count overflow")?;
        ensure!(samples <= MAX_SAMPLES, "sample count limit");
        let mut previous = None;
        for sample in &series.samples {
            sample.value.bits()?;
            ensure!(
                previous.is_none_or(|p| p <= sample.timestamp_ms),
                "samples must be timestamp ordered within a series"
            );
            previous = Some(sample.timestamp_ms);
        }
    }
    Ok(())
}
fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push(n as u8 | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn blob(out: &mut Vec<u8>, field: u64, value: &[u8]) {
    varint(out, field << 3 | 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}
pub fn encode_proto(batch: &WriteBatch) -> Result<Vec<u8>> {
    validate_batch(batch)?;
    let mut out = Vec::new();
    for series in &batch.series {
        let mut ts = Vec::new();
        for (name, value) in &series.labels {
            let mut label = Vec::new();
            blob(&mut label, 1, name.as_bytes());
            blob(&mut label, 2, value.as_bytes());
            blob(&mut ts, 1, &label);
        }
        for sample in &series.samples {
            let mut scalar = vec![9];
            scalar.extend_from_slice(&sample.value.bits()?.to_le_bytes());
            // int64 (not sint64): negative timestamps use a ten-byte varint.
            scalar.push(16);
            varint(&mut scalar, sample.timestamp_ms as u64);
            blob(&mut ts, 2, &scalar);
        }
        blob(&mut out, 1, &ts);
        ensure!(out.len() <= MAX_BODY_BYTES, "uncompressed body byte limit");
    }
    Ok(out)
}
pub fn encode_batch(batch: &WriteBatch) -> Result<Vec<u8>> {
    let body = snappy_encode(&encode_proto(batch)?);
    ensure!(body.len() <= MAX_BODY_BYTES, "wire body byte limit");
    Ok(body)
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).context("length overflow")?;
        let value = self.bytes.get(self.at..end).context("truncated field")?;
        self.at = end;
        Ok(value)
    }
    fn number(&mut self) -> Result<u64> {
        let mut n = 0;
        for shift in (0..70).step_by(7) {
            let b = self.take(1)?[0];
            ensure!(shift != 63 || b <= 1, "varint overflow");
            n |= u64::from(b & 127) << shift;
            if b & 128 == 0 {
                return Ok(n);
            }
        }
        bail!("varint overflow")
    }
    fn field(&mut self, budget: &mut usize) -> Result<Option<(u64, u8)>> {
        if self.at == self.bytes.len() {
            return Ok(None);
        }
        *budget = budget
            .checked_sub(1)
            .context("protobuf field count limit")?;
        let key = self.number()?;
        ensure!(
            key >> 3 > 0 && key >> 3 <= 0x1fffffff,
            "invalid protobuf field number"
        );
        Ok(Some((key >> 3, (key & 7) as u8)))
    }
    fn blob(&mut self, wire: u8) -> Result<&'a [u8]> {
        ensure!(wire == 2, "expected length-delimited field");
        let n = usize::try_from(self.number()?)?;
        self.take(n)
    }
    fn scalar(&mut self, wire: u8) -> Result<u64> {
        ensure!(wire == 0, "expected integer field");
        self.number()
    }
    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.number()?;
            }
            1 => {
                self.take(8)?;
            }
            2 => {
                self.blob(wire)?;
            }
            5 => {
                self.take(4)?;
            }
            _ => bail!("protobuf groups/invalid wire types unsupported"),
        }
        Ok(())
    }
}
fn label(bytes: &[u8], budget: &mut usize, ignored: &mut usize) -> Result<(String, String)> {
    let mut r = Reader::new(bytes);
    let (mut name, mut value) = (None, None);
    while let Some((field, wire)) = r.field(budget)? {
        match field {
            1 => {
                ensure!(name.is_none(), "duplicate label name field");
                name = Some(std::str::from_utf8(r.blob(wire)?)?.to_owned());
            }
            2 => {
                ensure!(value.is_none(), "duplicate label value field");
                value = Some(std::str::from_utf8(r.blob(wire)?)?.to_owned());
            }
            _ => {
                r.skip(wire)?;
                *ignored += 1;
            }
        }
    }
    Ok((name.unwrap_or_default(), value.unwrap_or_default()))
}
fn sample(bytes: &[u8], budget: &mut usize, ignored: &mut usize) -> Result<Sample> {
    let mut r = Reader::new(bytes);
    let (mut value, mut timestamp) = (None, None);
    while let Some((field, wire)) = r.field(budget)? {
        match field {
            1 => {
                ensure!(
                    value.is_none() && wire == 1,
                    "duplicate/invalid sample double"
                );
                value = Some(u64::from_le_bytes(r.take(8)?.try_into()?));
            }
            2 => {
                ensure!(timestamp.is_none(), "duplicate sample timestamp");
                timestamp = Some(r.scalar(wire)? as i64);
            }
            _ => {
                r.skip(wire)?;
                *ignored += 1;
            }
        }
    }
    Ok(Sample {
        timestamp_ms: timestamp.unwrap_or(0),
        value: SampleValue::from_bits(value.unwrap_or(0)),
    })
}
fn series(
    bytes: &[u8],
    budget: &mut usize,
    ignored: &mut usize,
    total_samples: &mut usize,
) -> Result<Series> {
    let mut r = Reader::new(bytes);
    let mut labels = BTreeMap::new();
    let mut samples = vec![];
    let mut previous = None;
    while let Some((field, wire)) = r.field(budget)? {
        match field {
            1 => {
                ensure!(labels.len() < MAX_LABELS, "label count limit");
                let (name, value) = label(r.blob(wire)?, budget, ignored)?;
                ensure!(
                    previous.as_ref().is_none_or(|p| p < &name),
                    "labels must be sorted and unique"
                );
                previous = Some(name.clone());
                labels.insert(name, value);
            }
            2 => {
                ensure!(*total_samples < MAX_SAMPLES, "sample count limit");
                samples.push(sample(r.blob(wire)?, budget, ignored)?);
                *total_samples += 1;
            }
            3 | 4 => bail!("v1 exemplars/native histograms outside selected float-sample scope"),
            _ => {
                r.skip(wire)?;
                *ignored += 1;
            }
        }
    }
    Ok(Series { labels, samples })
}
pub fn decode_proto(bytes: &[u8]) -> Result<DecodedBatch> {
    ensure!(
        bytes.len() <= MAX_BODY_BYTES,
        "uncompressed body byte limit"
    );
    let mut r = Reader::new(bytes);
    let mut budget = MAX_PROTO_FIELDS;
    let (mut ignored, mut total_samples) = (0, 0);
    let mut output = vec![];
    while let Some((field, wire)) = r.field(&mut budget)? {
        if field == 1 {
            ensure!(output.len() < MAX_SERIES, "series count limit");
            output.push(series(
                r.blob(wire)?,
                &mut budget,
                &mut ignored,
                &mut total_samples,
            )?);
        } else {
            // Reserved v1 source/metadata and future optional fields are discarded,
            // not exposed as bytes or interpreted as supported extension data.
            r.skip(wire)?;
            ignored += 1;
        }
    }
    validate_batch(&WriteBatch {
        series: output.clone(),
    })?;
    Ok(DecodedBatch {
        series: output,
        ignored_fields: ignored,
    })
}
pub fn decode_batch(bytes: &[u8]) -> Result<DecodedBatch> {
    ensure!(bytes.len() <= MAX_BODY_BYTES, "wire body byte limit");
    decode_proto(&snappy_decode(bytes)?)
}

// Literal-only encoding is valid Snappy; the decoder accepts all three copy forms.
pub fn snappy_encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, bytes.len() as u64);
    if !bytes.is_empty() {
        let n = bytes.len() - 1;
        if n < 60 {
            out.push((n as u8) << 2);
        } else {
            let width = ((usize::BITS - n.leading_zeros()) as usize).div_ceil(8);
            out.push(((59 + width) as u8) << 2);
            out.extend_from_slice(&(n as u32).to_le_bytes()[..width]);
        }
        out.extend_from_slice(bytes);
    }
    out
}
pub fn snappy_decode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut r = Reader::new(bytes);
    let n = usize::try_from(r.number()?)?;
    ensure!(r.at <= 5, "Snappy length preamble overflow");
    ensure!(n <= MAX_BODY_BYTES, "Snappy decompressed byte limit");
    let mut out = Vec::with_capacity(n);
    while r.at < bytes.len() {
        let tag = r.take(1)?[0];
        let (kind, len, offset) = match tag & 3 {
            0 => {
                let v = usize::from(tag >> 2);
                let len = if v < 60 {
                    v + 1
                } else {
                    let width = v - 59;
                    let mut raw = [0; 4];
                    raw[..width].copy_from_slice(r.take(width)?);
                    usize::try_from(u32::from_le_bytes(raw))?
                        .checked_add(1)
                        .context("literal length overflow")?
                };
                (0, len, 0)
            }
            1 => (
                1,
                usize::from((tag >> 2) & 7) + 4,
                (usize::from(tag & 0xe0) << 3) | usize::from(r.take(1)?[0]),
            ),
            2 => {
                let b = r.take(2)?;
                (
                    2,
                    usize::from(tag >> 2) + 1,
                    usize::from(u16::from_le_bytes([b[0], b[1]])),
                )
            }
            _ => {
                let b = r.take(4)?;
                (
                    3,
                    usize::from(tag >> 2) + 1,
                    usize::try_from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))?,
                )
            }
        };
        ensure!(
            len <= n.saturating_sub(out.len()),
            "Snappy output length mismatch/limit"
        );
        if kind == 0 {
            out.extend_from_slice(r.take(len)?);
        } else {
            ensure!(
                offset > 0 && offset <= out.len(),
                "invalid Snappy backreference"
            );
            for _ in 0..len {
                out.push(out[out.len() - offset]);
            }
        }
    }
    ensure!(out.len() == n, "Snappy truncated output");
    Ok(out)
}
