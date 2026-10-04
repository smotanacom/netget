//! RFC 3507 framing shared by both roles: ICAP heads, `Encapsulated` offsets, embedded HTTP
//! heads and chunked bodies with preview.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_HEAD_BYTES: usize = 64 * 1024;
pub const MAX_HEADERS: usize = 100;
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_CHUNKS: usize = 4096;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// An HTTP head (request or response) carried inside an ICAP message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpHead {
    /// "GET /path HTTP/1.1" or "HTTP/1.1 200 OK", split into three parts.
    pub start: [String; 3],
    pub headers: Vec<(String, String)>,
}

fn token(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn header_value_ok(v: &str) -> bool {
    !v.bytes()
        .any(|b| b == b'\r' || b == b'\n' || b == 0 || (b < 0x20 && b != b'\t') || b == 0x7f)
}

/// Parse a CRLF-separated head (start line + headers, without the blank line).
pub fn parse_head(text: &str) -> Result<HttpHead> {
    let mut lines = text.split("\r\n");
    let start = lines.next().context("empty head")?;
    let parts: Vec<&str> = start.splitn(3, ' ').collect();
    ensure!(
        parts.len() == 3 && parts.iter().take(2).all(|p| !p.is_empty()),
        "malformed start line '{start}'"
    );
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        ensure!(headers.len() < MAX_HEADERS, "too many headers");
        let (name, value) = line
            .split_once(':')
            .with_context(|| format!("header without ':' — '{line}'"))?;
        ensure!(token(name), "invalid header name '{name}'");
        let value = value.trim();
        ensure!(
            header_value_ok(value),
            "header {name} has a control character"
        );
        headers.push((name.to_owned(), value.to_owned()));
    }
    Ok(HttpHead {
        start: [parts[0].into(), parts[1].into(), parts[2].into()],
        headers,
    })
}

pub fn render_head(head: &HttpHead) -> Result<String> {
    for p in &head.start {
        ensure!(
            header_value_ok(p) && !p.contains('\t'),
            "start line part has a control character"
        );
    }
    ensure!(
        head.start
            .iter()
            .take(2)
            .all(|p| !p.is_empty() && !p.contains(' ')),
        "start line parts 1 and 2 must be single tokens"
    );
    ensure!(head.headers.len() <= MAX_HEADERS, "too many headers");
    let mut out = format!("{} {} {}\r\n", head.start[0], head.start[1], head.start[2]);
    for (k, v) in &head.headers {
        ensure!(token(k), "invalid header name '{k}'");
        ensure!(header_value_ok(v), "header {k} has a control character");
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    ensure!(
        out.len() <= MAX_HEAD_BYTES,
        "head exceeds {MAX_HEAD_BYTES} bytes"
    );
    Ok(out)
}

pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// `Encapsulated: req-hdr=0, res-hdr=137, res-body=296` in order.
pub fn parse_encapsulated(value: &str) -> Result<Vec<(String, usize)>> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for part in value.split(',') {
        let (k, v) = part
            .trim()
            .split_once('=')
            .context("Encapsulated entry without '='")?;
        ensure!(
            matches!(
                k,
                "req-hdr" | "res-hdr" | "req-body" | "res-body" | "opt-body" | "null-body"
            ),
            "unknown Encapsulated entity '{k}'"
        );
        let offset: usize = v
            .trim()
            .parse()
            .context("Encapsulated offset is not a number")?;
        ensure!(
            out.last().is_none_or(|(_, prev)| offset >= *prev),
            "Encapsulated offsets must not decrease"
        );
        ensure!(
            offset <= MAX_HEAD_BYTES,
            "Encapsulated offset beyond the head bound"
        );
        out.push((k.to_owned(), offset));
        ensure!(out.len() <= 4, "too many Encapsulated entities");
    }
    ensure!(!out.is_empty(), "empty Encapsulated header");
    let last = &out.last().expect("non-empty").0;
    ensure!(
        last.ends_with("-body"),
        "the last Encapsulated entity must be a body or null-body"
    );
    ensure!(
        out[..out.len() - 1]
            .iter()
            .all(|(k, _)| k.ends_with("-hdr")),
        "only the last Encapsulated entity may be a body"
    );
    Ok(out)
}

