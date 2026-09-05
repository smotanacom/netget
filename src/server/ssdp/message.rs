//! HTTPU (HTTP over UDP) message codec for SSDP.
//!
//! Pure functions only — no sockets, no LLM, no state. Everything here is deterministic
//! except [`response_delay_ms`], which draws the MX jitter and is separated out precisely so
//! the *range* can be tested without a clock.
//!
//! SSDP (UPnP Device Architecture 1.1, §1) is HTTP/1.1 syntax carried in a single UDP
//! datagram. There is no body, no chunking and no content length: a message is a start line,
//! a run of `NAME: value` header lines, and a blank line. That is the whole grammar, which
//! is why this is hand-rolled rather than reaching for an HTTP parser — a real HTTP parser
//! wants a body framing that SSDP does not have.

use std::fmt::Write as _;

/// The IPv4 SSDP multicast group (UDA 1.1 §1.1).
pub const SSDP_GROUP_V4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 255, 250);

/// The IPv6 link-local SSDP multicast group, `FF02::C`.
pub const SSDP_GROUP_V6: std::net::Ipv6Addr =
    std::net::Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xc);

/// The well-known SSDP port.
pub const SSDP_PORT: u16 = 1900;

/// UDA 1.1 §1.3.2: a device MUST NOT wait longer than 5 seconds, and MUST treat an MX
/// larger than 5 as 5.
pub const MAX_MX_SECONDS: u32 = 5;

/// Largest datagram we will parse. A datagram longer than this is dropped rather than
/// truncated: a half-message parses into plausible-looking headers, which is worse than
/// nothing.
pub const MAX_MESSAGE_LEN: usize = 8192;

/// A parsed HTTPU message.
///
/// Header order is preserved as received, because a model asked to reason about a device's
/// announcement should see what the device actually sent, not a normalised rewrite of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpuMessage {
    /// The start line verbatim, e.g. `M-SEARCH * HTTP/1.1` or `HTTP/1.1 200 OK`.
    pub start_line: String,
    /// The request method, uppercased — `M-SEARCH` or `NOTIFY`. `None` for a status line
    /// (`HTTP/1.1 200 OK`), which is a *response* and must never be answered.
    pub method: Option<String>,
    /// Headers in receive order, as `(name, value)` with the name uppercased and the value
    /// trimmed.
    pub headers: Vec<(String, String)>,
}

/// Why a datagram was not a usable SSDP message. Every variant is a reason to drop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Zero bytes, or nothing but blank lines.
    Empty,
    /// Longer than [`MAX_MESSAGE_LEN`].
    TooLong(usize),
    /// Not valid UTF-8. SSDP is a text protocol; binary here is not an SSDP message.
    NotUtf8,
    /// A header line with no `:` separator.
    MalformedHeader(String),
    /// A header line before any start line, or a start line that is not a request line or a
    /// status line.
    MalformedStartLine(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Empty => write!(f, "datagram is empty"),
            ParseError::TooLong(n) => {
                write!(
                    f,
                    "datagram is {n} bytes, more than the {MAX_MESSAGE_LEN} accepted"
                )
            }
            ParseError::NotUtf8 => {
                write!(f, "datagram is not valid UTF-8; SSDP is a text protocol")
            }
            ParseError::MalformedHeader(line) => {
                write!(f, "header line has no ':' separator: {line:?}")
            }
            ParseError::MalformedStartLine(line) => write!(f, "unusable start line: {line:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl HttpuMessage {
    /// Case-insensitive header lookup, first occurrence wins.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Headers as a JSON object, for the event payload.
    ///
    /// A map, never a pre-rendered blob: the root `CLAUDE.md` rule about structured fields
    /// applies to this exactly. A duplicated header keeps its first value here and is still
    /// visible in full through the protocol's own log.
    pub fn headers_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (k, v) in &self.headers {
            map.entry(k.clone())
                .or_insert_with(|| serde_json::Value::String(v.clone()));
        }
        serde_json::Value::Object(map)
    }

    /// The MX header as an integer, clamped to [`MAX_MX_SECONDS`] per UDA 1.1 §1.3.2.
    ///
    /// `None` when absent or unparseable. A negative or garbage MX is *not* silently read as
    /// zero: the caller distinguishes "no MX given" from "MX 0" in the event it shows the
    /// model.
    pub fn mx(&self) -> Option<u32> {
        self.header("MX")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .map(|v| v.min(MAX_MX_SECONDS))
    }
}

