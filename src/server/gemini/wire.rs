//! Gemini wire format: request validation, the response header, and gemtext rendering.
//!
//! Pure functions, shared by the session loop, the action executor and the tests. The model
//! supplies a status, a line of meta and structured gemtext lines; everything that makes a
//! Gemini response well-formed — the two-digit code, the single space, the CRLF, the 1024-byte
//! meta bound, the body appearing only after a 2x, and gemtext line prefixes that cannot be
//! confused with one another — is decided here.

use crate::utils::sanitize;

/// The longest URL a request may carry, in bytes, excluding its CRLF.
///
/// Gemini specification §"Requests": "The URI MUST NOT exceed 1024 bytes". A longer one is
/// refused with `59` before anything else happens.
pub const MAX_URL_BYTES: usize = 1024;

/// The longest request line this server buffers: [`MAX_URL_BYTES`] plus CRLF. This is the
/// declared `max_inbound_bytes` — a client sends nothing else on a connection.
pub const MAX_REQUEST_BYTES: usize = MAX_URL_BYTES + 2;

/// The longest meta field a response may carry (§"Responses": META MUST NOT exceed 1024 bytes).
pub const MAX_META_BYTES: usize = 1024;

/// The MIME type NetGet's rendered gemtext declares.
pub const GEMTEXT_MIME: &str = "text/gemini; charset=utf-8";

/// Every status code the specification defines, with the meta a response gets when the model
/// leaves it empty. A response header is always `<code> <meta>` with a non-empty meta: some
/// clients split the header on whitespace and fail outright on a bare `51\r\n`.
pub const STATUS_CODES: &[(u16, &str)] = &[
    (10, "Input"),
    (11, "Sensitive input"),
    (20, GEMTEXT_MIME),
    (30, ""),
    (31, ""),
    (40, "Temporary failure"),
    (41, "Server unavailable"),
    (42, "CGI error"),
    (43, "Proxy error"),
    (44, "60"),
    (50, "Permanent failure"),
    (51, "Not found"),
    (52, "Gone"),
    (53, "Proxy request refused"),
    (59, "Bad request"),
    (60, "Client certificate required"),
    (61, "Certificate not authorised"),
    (62, "Certificate not valid"),
];

/// A request that passed validation, as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiRequest {
    /// The URL exactly as the client sent it.
    pub url: String,
    pub host: String,
    /// The path as it appears in the URL (not decoded); `/` when empty.
    pub path: String,
    /// The query, percent-decoded; `None` when the URL has no `?`.
    pub query: Option<String>,
}

/// Why a request line was refused before reaching the model, with the response it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestRefusal {
    /// `59`: not an absolute URL, a BOM, userinfo, a fragment, no host, or too long.
    BadRequest(&'static str),
    /// `53`: an absolute URL for a scheme other than `gemini` — a proxy request, which this
    /// server does not serve.
    ProxyRefused,
}

impl RequestRefusal {
    /// The complete response for this refusal. Fixed text: nothing the client sent is echoed.
    pub fn response(&self) -> String {
        match self {
            RequestRefusal::BadRequest(why) => format!("59 {why}\r\n"),
            RequestRefusal::ProxyRefused => "53 Proxy request refused\r\n".to_string(),
        }
    }

    /// The `decision=` token the session logs for this refusal.
    pub fn decision(&self) -> &'static str {
        match self {
            RequestRefusal::BadRequest(_) => "refused_bad_request",
            RequestRefusal::ProxyRefused => "refused_proxy_request",
        }
    }
}

/// Validate a request line (without its CRLF).
pub fn parse_request(line: &str) -> Result<GeminiRequest, RequestRefusal> {
    if line.len() > MAX_URL_BYTES {
        return Err(RequestRefusal::BadRequest("Request too long"));
    }
    if line.starts_with('\u{feff}') {
        return Err(RequestRefusal::BadRequest(
            "Request must not begin with a BOM",
        ));
    }
    let url = url::Url::parse(line)
        .map_err(|_| RequestRefusal::BadRequest("Request is not an absolute URL"))?;
    if url.scheme() != "gemini" {
        return Err(RequestRefusal::ProxyRefused);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RequestRefusal::BadRequest("URL must not contain userinfo"));
    }
    if url.fragment().is_some() {
        return Err(RequestRefusal::BadRequest(
            "URL must not contain a fragment",
        ));
    }
    let host = match url.host_str() {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => return Err(RequestRefusal::BadRequest("URL has no host")),
    };
    let path = if url.path().is_empty() {
        "/".to_string()
    } else {
        url.path().to_string()
    };
    Ok(GeminiRequest {
        url: line.to_string(),
        host,
        path,
        // Decoded for the model, which should see `hello world` rather than `hello%20world`,
        // and then cleaned: `%1B` would otherwise put an escape sequence on the operator's
        // screen through the event log. Newline and tab survive — a query can be multi-line
        // input the model is meant to read.
        query: url.query().map(|q| sanitize::multiline(&percent_decode(q))),
    })
}

