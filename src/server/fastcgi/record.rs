//! FastCGI 1.0 records, name-value pairs and CGI responses, shared by the responder and the
//! client. Every length is checked against what the peer announced before anything is
//! allocated for it.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 8;
/// Largest content a record can carry; stream data is split at a multiple of 8 below it.
pub const MAX_CONTENT: usize = 65_535;
const CHUNK: usize = 65_528;

pub const BEGIN_REQUEST: u8 = 1;
pub const ABORT_REQUEST: u8 = 2;
pub const END_REQUEST: u8 = 3;
pub const PARAMS: u8 = 4;
pub const STDIN: u8 = 5;
pub const STDOUT: u8 = 6;
pub const STDERR: u8 = 7;
pub const DATA: u8 = 8;
pub const GET_VALUES: u8 = 9;
pub const GET_VALUES_RESULT: u8 = 10;
pub const UNKNOWN_TYPE: u8 = 11;

pub const ROLE_RESPONDER: u16 = 1;
pub const ROLE_AUTHORIZER: u16 = 2;
pub const ROLE_FILTER: u16 = 3;
pub const KEEP_CONN: u8 = 1;

pub const REQUEST_COMPLETE: u8 = 0;
pub const CANT_MPX_CONN: u8 = 1;
pub const OVERLOADED: u8 = 2;
pub const UNKNOWN_ROLE: u8 = 3;

/// Total bytes of name-value pairs accepted for one request's PARAMS stream.
pub const MAX_PARAMS_BYTES: usize = 64 * 1024;
pub const MAX_PAIRS: usize = 512;
/// Request body (STDIN) and response (STDOUT) bound.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

pub fn protocol_status_name(s: u8) -> &'static str {
    match s {
        REQUEST_COMPLETE => "request_complete",
        CANT_MPX_CONN => "cant_mpx_conn",
        OVERLOADED => "overloaded",
        UNKNOWN_ROLE => "unknown_role",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub kind: u8,
    pub request_id: u16,
    pub content: Vec<u8>,
}

/// Read one record. `Ok(None)` is a clean end of stream at a record boundary.
pub async fn read_record<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Record>> {
    let mut header = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        let n = r.read(&mut header[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            bail!("stream ended inside a record header");
        }
        got += n;
    }
    ensure!(
        header[0] == VERSION,
        "unsupported FastCGI version {}",
        header[0]
    );
    let content_len = u16::from_be_bytes([header[4], header[5]]) as usize;
    let padding = header[6] as usize;
    let mut body = vec![0u8; content_len + padding];
    r.read_exact(&mut body)
        .await
        .context("stream ended inside a record")?;
    body.truncate(content_len);
    Ok(Some(Record {
        kind: header[1],
        request_id: u16::from_be_bytes([header[2], header[3]]),
        content: body,
    }))
}

/// One record, padded to a multiple of 8 as the specification recommends.
pub fn encode(kind: u8, request_id: u16, content: &[u8]) -> Vec<u8> {
    assert!(
        content.len() <= MAX_CONTENT,
        "record content over 65535 bytes"
    );
    let padding = (8 - content.len() % 8) % 8;
    let mut out = Vec::with_capacity(HEADER_LEN + content.len() + padding);
    out.push(VERSION);
    out.push(kind);
    out.extend_from_slice(&request_id.to_be_bytes());
    out.extend_from_slice(&(content.len() as u16).to_be_bytes());
    out.push(padding as u8);
    out.push(0);
    out.extend_from_slice(content);
    out.resize(out.len() + padding, 0);
    out
}

/// A whole stream (PARAMS, STDIN, STDOUT, STDERR): as many records as it takes, then the empty
/// record that ends it.
pub fn encode_stream(kind: u8, request_id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(CHUNK) {
        out.extend(encode(kind, request_id, chunk));
    }
    out.extend(encode(kind, request_id, &[]));
    out
}

pub fn begin_request(role: u16, flags: u8) -> [u8; 8] {
    let r = role.to_be_bytes();
    [r[0], r[1], flags, 0, 0, 0, 0, 0]
}

pub fn end_request(app_status: u32, protocol_status: u8) -> [u8; 8] {
    let a = app_status.to_be_bytes();
    [a[0], a[1], a[2], a[3], protocol_status, 0, 0, 0]
}

pub fn unknown_type(kind: u8) -> [u8; 8] {
    [kind, 0, 0, 0, 0, 0, 0, 0]
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    if n < 128 {
        out.push(n as u8);
    } else {
        out.extend_from_slice(&((n as u32) | 0x8000_0000).to_be_bytes());
    }
}

pub fn encode_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in pairs {
        put_len(&mut out, k.len());
        put_len(&mut out, v.len());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    out
}

