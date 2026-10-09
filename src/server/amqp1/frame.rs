//! AMQP 1.0 transport (part 2): protocol headers, frames, performative codes and helpers.
use super::types::{self, Value};
use anyhow::{bail, ensure, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const AMQP_HEADER: [u8; 8] = *b"AMQP\x00\x01\x00\x00";
pub const SASL_HEADER: [u8; 8] = *b"AMQP\x03\x01\x00\x00";
/// The largest frame either side may use; announced as max-frame-size.
pub const MAX_FRAME: u32 = 1024 * 1024;
/// The smallest max-frame-size the specification allows.
pub const MIN_MAX_FRAME: u32 = 512;

pub const OPEN: u64 = 0x10;
pub const BEGIN: u64 = 0x11;
pub const ATTACH: u64 = 0x12;
pub const FLOW: u64 = 0x13;
pub const TRANSFER: u64 = 0x14;
pub const DISPOSITION: u64 = 0x15;
pub const DETACH: u64 = 0x16;
pub const END: u64 = 0x17;
pub const CLOSE: u64 = 0x18;
pub const ERROR: u64 = 0x1d;
pub const ACCEPTED: u64 = 0x24;
pub const REJECTED: u64 = 0x25;
pub const RELEASED: u64 = 0x26;
pub const MODIFIED: u64 = 0x27;
pub const SOURCE: u64 = 0x28;
pub const TARGET: u64 = 0x29;
pub const SASL_MECHANISMS: u64 = 0x40;
pub const SASL_INIT: u64 = 0x41;
pub const SASL_OUTCOME: u64 = 0x44;

pub const TYPE_AMQP: u8 = 0;
pub const TYPE_SASL: u8 = 1;

/// One frame: its type, channel, performative (None for an empty, keep-alive frame) and payload.
#[derive(Debug)]
pub struct Frame {
    pub kind: u8,
    pub channel: u16,
    pub body: Option<Value>,
    pub payload: Vec<u8>,
}

pub async fn read<R: AsyncRead + Unpin>(r: &mut R, max: u32) -> Result<Option<Frame>> {
    let mut head = [0u8; 8];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let size = u32::from_be_bytes(head[..4].try_into()?);
    let doff = head[4] as u32 * 4;
    ensure!(
        size >= 8 && doff >= 8 && doff <= size,
        "malformed frame header"
    );
    ensure!(
        size <= max,
        "frame of {size} bytes exceeds max-frame-size {max}"
    );
    let mut rest = vec![0u8; size as usize - 8];
    r.read_exact(&mut rest).await?;
    let body = &rest[(doff - 8) as usize..];
    let channel = u16::from_be_bytes([head[6], head[7]]);
    if body.is_empty() {
        return Ok(Some(Frame {
            kind: head[5],
            channel,
            body: None,
            payload: vec![],
        }));
    }
    let (perf, used) = types::decode(body)?;
    ensure!(
        perf.descriptor().is_some(),
        "frame body is not a performative"
    );
    Ok(Some(Frame {
        kind: head[5],
        channel,
        body: Some(perf),
        payload: body[used..].to_vec(),
    }))
}

pub fn encode(kind: u8, channel: u16, body: Option<&Value>, payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    if let Some(v) = body {
        types::encode_into(v, &mut b);
    }
    b.extend(payload);
    let mut out = ((b.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend([2, kind]);
    out.extend(channel.to_be_bytes());
    out.extend(b);
    out
}

/// An `amqp:error:list`.
pub fn error(condition: &str, description: &str) -> Value {
    Value::described(ERROR, vec![Value::sym(condition), Value::str(description)])
}

/// The condition and description of an error value.
pub fn describe_error(e: &Value) -> (Option<String>, Option<String>) {
    (
        types::field(e, 0).as_str().map(str::to_owned),
        types::field(e, 1).as_str().map(str::to_owned),
    )
}

/// The address of a source or target terminus.
pub fn address(terminus: &Value) -> Option<String> {
    types::field(terminus, 0).as_str().map(str::to_owned)
}

pub fn expect(frame: &Frame, kind: u8, code: u64) -> Result<&Value> {
    ensure!(
        frame.kind == kind,
        "expected a {} frame",
        if kind == TYPE_SASL { "SASL" } else { "AMQP" }
    );
    match &frame.body {
        Some(v) if v.descriptor() == Some(code) => Ok(v),
        Some(v) => bail!(
            "expected performative 0x{code:02x}, got 0x{:02x}",
            v.descriptor().unwrap_or(0)
        ),
        None => bail!("expected performative 0x{code:02x}, got an empty frame"),
    }
}

pub async fn header<R: AsyncRead + Unpin>(r: &mut R) -> Result<[u8; 8]> {
    let mut h = [0u8; 8];
    r.read_exact(&mut h).await.context("no protocol header")?;
    Ok(h)
}
