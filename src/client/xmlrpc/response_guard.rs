//! Bounds on what an XML-RPC server is allowed to answer with, applied before the `xmlrpc`
//! crate's parser ever sees the bytes.
//!
//! # Why this exists
//!
//! `xmlrpc` 0.15's `Parser::parse_value` → `parse_value_inner` → `parse_value` recurses once
//! per `<value>` element with **no depth counter**, and the crate reads the whole response
//! body with no size limit. `<value><array><data>` is about twenty bytes per level, so a
//! hostile or compromised server can answer the very first call with a few megabytes that
//! drive the parser tens of thousands of frames deep. A Rust stack overflow is a `SIGSEGV`
//! against the guard page, not a panic: `spawn_blocking` cannot contain it, `catch_unwind`
//! cannot see it, and **the whole NetGet process dies**.
//!
//! # The seam
//!
//! `Request::call_url` owns both the fetch and the parse, so there is no way in through it —
//! but `Request::call` takes any [`xmlrpc::Transport`], and `Transport` is a public trait with
//! an associated `Stream: Read` that anything can implement. (The reqwest 0.11
//! `RequestBuilder` is one *provided* implementation of it, not the signature; this file's
//! own doc comment used to say otherwise and that reading is what left the bug open.)
//!
//! So NetGet fetches the response itself with its own reqwest 0.12 client, refuses it if it
//! is too large or too deeply nested, and hands the crate a [`PrefetchedTransport`] wrapping
//! bytes that are already known to be safe to parse. The crate still does the parsing, so
//! fault handling, type coverage and error shapes are unchanged.
//!
//! # What it refuses
//!
//! - A body larger than [`MAX_RESPONSE_BYTES`], measured as it streams in, so the cap is never
//!   exceeded even transiently.
//! - Element nesting deeper than [`MAX_ELEMENT_DEPTH`].
//! - A body `quick-xml` cannot scan at all. We cannot certify a depth we could not measure,
//!   so an unscannable body is refused rather than passed through hopefully. A response that
//!   is not well-formed XML would fail in the crate's parser anyway.
//!
//! Refuse, never truncate: a response cut off at the cap is indistinguishable from a complete
//! one once it has been parsed, and the model would be told the server said something it did
//! not say. Each refusal carries a stable `decision=fail_closed_*` tag, in the shape
//! `src/server/radius/` established.
//!
//! # What this does not cover
//!
//! [`super::MAX_VALUE_DEPTH`] bounds NetGet's own recursive walk of the parsed value. That is
//! a second, independent way for a deep reply to kill the process, and it only ever mattered
//! once the crate's parser had survived. Both bounds are needed; neither replaces the other.

use std::error::Error;
use std::io::Cursor;
use std::time::Duration;

/// Largest XML-RPC response body NetGet will read.
///
/// XML-RPC replies are small by nature — `system.listMethods` on a large server is tens of
/// kilobytes. 8 MiB is far past any real payload and still small enough that scanning it costs
/// milliseconds. It is a backstop, not the main guard: at ~20 bytes per nesting level even
/// 1 MiB would buy ~50 000 parser frames, which is why [`MAX_ELEMENT_DEPTH`] exists.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Deepest XML element nesting NetGet will hand to the crate's parser.
///
/// The parser recurses once per `<value>`, and one XML-RPC value level costs at least three
/// elements (`<value><array><data>`), so this admits roughly 64 nested values — the same
/// ceiling [`super::MAX_VALUE_DEPTH`] puts on NetGet's own walk of the result, so anything the
/// scan lets through is something the conversion can also handle. Real XML-RPC APIs are a
/// handful of levels deep. 256 parser frames is three orders of magnitude below the stack a
/// blocking-pool thread has.
pub const MAX_ELEMENT_DEPTH: usize = 256;

/// Why a server's answer was refused before it was parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponseRefusal {
    /// The body passed [`MAX_RESPONSE_BYTES`] while streaming in.
    TooLarge { seen: usize },
    /// Element nesting passed [`MAX_ELEMENT_DEPTH`].
    TooDeep { depth: usize },
    /// The body could not be scanned, so its depth is unknown.
    Unscannable { detail: String },
}

impl ResponseRefusal {
    /// A stable tag an operator can grep for.
    pub fn decision_tag(&self) -> &'static str {
        match self {
            Self::TooLarge { .. } => "fail_closed_response_too_large",
            Self::TooDeep { .. } => "fail_closed_response_too_deep",
            Self::Unscannable { .. } => "fail_closed_response_unscannable",
        }
    }
}

impl std::fmt::Display for ResponseRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { seen } => write!(
                f,
                "server response exceeded {} bytes (stopped at {})",
                MAX_RESPONSE_BYTES, seen
            ),
            Self::TooDeep { depth } => write!(
                f,
                "server response nested deeper than {} XML elements (reached {})",
                MAX_ELEMENT_DEPTH, depth
            ),
            Self::Unscannable { detail } => write!(
                f,
                "server response could not be scanned for nesting depth: {}",
                detail
            ),
        }
    }
}