/// Parse a datagram into an [`HttpuMessage`].
pub fn parse(data: &[u8]) -> Result<HttpuMessage, ParseError> {
    if data.len() > MAX_MESSAGE_LEN {
        return Err(ParseError::TooLong(data.len()));
    }
    let text = std::str::from_utf8(data).map_err(|_| ParseError::NotUtf8)?;

    // Real implementations are sloppy about line endings, so accept a bare LF as well as
    // CRLF. Everything we *emit* is strictly CRLF.
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));

    let start_line = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l.trim().to_string(),
            None => return Err(ParseError::Empty),
        }
    };

    let method = classify_start_line(&start_line)?;

    let mut headers = Vec::new();
    for line in lines {
        // A blank line ends the header block. Anything after it is not part of the message
        // (SSDP has no body) and is ignored.
        if line.trim().is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| ParseError::MalformedHeader(line.to_string()))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(ParseError::MalformedHeader(line.to_string()));
        }
        headers.push((name.to_ascii_uppercase(), value.trim().to_string()));
    }

    Ok(HttpuMessage {
        start_line,
        method,
        headers,
    })
}

/// Decide whether a start line is a request (and which method) or a status line.
///
/// Anything else is refused. In particular an unrecognised method is an error rather than
/// `None`: `None` means "this is a response", and conflating the two would let a stray
/// `FOO * HTTP/1.1` take the response path and be silently ignored instead of logged.
fn classify_start_line(line: &str) -> Result<Option<String>, ParseError> {
    if line.starts_with("HTTP/") {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| ParseError::MalformedStartLine(line.to_string()))?
        .to_ascii_uppercase();
    // `<method> <target> HTTP/<version>` — three fields, and the last must name HTTP.
    let target = parts.next();
    let version = parts.next();
    match (target, version) {
        (Some(_), Some(v)) if v.starts_with("HTTP/") => Ok(Some(method)),
        _ => Err(ParseError::MalformedStartLine(line.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A header name or value that would break the message framing.
///
/// CR and LF in a model-supplied header would let one action emit several headers, or end
/// the message early and append a second one — the HTTP response-splitting shape. Rejected
/// rather than sanitised, so the model is told what it did.
pub fn validate_header_piece(kind: &str, name: &str, s: &str) -> anyhow::Result<()> {
    if s.contains('\r') || s.contains('\n') {
        return Err(anyhow::anyhow!(
            "SSDP header {kind} for '{name}' contains a carriage return or newline, which \
             would split the message into two. Remove it."
        ));
    }
    Ok(())
}

/// The `DATE` header value in the RFC 1123 form HTTP requires.
///
/// `chrono`'s `to_rfc2822` renders the offset as `+0000`; HTTP-date wants the literal
/// `GMT`, so the format is spelled out. `%a`/`%b` are English in chrono regardless of
/// locale, which is what the grammar requires.
pub fn http_date(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Fields of the unicast `HTTP/1.1 200 OK` answer to an M-SEARCH (UDA 1.1 §1.3.3).
#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub st: String,
    pub usn: String,
    pub location: String,
    pub server: String,
    pub max_age: u32,
    pub date: String,
    /// Extra headers, appended in order after the mandatory set.
    pub extra_headers: Vec<(String, String)>,
}

/// Render an M-SEARCH response.
///
/// The mandatory header set is exactly the one UDA 1.1 §1.3.3 lists: `CACHE-CONTROL`,
/// `DATE`, `EXT`, `LOCATION`, `SERVER`, `ST`, `USN`. `EXT` is deliberately emitted with an
/// empty value — it is a marker header, and a control point that does not see it treats the
/// response as coming from a device that ignored the `MAN` extension.
pub fn render_search_response(r: &SearchResponse) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("HTTP/1.1 200 OK\r\n");
    let _ = writeln!(out, "CACHE-CONTROL: max-age={}\r", r.max_age);
    let _ = writeln!(out, "DATE: {}\r", r.date);
    out.push_str("EXT:\r\n");
    let _ = writeln!(out, "LOCATION: {}\r", r.location);
    let _ = writeln!(out, "SERVER: {}\r", r.server);
    let _ = writeln!(out, "ST: {}\r", r.st);
    let _ = writeln!(out, "USN: {}\r", r.usn);
    for (k, v) in &r.extra_headers {
        let _ = writeln!(out, "{k}: {v}\r");
    }
    out.push_str("\r\n");
    out
}

/// Fields of a multicast `NOTIFY * HTTP/1.1` announcement (UDA 1.1 §1.2).
#[derive(Debug, Clone)]
pub struct NotifyMessage {
    pub host: String,
    pub nt: String,
    pub nts: String,
    pub usn: String,
    /// Required for `ssdp:alive` and `ssdp:update`; omitted from `ssdp:byebye`.
    pub location: Option<String>,
    pub server: Option<String>,
    pub max_age: Option<u32>,
}

/// Render a NOTIFY.
///
/// A `ssdp:byebye` carries **only** `HOST`, `NT`, `NTS` and `USN` (UDA 1.1 §1.2.3). Sending
/// `LOCATION` and `CACHE-CONTROL` on a byebye contradicts the message: the announcement says
/// the device is going away while the headers describe where to reach it and for how long to
/// cache that. The caller passes `None` for those fields on a byebye and this renders
/// accordingly; it does not second-guess the caller beyond that.
pub fn render_notify(n: &NotifyMessage) -> String {
    let mut out = String::with_capacity(256);
    out.push_str("NOTIFY * HTTP/1.1\r\n");
    let _ = writeln!(out, "HOST: {}\r", n.host);
    if let Some(max_age) = n.max_age {
        let _ = writeln!(out, "CACHE-CONTROL: max-age={max_age}\r");
    }
    if let Some(location) = &n.location {
        let _ = writeln!(out, "LOCATION: {location}\r");
    }
    let _ = writeln!(out, "NT: {}\r", n.nt);
    let _ = writeln!(out, "NTS: {}\r", n.nts);
    if let Some(server) = &n.server {
        let _ = writeln!(out, "SERVER: {server}\r");
    }
    let _ = writeln!(out, "USN: {}\r", n.usn);
    out.push_str("\r\n");
    out
}

// ---------------------------------------------------------------------------
// MX jitter
// ---------------------------------------------------------------------------

/// Upper bound, in milliseconds, on how long a response to this M-SEARCH may be held back.
///
/// UDA 1.1 §1.3.3 says a device waits a random interval between 0 and MX seconds before
/// answering a multicast M-SEARCH; the whole point is to spread a whole network's answers so
/// the control point is not flooded. That behaviour is what makes NetGet's traffic look like
/// a real device's rather than like a machine answering instantly every time.
///
/// `cap_ms` is the operator's ceiling (`max_response_delay_ms`), because a faithful 5-second
/// wait makes every test that touches this protocol five seconds slower. A cap of 0 disables
/// the jitter entirely.
pub fn response_delay_bound_ms(mx: Option<u32>, cap_ms: u64) -> u64 {
    let mx_ms = u64::from(mx.unwrap_or(0).min(MAX_MX_SECONDS)) * 1000;
    mx_ms.min(cap_ms)
}

/// Draw the actual delay: uniform over `0..=bound`.
pub fn response_delay_ms(mx: Option<u32>, cap_ms: u64) -> u64 {
    use rand::Rng as _;
    let bound = response_delay_bound_ms(mx, cap_ms);
    if bound == 0 {
        return 0;
    }
    rand::thread_rng().gen_range(0..=bound)
}