/// Decode `%XX` escapes (and nothing else — `+` is a plus sign in a Gemini query, which is
/// not form encoding). Invalid escapes are kept literally; invalid UTF-8 is replaced.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Bytes, not `&s[..]`: an index into a multi-byte character would panic.
        // Both digits checked explicitly: `from_str_radix` would accept "+1".
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let decoded = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(b) = decoded {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why a response could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseError {
    UnknownStatus(u16),
    MetaTooLong(usize),
    MissingMeta(u16),
    BadMeta(u16, &'static str),
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseError::UnknownStatus(c) => {
                let codes: Vec<String> = STATUS_CODES.iter().map(|(c, _)| c.to_string()).collect();
                write!(
                    f,
                    "status {c} is not a Gemini status; use one of {}",
                    codes.join(", ")
                )
            }
            ResponseError::MetaTooLong(n) => {
                write!(
                    f,
                    "meta is {n} bytes; Gemini allows at most {MAX_META_BYTES}"
                )
            }
            ResponseError::MissingMeta(c) => write!(f, "status {c} requires a meta"),
            ResponseError::BadMeta(c, why) => write!(f, "status {c}: {why}"),
        }
    }
}

impl std::error::Error for ResponseError {}

/// Render a complete response: header, and the body only for a 2x.
///
/// The meta becomes one line (control characters → spaces, trimmed), an empty meta takes the
/// status's default, and a body given with any status other than 2x is dropped — Gemini
/// defines no body for them, and a client reading one would take it for the next response's
/// garbage. The caller learns whether that happened from the second return value.
pub fn render_response(
    status: u16,
    meta: &str,
    body: Option<&str>,
) -> Result<(String, bool), ResponseError> {
    let (_, default_meta) = STATUS_CODES
        .iter()
        .find(|(c, _)| *c == status)
        .ok_or(ResponseError::UnknownStatus(status))?;
    let meta = sanitize::line_field(meta).trim().to_string();
    let meta = if meta.is_empty() {
        if default_meta.is_empty() {
            return Err(ResponseError::MissingMeta(status));
        }
        default_meta.to_string()
    } else {
        meta
    };
    if meta.len() > MAX_META_BYTES {
        return Err(ResponseError::MetaTooLong(meta.len()));
    }
    if status == 44 && !meta.chars().all(|c| c.is_ascii_digit()) {
        return Err(ResponseError::BadMeta(
            44,
            "meta must be the number of seconds to wait",
        ));
    }
    if (status == 30 || status == 31) && meta.contains(char::is_whitespace) {
        return Err(ResponseError::BadMeta(status, "meta must be a URL"));
    }
    let mut out = format!("{status} {meta}\r\n");
    let is_success = (20..30).contains(&status);
    let dropped_body = !is_success && body.is_some_and(|b| !b.is_empty());
    if is_success {
        if let Some(body) = body {
            out.push_str(body);
        }
    }
    Ok((out, dropped_body))
}

/// One gemtext line as the model supplies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GemtextLine {
    Text(String),
    Link { url: String, text: String },
    Heading(u8, String),
    ListItem(String),
    Quote(String),
    Preformatted { alt: String, text: String },
}

/// Why a gemtext line could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GemtextError {
    UnknownType(String),
    LinkWithoutUrl,
    LinkUrlHasWhitespace,
}

impl std::fmt::Display for GemtextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GemtextError::UnknownType(t) => write!(
                f,
                "unknown gemtext line type {t:?}; use text, link, heading1, heading2, heading3, \
                 list, quote or preformatted"
            ),
            GemtextError::LinkWithoutUrl => write!(f, "a link line needs a 'url'"),
            GemtextError::LinkUrlHasWhitespace => write!(
                f,
                "a link 'url' cannot contain whitespace (percent-encode it: %20)"
            ),
        }
    }
}

impl std::error::Error for GemtextError {}