impl std::error::Error for ResponseRefusal {}

/// Measure element nesting without building a document, and refuse anything too deep.
///
/// Returns the deepest nesting seen. `quick-xml` does not expand entities and treats a DTD
/// internal subset as opaque text, so this scan cannot itself be turned into an amplification
/// — and neither can the crate's own parser, whose `xml-rs` backend rejects every entity
/// outside the five predefined ones.
///
/// End-name checking is off on purpose: mismatched tags are the real parser's business to
/// reject, and turning them into a scan error here would refuse responses on a question this
/// function has no opinion about. A stray close tag still decrements the depth, which is the
/// conservative direction.
pub fn scan_depth(body: &[u8]) -> Result<usize, ResponseRefusal> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_reader(body);
    reader.config_mut().check_end_names = false;

    let mut buf = Vec::new();
    let mut depth: usize = 0;
    let mut deepest: usize = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => {
                depth += 1;
                deepest = deepest.max(depth);
                if depth > MAX_ELEMENT_DEPTH {
                    return Err(ResponseRefusal::TooDeep { depth });
                }
            }
            Ok(Event::Empty(_)) => {
                // A leaf. It occupies one level but closes immediately.
                deepest = deepest.max(depth + 1);
                if depth + 1 > MAX_ELEMENT_DEPTH {
                    return Err(ResponseRefusal::TooDeep { depth: depth + 1 });
                }
            }
            Ok(Event::End(_)) => {
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(ResponseRefusal::Unscannable {
                    detail: e.to_string(),
                })
            }
        }
        buf.clear();
    }

    Ok(deepest)
}

/// Read a response body, refusing at [`MAX_RESPONSE_BYTES`] rather than buffering past it.
///
/// The check is per chunk as it arrives, so a server advertising a small `Content-Length` and
/// then sending gigabytes gets no further than the cap.
pub async fn read_body_capped(response: reqwest::Response) -> Result<Vec<u8>, anyhow::Error> {
    let mut response = response;
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ResponseRefusal::TooLarge {
                seen: body.len() + chunk.len(),
            }
            .into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The header checks `xmlrpc::transport::http::check_response` performs, reproduced against
/// reqwest 0.12 so replacing the transport does not quietly relax them.
///
/// A 4xx/5xx is an error, and a `Content-Type` that is present and parseable must be
/// `text/xml`. A missing or unparseable header is ignored, exactly as the crate ignores it.
pub fn check_response(response: &reqwest::Response) -> Result<(), anyhow::Error> {
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        anyhow::bail!("server response indicates error: {}", status);
    }

    if let Some(value) = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let essence = value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if let Some((ty, sub)) = essence.split_once('/') {
            if !(ty == "text" && sub == "xml") {
                anyhow::bail!("expected Content-Type 'text/xml', got '{}/{}'", ty, sub);
            }
        }
    }

    Ok(())
}

/// An [`xmlrpc::Transport`] that answers with bytes NetGet has already fetched and screened.
///
/// The `Request` argument is ignored: it was serialised and sent before this point, which is
/// the whole reason the fetch could be bounded at all.
#[derive(Debug)]
pub struct PrefetchedTransport {
    body: Vec<u8>,
}

impl PrefetchedTransport {
    /// Wrap a body that [`scan_depth`] and [`read_body_capped`] have already accepted.
    pub fn new(body: Vec<u8>) -> Self {
        Self { body }
    }
}

impl xmlrpc::Transport for PrefetchedTransport {
    type Stream = Cursor<Vec<u8>>;

    fn transmit(
        self,
        _request: &xmlrpc::Request<'_>,
    ) -> Result<Self::Stream, Box<dyn Error + Send + Sync>> {
        Ok(Cursor::new(self.body))
    }
}

/// The HTTP client used for one XML-RPC endpoint.
///
/// Built once per (host, timeout) and cached. Building a `reqwest::Client` loads the platform
/// root store — on macOS that reads the keychain through Security.framework, synchronously and
/// serialised across processes — so it is built on the blocking pool and never on a request
/// path. `client_for_endpoint_with_timeout` additionally skips the system resolver when the
/// host is a literal IP, which `getaddrinfo("127.0.0.1")` makes worth doing.
pub async fn http_client_for(server_url: &str, timeout: Duration) -> reqwest::Client {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<(String, u64), reqwest::Client>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let key = (
        crate::llm::ollama_client::host_of(server_url).to_string(),
        timeout.as_secs(),
    );

    if let Ok(guard) = cache.lock() {
        if let Some(client) = guard.get(&key) {
            return client.clone();
        }
    }

    let url = server_url.to_string();
    let built = tokio::task::spawn_blocking(move || {
        crate::llm::ollama_client::client_for_endpoint_with_timeout(&url, timeout)
    })
    .await
    .unwrap_or_else(|_| reqwest::Client::new());

    if let Ok(mut guard) = cache.lock() {
        // A concurrent build is possible and harmless - both clients are equivalent.
        guard.insert(key, built.clone());
    }
    built
}