/// Read one CRLF-terminated head (ending in an empty line), bounded.
pub async fn read_head<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<String>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let n = (&mut *reader)
            .take((limit + 2 - head.len().min(limit)) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            ensure!(head.is_empty(), "peer closed inside a head");
            return Ok(None);
        }
        ensure!(line.ends_with(b"\r\n"), "head lines must end in CRLF");
        if line == b"\r\n" {
            if head.is_empty() {
                continue; // tolerate a stray CRLF between messages
            }
            break;
        }
        head.extend_from_slice(&line);
        ensure!(head.len() <= limit, "head exceeds {limit} bytes");
    }
    head.truncate(head.len() - 2);
    String::from_utf8(head)
        .map(Some)
        .context("head is not UTF-8")
}

/// Read exactly `len` bytes of an embedded HTTP head.
pub async fn read_exact_head<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    len: usize,
) -> Result<HttpHead> {
    ensure!(
        len <= MAX_HEAD_BYTES && len >= 4,
        "embedded head length out of bounds"
    );
    let mut bytes = vec![0u8; len];
    reader.read_exact(&mut bytes).await?;
    ensure!(
        bytes.ends_with(b"\r\n\r\n"),
        "embedded HTTP head must end with an empty line"
    );
    let text = std::str::from_utf8(&bytes[..len - 4]).context("embedded head is not UTF-8")?;
    parse_head(text)
}

/// Outcome of reading chunks: the data and whether the sender marked end of preview (`ieof`).
pub struct Chunks {
    pub data: Vec<u8>,
    pub ieof: bool,
}

/// Read chunks up to the zero chunk. `total` is what earlier reads already accepted.
pub async fn read_chunks<R: AsyncBufRead + Unpin>(reader: &mut R, total: usize) -> Result<Chunks> {
    let mut data = Vec::new();
    for _ in 0..MAX_CHUNKS {
        let mut line = Vec::new();
        (&mut *reader)
            .take(258)
            .read_until(b'\n', &mut line)
            .await?;
        ensure!(line.ends_with(b"\r\n"), "chunk size line must end in CRLF");
        let text =
            std::str::from_utf8(&line[..line.len() - 2]).context("chunk size is not ASCII")?;
        let (size, ext) = match text.split_once(';') {
            Some((s, e)) => (s.trim(), Some(e.trim())),
            None => (text.trim(), None),
        };
        ensure!(
            !size.is_empty() && size.len() <= 8 && size.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid chunk size '{size}'"
        );
        let size = usize::from_str_radix(size, 16)?;
        if size == 0 {
            let mut end = [0u8; 2];
            reader.read_exact(&mut end).await?;
            ensure!(&end == b"\r\n", "zero chunk must be followed by CRLF");
            return Ok(Chunks {
                data,
                ieof: ext == Some("ieof"),
            });
        }
        ensure!(ext.is_none(), "only the zero chunk may carry an extension");
        ensure!(
            size <= MAX_BODY_BYTES.saturating_sub(total + data.len()),
            "body exceeds {MAX_BODY_BYTES} bytes"
        );
        let start = data.len();
        data.resize(start + size, 0);
        reader.read_exact(&mut data[start..]).await?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).await?;
        ensure!(&crlf == b"\r\n", "chunk data must be followed by CRLF");
    }
    bail!("body exceeds {MAX_CHUNKS} chunks")
}