/// Decode name-value pairs. Names must be printable ASCII; values may be any bytes and are
/// decoded lossily (a CGI value is text in practice).
pub fn decode_pairs(data: &[u8]) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut i = 0;
    let len = |i: &mut usize| -> Result<usize> {
        let b = *data.get(*i).context("truncated name-value length")?;
        if b < 0x80 {
            *i += 1;
            Ok(b as usize)
        } else {
            let bytes = data
                .get(*i..*i + 4)
                .context("truncated name-value length")?;
            *i += 4;
            Ok(
                (u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) & 0x7fff_ffff)
                    as usize,
            )
        }
    };
    while i < data.len() {
        ensure!(
            out.len() < MAX_PAIRS,
            "more than {MAX_PAIRS} name-value pairs"
        );
        let nl = len(&mut i)?;
        let vl = len(&mut i)?;
        ensure!(
            nl <= data.len() && vl <= data.len() && i + nl + vl <= data.len(),
            "name-value pair longer than its stream"
        );
        let name = &data[i..i + nl];
        ensure!(
            !name.is_empty() && name.iter().all(|b| b.is_ascii_graphic()),
            "name-value pair name is not printable ASCII"
        );
        let value = String::from_utf8_lossy(&data[i + nl..i + nl + vl]).into_owned();
        out.push((String::from_utf8_lossy(name).into_owned(), value));
        i += nl + vl;
    }
    Ok(out)
}

/// Body bytes for an event: text when they are UTF-8, hex otherwise, and which.
pub fn body_text(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), "utf8"),
        Err(_) => (hex::encode(bytes), "hex"),
    }
}

/// Body bytes from an action's `body` and `body_encoding`.
pub fn decode_body(v: &Value) -> Result<Vec<u8>> {
    let body = match v.get("body") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::String(s)) => s,
        Some(_) => bail!("body must be a string"),
    };
    let bytes = match v["body_encoding"].as_str().unwrap_or("utf8") {
        "utf8" => body.as_bytes().to_vec(),
        "hex" => hex::decode(body).context("body is not valid hex")?,
        other => bail!("body_encoding must be utf8 or hex, not {other}"),
    };
    ensure!(bytes.len() <= MAX_BODY_BYTES, "body over 1 MiB");
    Ok(bytes)
}

/// Header names are HTTP tokens; values carry no line breaks or other controls.
pub fn check_headers(headers: Option<&Value>) -> Result<Vec<(String, String)>> {
    let Some(h) = headers.filter(|h| !h.is_null()) else {
        return Ok(vec![]);
    };
    let map = h.as_object().context("headers must be an object")?;
    ensure!(map.len() <= 64, "at most 64 headers");
    map.iter()
        .map(|(k, v)| {
            ensure!(
                !k.is_empty()
                    && k.len() <= 256
                    && k.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
                "header name {k:?} is not an HTTP token"
            );
            let v = v
                .as_str()
                .with_context(|| format!("header {k} must be a string"))?;
            ensure!(
                v.len() <= 8192 && !v.chars().any(|c| c.is_control() && c != '\t'),
                "header {k} contains a control character"
            );
            Ok((k.clone(), v.to_owned()))
        })
        .collect()
}

fn reason(status: u16) -> &'static str {
    hyper::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Unknown")
}

/// A CGI response (RFC 3875 §6): `Status:`, the handler's headers, a blank line, the body.
pub fn build_cgi_response(status: u16, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("Status: {status} {}\r\n", reason(status)).into_bytes();
    let mut typed = false;
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("status") {
            continue;
        }
        typed |= k.eq_ignore_ascii_case("content-type");
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if !typed {
        out.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Parse a CGI response: header lines up to the first blank line (CRLF or LF), `Status`
/// (default 200, or 302 with a `Location`), the rest the body.
pub fn parse_cgi_response(stdout: &[u8]) -> Result<(u16, Map<String, Value>, Vec<u8>)> {
    let (head, body) = match find(stdout, b"\r\n\r\n") {
        Some(i) => (&stdout[..i], &stdout[i + 4..]),
        None => match find(stdout, b"\n\n") {
            Some(i) => (&stdout[..i], &stdout[i + 2..]),
            None => bail!("CGI response has no end of headers"),
        },
    };
    ensure!(head.len() <= 64 * 1024, "CGI response headers over 64 KiB");
    let head = std::str::from_utf8(head).context("CGI response headers are not UTF-8")?;
    let mut headers = Map::new();
    let mut status = None;
    for line in head.split('\n').map(|l| l.trim_end_matches('\r')) {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .with_context(|| format!("CGI header line without a colon: {line:?}"))?;
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("status") {
            let code = v.split_whitespace().next().unwrap_or("");
            let code: u16 = code.parse().context("Status is not a number")?;
            ensure!((100..=599).contains(&code), "Status {code} out of range");
            status = Some(code);
        } else {
            headers.insert(k.to_ascii_lowercase(), json!(v));
        }
    }
    let status = status.unwrap_or(if headers.contains_key("location") {
        302
    } else {
        200
    });
    Ok((status, headers, body.to_vec()))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