impl GemtextLine {
    pub fn from_parts(
        kind: &str,
        text: &str,
        url: Option<&str>,
        alt: Option<&str>,
    ) -> Result<Self, GemtextError> {
        Ok(match kind {
            "text" => GemtextLine::Text(text.to_string()),
            "link" => {
                let url = url.map(str::trim).filter(|u| !u.is_empty());
                let url = url.ok_or(GemtextError::LinkWithoutUrl)?;
                if url.contains(char::is_whitespace) || sanitize::strip_controls(url) != url {
                    return Err(GemtextError::LinkUrlHasWhitespace);
                }
                GemtextLine::Link {
                    url: url.to_string(),
                    text: text.to_string(),
                }
            }
            "heading1" => GemtextLine::Heading(1, text.to_string()),
            "heading2" => GemtextLine::Heading(2, text.to_string()),
            "heading3" => GemtextLine::Heading(3, text.to_string()),
            "list" => GemtextLine::ListItem(text.to_string()),
            "quote" => GemtextLine::Quote(text.to_string()),
            "preformatted" => GemtextLine::Preformatted {
                alt: alt.unwrap_or("").to_string(),
                text: text.to_string(),
            },
            other => return Err(GemtextError::UnknownType(other.to_string())),
        })
    }
}

/// The prefixes that give a gemtext line a type. A text line beginning with one would be read
/// as that type.
const LINE_TYPE_PREFIXES: &[&str] = &["=>", "#", "*", ">", "```"];

/// Make a line safe as a *text* line: control characters out, and a leading space in front of
/// anything that would otherwise read as a link, heading, list item, quote or toggle. Gemtext
/// has no escape character; a text line is only a link when `=>` is at column 0, so one space
/// is the least change that keeps the words and loses the misreading.
fn plain(line: &str) -> String {
    let clean = sanitize::strip_controls(line);
    if LINE_TYPE_PREFIXES.iter().any(|p| clean.starts_with(p)) {
        format!(" {clean}")
    } else {
        clean
    }
}

/// One-line content for a typed line: control characters (including newlines) become spaces.
fn one_line(s: &str) -> String {
    sanitize::line_field(s).trim().to_string()
}

fn split_lines(text: &str) -> Vec<String> {
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    sanitize::multiline(&normalised)
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Render gemtext. Every line ends in LF, which the specification allows and every client reads.
pub fn render_gemtext(lines: &[GemtextLine]) -> String {
    let mut out = String::new();
    for line in lines {
        match line {
            GemtextLine::Text(text) => {
                for l in split_lines(text) {
                    out.push_str(&plain(&l));
                    out.push('\n');
                }
            }
            GemtextLine::Link { url, text } => {
                let label = one_line(text);
                if label.is_empty() {
                    out.push_str(&format!("=> {url}\n"));
                } else {
                    out.push_str(&format!("=> {url} {label}\n"));
                }
            }
            GemtextLine::Heading(level, text) => {
                out.push_str(&"#".repeat(*level as usize));
                out.push(' ');
                out.push_str(&one_line(text));
                out.push('\n');
            }
            GemtextLine::ListItem(text) => {
                out.push_str("* ");
                out.push_str(&one_line(text));
                out.push('\n');
            }
            GemtextLine::Quote(text) => {
                for l in split_lines(text) {
                    out.push_str("> ");
                    out.push_str(&sanitize::strip_controls(&l));
                    out.push('\n');
                }
            }
            GemtextLine::Preformatted { alt, text } => {
                out.push_str("```");
                out.push_str(&one_line(alt));
                out.push('\n');
                for l in split_lines(text) {
                    // Inside a block only a line starting with ``` means anything (it closes
                    // the block), so that is the one prefix to defuse.
                    let l = sanitize::strip_controls(&l);
                    if l.starts_with("```") {
                        out.push(' ');
                    }
                    out.push_str(&l);
                    out.push('\n');
                }
                out.push_str("```\n");
            }
        }
    }
    out
}

/// A `lang` parameter for the gemtext MIME type: BCP 47 tags, comma-separated. Anything else is
/// dropped rather than written into the header.
pub fn gemtext_mime(lang: Option<&str>) -> String {
    match lang.map(str::trim).filter(|l| {
        !l.is_empty()
            && l.len() <= 64
            && l.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == ',')
    }) {
        Some(lang) => format!("{GEMTEXT_MIME}; lang={lang}"),
        None => GEMTEXT_MIME.to_string(),
    }
}

/// The two-digit status a rendered response begins with.
pub fn leading_status(response: &[u8]) -> Option<u16> {
    let head = response.get(..2)?;
    if !head.iter().all(u8::is_ascii_digit) || response.get(2) != Some(&b' ') {
        return None;
    }
    std::str::from_utf8(head).ok()?.parse().ok()
}