pub fn chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 16);
    if !body.is_empty() {
        out.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// Assemble an ICAP message: head, then the embedded HTTP heads and body, with the
/// `Encapsulated` header computed from their sizes.
pub fn message(
    start: &str,
    mut icap_headers: Vec<(String, String)>,
    req: Option<&HttpHead>,
    res: Option<&HttpHead>,
    body: Option<&[u8]>,
    body_kind: &str,
) -> Result<Vec<u8>> {
    let mut encapsulated = Vec::new();
    let mut section = Vec::new();
    if let Some(h) = req {
        encapsulated.push(format!("req-hdr={}", section.len()));
        section.extend_from_slice(render_head(h)?.as_bytes());
    }
    if let Some(h) = res {
        encapsulated.push(format!("res-hdr={}", section.len()));
        section.extend_from_slice(render_head(h)?.as_bytes());
    }
    match body {
        Some(b) => {
            encapsulated.push(format!("{body_kind}={}", section.len()));
            section.extend_from_slice(&chunked(b));
        }
        None => encapsulated.push(format!("null-body={}", section.len())),
    }
    icap_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("Encapsulated"));
    icap_headers.push(("Encapsulated".into(), encapsulated.join(", ")));
    let mut out = format!("{start}\r\n");
    for (k, v) in &icap_headers {
        ensure!(token(k) && header_value_ok(v), "invalid ICAP header {k}");
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&section);
    Ok(bytes)
}

pub async fn write_all<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> Result<()> {
    tokio::time::timeout(IO_TIMEOUT, async {
        writer.write_all(bytes).await?;
        writer.flush().await
    })
    .await
    .context("ICAP write deadline")??;
    Ok(())
}

pub fn head_json(h: &HttpHead, request: bool) -> Value {
    let headers: Vec<Value> = h.headers.iter().map(|(k, v)| json!([k, v])).collect();
    if request {
        json!({"method": h.start[0], "uri": h.start[1], "version": h.start[2], "headers": headers})
    } else {
        json!({"version": h.start[0], "status": h.start[1].parse::<u16>().unwrap_or(0), "reason": h.start[2], "headers": headers})
    }
}

fn headers_from(v: &Value) -> Result<Vec<(String, String)>> {
    match v {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|i| {
                let pair = i
                    .as_array()
                    .filter(|p| p.len() == 2)
                    .context("headers must be [[name, value], ...]")?;
                Ok((
                    pair[0]
                        .as_str()
                        .context("header name must be a string")?
                        .to_owned(),
                    pair[1]
                        .as_str()
                        .context("header value must be a string")?
                        .to_owned(),
                ))
            })
            .collect(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                Ok((
                    k.clone(),
                    v.as_str()
                        .context("header value must be a string")?
                        .to_owned(),
                ))
            })
            .collect(),
        _ => bail!("headers must be an array of [name, value] or an object"),
    }
}

/// Structured HTTP request → head.
pub fn request_head(v: &Value) -> Result<HttpHead> {
    let method = v["method"].as_str().unwrap_or("GET");
    ensure!(token(method), "invalid HTTP method");
    let uri = v["uri"].as_str().context("http_request.uri is required")?;
    let version = v["version"].as_str().unwrap_or("HTTP/1.1");
    let head = HttpHead {
        start: [method.into(), uri.into(), version.into()],
        headers: headers_from(&v["headers"])?,
    };
    render_head(&head)?;
    Ok(head)
}

/// Structured HTTP response → head.
pub fn response_head(v: &Value) -> Result<HttpHead> {
    let status = v["status"]
        .as_u64()
        .context("http_response.status is required")?;
    ensure!((100..=599).contains(&status), "HTTP status out of range");
    let reason = v["reason"].as_str().unwrap_or("");
    let version = v["version"].as_str().unwrap_or("HTTP/1.1");
    let head = HttpHead {
        start: [version.into(), status.to_string(), reason.into()],
        headers: headers_from(&v["headers"])?,
    };
    render_head(&head)?;
    Ok(head)
}

/// Body as event data: text when it is UTF-8, otherwise its size only.
pub fn body_json(body: &[u8]) -> Value {
    match std::str::from_utf8(body) {
        Ok(t) => json!({"body_text": t, "body_bytes": body.len()}),
        Err(_) => json!({"body_binary": true, "body_bytes": body.len()}),
    }
}

/// Replace or set Content-Length on a head carrying a body we re-encode.
pub fn set_length(head: &mut HttpHead, len: usize) {
    head.headers.retain(|(k, _)| {
        !k.eq_ignore_ascii_case("Content-Length") && !k.eq_ignore_ascii_case("Transfer-Encoding")
    });
    head.headers
        .push(("Content-Length".into(), len.to_string()));
}
