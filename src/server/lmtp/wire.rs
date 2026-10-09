//! LMTP (RFC 2033) framing shared by the server and the client: bounded CRLF lines, reply
//! rendering and parsing, path extraction and the message summary handed to handlers.
//!
//! Every limit here is enforced on the length the peer *sends*, before anything is buffered
//! for it: a line longer than [`MAX_LINE_BYTES`] is refused without being kept, and a message
//! body past the configured size is drained and discarded rather than stored.
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// RFC 5321 4.5.3.1.6: a text line is at most 1000 octets including CRLF.
pub const MAX_LINE_BYTES: usize = 1000;
/// RFC 5321 4.5.3.1.8 requires a server to accept at least 100 recipients.
pub const MAX_RECIPIENTS: usize = 100;
/// Default message size limit, advertised through the SIZE extension.
pub const DEFAULT_MAX_MESSAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Upper bound on the configurable message size limit.
pub const MAX_MESSAGE_BYTES_LIMIT: u64 = 64 * 1024 * 1024;
/// Body text handed to a handler; the rest is reported as truncated, never invented.
pub const MAX_BODY_FOR_HANDLER: usize = 64 * 1024;
/// Header fields handed to a handler.
pub const MAX_HEADERS: usize = 100;
/// Lines in one multi-line reply the client accepts.
pub const MAX_REPLY_LINES: usize = 64;
/// Connect, write and reply deadline.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of reading one bounded line.
pub enum Line {
    Text(String),
    TooLong,
    Eof,
}

/// Read one CRLF (or bare LF) terminated line, refusing past `MAX_LINE_BYTES` without
/// buffering the excess. A refused line is consumed through its terminator.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    deadline: Duration,
) -> Result<Line> {
    tokio::time::timeout(deadline, read_line_inner(reader))
        .await
        .context("LMTP read deadline")?
}

async fn read_line_inner<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Line> {
    let mut line = Vec::new();
    let mut overflow = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() && !overflow {
                return Ok(Line::Eof);
            }
            bail!("LMTP peer closed mid-line");
        }
        let (chunk, found) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (&available[..=i], true),
            None => (available, false),
        };
        let taken = chunk.len();
        if !overflow {
            if line.len() + taken > MAX_LINE_BYTES {
                overflow = true;
                line.clear();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        reader.consume(taken);
        if found {
            if overflow {
                return Ok(Line::TooLong);
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            return Ok(Line::Text(String::from_utf8_lossy(&line).into_owned()));
        }
    }
}

/// One reply line or a multi-line reply: `code`, an enhanced status code and text lines.
pub fn render(code: u16, enhanced: &str, text: &str) -> String {
    let text = sanitize(text);
    if enhanced.is_empty() {
        format!("{code} {text}\r\n")
    } else {
        format!("{code} {enhanced} {text}\r\n")
    }
}

pub fn render_multi(code: u16, lines: &[String]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let sep = if i + 1 == lines.len() { ' ' } else { '-' };
        out.push_str(&format!("{code}{sep}{}\r\n", sanitize(line)));
    }
    out
}

/// Strip control characters so handler text cannot inject a second reply line.
pub fn sanitize(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > 400 {
            break;
        }
        out.push(c);
    }
    out
}

/// A parsed reply: the code, and the text of each line without the code.
#[derive(Debug, Clone)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    pub fn text(&self) -> String {
        self.lines.join(" ")
    }
    pub fn positive(&self) -> bool {
        (200..400).contains(&self.code)
    }
    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "text": self.text()})
    }
}

/// Read one (possibly multi-line) reply. Codes must agree across lines.
pub async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Reply> {
    let mut lines = Vec::new();
    let mut code = None;
    loop {
        let line = match read_line(reader, IO_TIMEOUT).await? {
            Line::Text(line) => line,
            Line::TooLong => bail!("LMTP reply line exceeds {MAX_LINE_BYTES} bytes"),
            Line::Eof => bail!("LMTP server closed the connection before replying"),
        };
        let bytes = line.as_bytes();
        if bytes.len() < 3 || !bytes[..3].iter().all(u8::is_ascii_digit) {
            bail!("Malformed LMTP reply line");
        }
        let this: u16 = line[..3].parse()?;
        if let Some(previous) = code {
            if previous != this {
                bail!("LMTP multi-line reply changed code from {previous} to {this}");
            }
        }
        code = Some(this);
        let more = bytes.get(3) == Some(&b'-');
        if bytes.len() > 3 && !matches!(bytes[3], b' ' | b'-') {
            bail!("Malformed LMTP reply separator");
        }
        lines.push(line.get(4..).unwrap_or("").to_string());
        if lines.len() > MAX_REPLY_LINES {
            bail!("LMTP reply exceeds {MAX_REPLY_LINES} lines");
        }
        if !more {
            return Ok(Reply { code: this, lines });
        }
    }
}

/// Extract the path from `FROM:<a@b> SIZE=1` / `TO:<a@b>`; returns the address and parameters.
pub fn parse_path(argument: &str, keyword: &str) -> Option<(String, Vec<String>)> {
    let argument = argument.trim_start();
    if argument.len() < keyword.len() || !argument[..keyword.len()].eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = argument[keyword.len()..].trim_start();
    let rest = rest.strip_prefix('<')?;
    let end = rest.find('>')?;
    let address = rest[..end].trim().to_string();
    if address.len() > 256 || address.chars().any(|c| c.is_control() || c == ' ') {
        return None;
    }
    let params = rest[end + 1..]
        .split_whitespace()
        .map(str::to_string)
        .collect();
    Some((address, params))
}

/// A mailbox the client may put in MAIL FROM / RCPT TO: printable, no spaces or brackets.
pub fn valid_mailbox(address: &str, allow_empty: bool) -> bool {
    (allow_empty || !address.is_empty())
        && address.len() <= 256
        && !address
            .chars()
            .any(|c| c.is_control() || matches!(c, ' ' | '<' | '>'))
}

/// Summary of a received message for a handler: headers as fields, the body as text.
pub fn summarize(raw: &[u8]) -> (Map<String, Value>, String, bool) {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => match text.find("\n\n") {
            Some(i) => (&text[..i], &text[i + 2..]),
            None => (text.as_ref(), ""),
        },
    };
    let mut headers = Map::new();
    let mut current: Option<(String, String)> = None;
    let flush = |current: &mut Option<(String, String)>, headers: &mut Map<String, Value>| {
        if let Some((name, value)) = current.take() {
            if headers.len() < MAX_HEADERS && !headers.contains_key(&name) {
                headers.insert(name, Value::String(value.trim().to_string()));
            }
        }
    };
    for line in head.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        flush(&mut current, &mut headers);
        if let Some((name, value)) = line.split_once(':') {
            current = Some((name.trim().to_ascii_lowercase(), value.to_string()));
        }
    }
    flush(&mut current, &mut headers);
    let truncated = body.len() > MAX_BODY_FOR_HANDLER;
    let mut end = body.len().min(MAX_BODY_FOR_HANDLER);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    (headers, body[..end].to_string(), truncated)
}

/// Dot-stuff a message for DATA and terminate it (RFC 5321 4.5.2).
pub fn dot_stuff(message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 8);
    for line in message.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('.') {
            out.push(b'.');
        }
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // `split` yields a trailing empty piece for a terminating newline; drop its CRLF pair.
    if message.ends_with('\n') {
        out.truncate(out.len() - 2);
    }
    out.extend_from_slice(b".\r\n");
    out
}
