//! Wire helpers for the classic inetd services: Echo (RFC 862), Discard (RFC 863),
//! Chargen (RFC 864), QOTD (RFC 865), Daytime (RFC 867) and Time (RFC 868).
use anyhow::{bail, ensure, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// One TCP read, and the largest UDP datagram accepted.
pub const READ_CHUNK: usize = 8192;
/// RFC 865: a quote should be limited to 512 characters.
pub const MAX_QUOTE_CHARS: usize = 512;
/// Daytime text a handler may supply.
pub const MAX_DAYTIME_CHARS: usize = 256;
/// RFC 864: a UDP chargen reply is between 0 and 512 characters.
pub const MAX_CHARGEN_UDP: usize = 512;
/// Upper bound on a chargen line.
pub const MAX_CHARGEN_LINE: usize = 512;
/// Seconds between 1900-01-01 and 1970-01-01 (RFC 868).
pub const SECONDS_1900_TO_1970: u64 = 2_208_988_800;
/// Write and per-reply deadline.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes a client reads from a stream service (Daytime, QOTD, Time) before giving up.
pub const MAX_STREAM_REPLY: usize = 64 * 1024;

/// The RFC 864 character set: the 95 printable ASCII characters, space first.
pub fn default_charset() -> String {
    (0x20u8..=0x7e).map(char::from).collect()
}

/// Line `n` of the rotating chargen pattern: `width` characters starting `n` into the set.
pub fn chargen_line(charset: &[u8], width: usize, n: usize) -> Vec<u8> {
    let mut line = Vec::with_capacity(width + 2);
    for i in 0..width {
        line.push(charset[(n + i) % charset.len()]);
    }
    line.extend_from_slice(b"\r\n");
    line
}

/// Whether text follows the rotating pattern of `charset` (any starting offset, any width).
pub fn conforms_to_chargen(text: &str, charset: &str) -> bool {
    let set = charset.as_bytes();
    let lines: Vec<&str> = text.split("\r\n").filter(|l| !l.is_empty()).collect();
    if lines.len() < 2 {
        return !lines.is_empty() && lines[0].bytes().all(|b| set.contains(&b));
    }
    let start = |line: &str| {
        line.bytes()
            .next()
            .and_then(|b| set.iter().position(|c| *c == b))
    };
    lines
        .windows(2)
        .all(|pair| match (start(pair[0]), start(pair[1])) {
            (Some(a), Some(b)) => b == (a + 1) % set.len(),
            _ => false,
        })
        && lines.iter().all(|line| {
            let first = start(line).unwrap_or(0);
            line.bytes()
                .enumerate()
                .all(|(i, b)| set[(first + i) % set.len()] == b)
        })
}

/// Validate a handler's chargen character set: printable ASCII, 1 to 95 characters.
pub fn charset(value: Option<&Value>) -> Result<String> {
    match value {
        None | Some(Value::Null) => Ok(default_charset()),
        Some(Value::String(s)) => {
            ensure!(
                !s.is_empty() && s.len() <= 95,
                "charset must hold 1 to 95 characters"
            );
            ensure!(
                s.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                "charset must be printable ASCII"
            );
            Ok(s.clone())
        }
        _ => bail!("charset must be a string"),
    }
}

/// RFC 867 suggests "Weekday, Month Day, Year HH:MM:SS-ZONE".
pub fn daytime_now() -> String {
    chrono::Utc::now()
        .format("%A, %B %-d, %Y %H:%M:%S-UTC")
        .to_string()
}

/// RFC 868 time: seconds since 1900 as a 32-bit big-endian number (wrapping in 2036).
pub fn time_bytes(unix_seconds: i64) -> [u8; 4] {
    let seconds = (unix_seconds as i128 + SECONDS_1900_TO_1970 as i128).rem_euclid(1 << 32) as u32;
    seconds.to_be_bytes()
}

/// Unix seconds from a handler's `time_reply`: `unix_seconds`, `iso8601`, or now.
pub fn requested_time(answer: &Value) -> Result<i64> {
    if let Some(seconds) = answer.get("unix_seconds").filter(|v| !v.is_null()) {
        return seconds
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("unix_seconds must be an integer"));
    }
    if let Some(iso) = answer.get("iso8601").filter(|v| !v.is_null()) {
        let iso = iso
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("iso8601 must be a string"))?;
        return Ok(chrono::DateTime::parse_from_rfc3339(iso)
            .map_err(|e| anyhow::anyhow!("iso8601 is not RFC 3339: {e}"))?
            .timestamp());
    }
    Ok(chrono::Utc::now().timestamp())
}

/// Describe a received RFC 868 value for a handler.
pub fn describe_time(bytes: [u8; 4]) -> Value {
    let since_1900 = u32::from_be_bytes(bytes) as i64;
    let unix = since_1900 - SECONDS_1900_TO_1970 as i64;
    let iso = chrono::DateTime::from_timestamp(unix, 0).map(|t| t.to_rfc3339());
    json!({"seconds_since_1900": since_1900, "unix_seconds": unix, "iso8601": iso})
}

/// Data for an event: text when it is printable UTF-8, hex otherwise, with the encoding named.
pub fn encode(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(text)
            if !text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
        {
            (text.to_string(), "utf8")
        }
        _ => (hex::encode(bytes), "hex"),
    }
}

/// Bytes from an action's `data` and `encoding` (`utf8`, the default, or `hex`).
pub fn decode(data: &str, encoding: Option<&str>) -> Result<Vec<u8>> {
    match encoding.unwrap_or("utf8") {
        "utf8" => Ok(data.as_bytes().to_vec()),
        "hex" => hex::decode(data).map_err(|e| anyhow::anyhow!("data is not valid hex: {e}")),
        other => bail!("encoding must be utf8 or hex, not {other}"),
    }
}

/// Handler text for a reply line: no control characters, at most `limit` characters.
pub fn line(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
