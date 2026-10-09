//! Typed NSQ V2 requests and a bounded, complete-frame reader.
use crate::server::nsq::wire::{self as nsq, Command, Frame};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
pub const DEADLINE: Duration = Duration::from_secs(15);
pub const MAX_FOLLOWUPS: u8 = 4;
pub const MAX_BATCH_MESSAGES: usize = 1024;

pub fn identify(heartbeat_ms: i64) -> Command {
    Command::Identify(
        serde_json::to_vec(&serde_json::json!({
            "client_id":"netget", "hostname":"netget", "user_agent":"netget",
            "feature_negotiation":true, "heartbeat_interval":heartbeat_ms,
            "output_buffer_size":-1, "tls_v1":false, "snappy":false, "deflate":false
        }))
        .expect("literal identify JSON"),
    )
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("NSQ {key} must be a string"))
}
fn name(v: &Value, key: &str) -> Result<String> {
    let s = text(v, key)?;
    anyhow::ensure!(nsq::valid_name(s), "Invalid NSQ {key}");
    Ok(s.into())
}
fn number(v: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    let n = match v.get(key) {
        None => default,
        Some(n) => n
            .as_u64()
            .with_context(|| format!("NSQ {key} must be a non-negative integer"))?,
    };
    anyhow::ensure!(n <= max, "NSQ {key} exceeds {max}");
    Ok(n)
}
fn id(v: &Value) -> Result<String> {
    let id = text(v, "message_id")?;
    anyhow::ensure!(
        id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "NSQ message_id must be 16 hexadecimal characters"
    );
    Ok(id.into())
}
fn body(s: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(
        !s.is_empty() && s.len() <= nsq::MAX_MSG_SIZE,
        "NSQ body must contain 1..={} UTF-8 bytes",
        nsq::MAX_MSG_SIZE
    );
    Ok(s.as_bytes().to_vec())
}
pub fn request(v: &Value) -> Result<Command> {
    anyhow::ensure!(v["type"] == "nsq_request", "Unknown NSQ client action");
    Ok(match text(v, "operation")? {
        "publish" => Command::Pub {
            topic: name(v, "topic")?,
            body: body(text(v, "body")?)?,
        },
        "publish_many" => {
            let messages = v["messages"]
                .as_array()
                .context("NSQ messages must be an array")?;
            anyhow::ensure!(
                !messages.is_empty() && messages.len() <= MAX_BATCH_MESSAGES,
                "NSQ messages count out of bounds"
            );
            let mut total = 4usize;
            let mut bodies = Vec::with_capacity(messages.len());
            for msg in messages {
                let b = body(msg.as_str().context("NSQ message must be UTF-8 text")?)?;
                total = total
                    .checked_add(4 + b.len())
                    .context("NSQ MPUB size overflow")?;
                anyhow::ensure!(
                    total <= nsq::MAX_BODY_SIZE,
                    "NSQ MPUB body exceeds {} bytes",
                    nsq::MAX_BODY_SIZE
                );
                bodies.push(b);
            }
            Command::Mpub {
                topic: name(v, "topic")?,
                messages: bodies,
            }
        }
        "publish_deferred" => Command::Dpub {
            topic: name(v, "topic")?,
            body: body(text(v, "body")?)?,
            defer_ms: number(v, "delay_ms", 0, nsq::MAX_REQ_TIMEOUT_MS as u64)? as i64,
        },
        "subscribe" => Command::Sub {
            topic: name(v, "topic")?,
            channel: name(v, "channel")?,
        },
        "ready" => Command::Rdy(number(v, "count", 1, nsq::MAX_RDY_COUNT)?),
        "finish" => Command::Fin(id(v)?),
        "requeue" => Command::Req {
            id: id(v)?,
            timeout_ms: number(v, "delay_ms", 0, nsq::MAX_REQ_TIMEOUT_MS as u64)? as i64,
        },
        "touch" => Command::Touch(id(v)?),
        "close" => Command::Cls,
        "nop" => Command::Nop,
        _ => bail!("Unknown NSQ operation"),
    })
}
pub fn expects_reply(command: &Command) -> bool {
    matches!(
        command,
        Command::Identify(_)
            | Command::Pub { .. }
            | Command::Mpub { .. }
            | Command::Dpub { .. }
            | Command::Sub { .. }
            | Command::Cls
            | Command::Auth(_)
    )
}

/// Read each frame in one owned reader task. Never cancel this future to poll actions.
/// The first byte may wait on an idle connection; partial frames have a 15 s deadline.
pub async fn frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Frame>> {
    let mut size = [0u8; 4];
    if reader.read(&mut size[..1]).await? == 0 {
        return Ok(None);
    }
    tokio::time::timeout(DEADLINE, async {
        reader.read_exact(&mut size[1..]).await?;
        let size = u32::from_be_bytes(size) as usize;
        anyhow::ensure!(
            (4..=4 + nsq::MAX_FRAME_DATA).contains(&size),
            "NSQ frame size outside bounds"
        );
        let mut frame = vec![0; size];
        reader.read_exact(&mut frame).await?;
        let frame_type = u32::from_be_bytes(frame[..4].try_into()?);
        anyhow::ensure!(frame_type <= nsq::FRAME_MESSAGE, "Unknown NSQ frame type");
        Ok(Some(Frame {
            frame_type,
            data: frame[4..].to_vec(),
        }))
    })
    .await
    .context("NSQ partial frame deadline")?
}
