//! One HTTP/1.1 request and its response, over a stream NetGet opens itself.
//!
//! This is the transport every HTTP-family client uses in the browser build, through
//! [`super::FetchClient`] (the `http` client calls [`fetch`] directly). `reqwest` cannot serve
//! there: its wasm32 backend is the browser's `fetch`, whose futures are not `Send` (the `Client`
//! trait requires `Send`) and which cannot reach the page's virtual loopback anyway — NetGet's
//! servers in the page listen on `crates/netget-tokio-wasm`'s in-memory network, not on anything
//! `fetch` can dial. So the request is written with hyper 1's `client::conn::http1` over whatever
//! [`tokio::net::TcpStream`] is on this target: the kernel's natively, the virtual loopback's in
//! the browser. The client role of hyper's HTTP/1 dispatcher never touches the clock (only the
//! server role maintains a `Date` header cache, which `vendor/hyper` patches for the browser), so
//! this path runs there unchanged.
//!
//! It compiles on both targets so it can be tested natively (`tests/client/http/transport_test.rs`
//! drives NetGet's own HTTP and TCP servers through it). The native clients still use reqwest,
//! which brings TLS, HTTP/2 and connection pooling this module deliberately does not have.
//!
//! HTTP/2 is spoken with prior knowledge only ([`Wire::Http2PriorKnowledge`], cleartext h2c
//! with no upgrade and no ALPN — what `reqwest`'s `http2_prior_knowledge()` does), through
//! hyper's `client::conn::http2`. Its connection spawns its own tasks through
//! [`SpawnExecutor`], which is `tokio::spawn` as this crate names it: tokio's natively, the
//! browser shim's (the JS event loop) on wasm32.
//!
//! What it does not do, stated rather than implied: no TLS — an `https://` URL is refused with
//! the reason (see [`HTTPS_UNSUPPORTED`]); no redirects (a 3xx is reported to the model like any
//! other status, which is what the model is for); no connection reuse (one connection per
//! request, closed after the response); no compression negotiation.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};

/// One completed HTTP exchange, the body read as text (lossily, where it is not UTF-8).
///
/// What the `http` client reports to the model; [`fetch_response`] keeps the body as bytes for
/// the clients whose bodies are binary.
#[derive(Debug, Clone)]
pub struct HttpExchange {
    pub status_code: u16,
    pub status_text: String,
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: String,
}

impl HttpExchange {
    fn from_response(response: hyper::Response<Bytes>) -> Self {
        let status = response.status();
        let mut headers = serde_json::Map::new();
        for (name, value) in response.headers() {
            if let Ok(text) = value.to_str() {
                headers.insert(name.to_string(), serde_json::json!(text));
            }
        }
        HttpExchange {
            status_code: status.as_u16(),
            status_text: status.to_string(),
            headers,
            body: String::from_utf8_lossy(response.body()).into_owned(),
        }
    }
}

/// The largest response body this client reads before refusing the response. The HTTP/3 client
/// uses the same number; it is far above anything a model can usefully read in one event.
pub const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

/// How long one exchange — connect, request, response head and body — may take in total.
/// The same 30 seconds the native reqwest client is built with.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an `https://` URL is refused on this transport.
pub const HTTPS_UNSUPPORTED: &str =
    "https:// is not available on this transport: it has no TLS stack, and in the browser build \
     no server on the page's virtual network holds a certificate a client could verify. Use \
     http:// (NetGet's own servers in the page speak plain HTTP)";

/// The connection driver must end even when a caller drops or times out the exchange.
/// A plain JoinHandle detaches on drop, leaving an unanswered request's socket alive.
struct ConnectionDriver(tokio::task::JoinHandle<()>);

impl Drop for ConnectionDriver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn parse_method(method: &str) -> Result<hyper::Method> {
    // Retain case-insensitive convenience for the previously supported methods.
    // Extension methods are case-sensitive and must reach the peer unchanged.
    const STANDARD_METHODS: [&str; 7] =
        ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH", "OPTIONS"];
    let method = STANDARD_METHODS
        .into_iter()
        .find(|standard| method.eq_ignore_ascii_case(standard))
        .unwrap_or(method);
    hyper::Method::from_bytes(method.as_bytes()).context("invalid HTTP method")
}

/// Where an `http://` URL points: the host and port to dial, the `Host` header, and the
/// origin-form request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpTarget {
    pub host: String,
    pub port: u16,
    pub authority: String,
    pub path_and_query: String,
}

/// Parse an absolute `http://` URL. `https://` is refused with [`HTTPS_UNSUPPORTED`]; any other
/// scheme, or no host, is an error naming the URL.
pub fn parse_http_url(url: &str) -> Result<HttpTarget> {
    if url
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("HTTP URL contains unescaped whitespace or control characters");
    }
    let parsed = url::Url::parse(url).context("invalid HTTP URL")?;
    if parsed.host_str().is_none() {
        bail!("HTTP URL requires a host");
    }
    let uri: hyper::Uri = url.parse().with_context(|| format!("not a URL: {url}"))?;
    match uri.scheme_str() {
        Some("http") => {}
        Some("https") => bail!("{HTTPS_UNSUPPORTED} (asked for {url})"),
        Some(other) => bail!("unsupported URL scheme {other:?} in {url}; only http:// is"),
        None => bail!("no scheme in {url}; expected http://host:port/path"),
    }
    let authority = uri
        .authority()
        .with_context(|| format!("no host in {url}"))?;
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if authority.as_str().contains('@') {
        bail!("HTTP URL userinfo is unsupported; use an explicit Authorization header");
    }
    if host.is_empty() {
        bail!("HTTP URL requires a nonempty host");
    }
    let port = match authority.port() {
        Some(port) => port
            .as_str()
            .parse::<u16>()
            .context("invalid HTTP URL port")?,
        None if authority.as_str().ends_with(':') => bail!("HTTP URL port is empty"),
        None => 80,
    };
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/".to_string());
    Ok(HttpTarget {
        host,
        port,
        authority: authority.as_str().to_string(),
        path_and_query,
    })
}

/// Dial `url`'s host and exchange one request, bounded by `timeout` as a whole and the body by
/// `max_body` bytes. `headers` are applied in order after `Host`; a `Host` among them replaces
/// the one derived from the URL.
pub async fn fetch(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<String>,
    timeout: Duration,
    max_body: usize,
) -> Result<HttpExchange> {
    fetch_response(
        method,
        url,
        headers,
        body.map(Bytes::from),
        timeout,
        max_body,
    )
    .await
    .map(HttpExchange::from_response)
}

/// Which HTTP a request is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    /// HTTP/1.1.
    Http1,
    /// HTTP/2 over cleartext TCP with prior knowledge (h2c, no upgrade, no ALPN).
    Http2PriorKnowledge,
}

/// Runs the tasks hyper's HTTP/2 connection spawns (its connection and per-stream body
/// pipes) with `tokio::spawn` as this crate resolves it: tokio's natively, the browser shim's
/// on wasm32. hyper-util's `TokioExecutor` would call the real tokio's `spawn` in the browser,
/// where no tokio runtime exists.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpawnExecutor;

impl<F> hyper::rt::Executor<F> for SpawnExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        // Each task ends with the stream or the connection it serves; the exchange aborts the
        // connection driver on every exit, which ends the rest.
        drop(tokio::spawn(future));
    }
}

/// As [`fetch`], with the request and response bodies as bytes.
pub async fn fetch_response(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<Bytes>,
    timeout: Duration,
    max_body: usize,
) -> Result<hyper::Response<Bytes>> {
    fetch_response_on(Wire::Http1, method, url, headers, body, timeout, max_body).await
}

/// As [`fetch_response`], written in `wire`'s HTTP.
#[allow(clippy::too_many_arguments)]
pub async fn fetch_response_on(
    wire: Wire,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<Bytes>,
    timeout: Duration,
    max_body: usize,
) -> Result<hyper::Response<Bytes>> {
    let target = parse_http_url(url)?;
    let exchange = async {
        let stream = tokio::net::TcpStream::connect((target.host.clone(), target.port))
            .await
            .with_context(|| format!("connect to {}:{}", target.host, target.port))?;
        match wire {
            Wire::Http1 => {
                exchange_response(stream, method, &target, headers, body, max_body).await
            }
            Wire::Http2PriorKnowledge => {
                exchange_response_h2(stream, method, &target, headers, body, max_body).await
            }
        }
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| anyhow!("no complete response from {url} within {timeout:?}"))?
}

/// Write one request on `io` and read its response, the body bounded by `max_body` bytes.
pub async fn exchange<S>(
    io: S,
    method: &str,
    target: &HttpTarget,
    headers: &[(String, String)],
    body: Option<String>,
    max_body: usize,
) -> Result<HttpExchange>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    exchange_response(io, method, target, headers, body.map(Bytes::from), max_body)
        .await
        .map(HttpExchange::from_response)
}

/// As [`exchange`], with the request and response bodies as bytes.
pub async fn exchange_response<S>(
    io: S,
    method: &str,
    target: &HttpTarget,
    headers: &[(String, String)],
    body: Option<Bytes>,
    max_body: usize,
) -> Result<hyper::Response<Bytes>>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let method = parse_method(method)?;

    let mut builder = hyper::Request::builder()
        .method(method.as_str())
        .uri(target.path_and_query.as_str())
        .header(hyper::header::HOST, target.authority.as_str());
    for (name, value) in headers {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header name {name:?}"))?;
        let value = hyper::header::HeaderValue::from_str(value)
            .with_context(|| format!("invalid value for header {name}"))?;
        let map = builder
            .headers_mut()
            .context("request builder rejected an earlier part")?;
        if name == hyper::header::HOST {
            map.insert(name, value);
        } else {
            map.append(name, value);
        }
    }
    let request = builder
        .body(Full::new(body.unwrap_or_default()))
        .context("build HTTP request")?;

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .context("HTTP/1.1 handshake")?;
    // The connection future drives the socket; it ends when the response is read and the
    // sender dropped. Aborted on every exit so an abandoned exchange cannot keep the socket.
    let _driver = ConnectionDriver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let result = async {
        let response = sender
            .send_request(request)
            .await
            .context("send HTTP request")?;
        let (parts, body) = response.into_parts();
        let collected = Limited::new(body, max_body).collect().await.map_err(|e| {
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                anyhow!("response body exceeds the {max_body}-byte limit; refused")
            } else {
                anyhow!("read HTTP response body: {e}")
            }
        })?;
        Ok(hyper::Response::from_parts(parts, collected.to_bytes()))
    }
    .await;
    result
}

/// As [`exchange_response`], in HTTP/2 with prior knowledge: the connection preface goes out
/// first, with no HTTP/1.1 upgrade. The request carries its scheme and authority in the URI,
/// which hyper writes as the `:scheme` and `:authority` pseudo-headers; no `Host` header is
/// added (a `Host` among `headers` is sent as given).
pub async fn exchange_response_h2<S>(
    io: S,
    method: &str,
    target: &HttpTarget,
    headers: &[(String, String)],
    body: Option<Bytes>,
    max_body: usize,
) -> Result<hyper::Response<Bytes>>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let method = parse_method(method)?;

    let uri = format!("http://{}{}", target.authority, target.path_and_query);
    let mut builder = hyper::Request::builder()
        .method(method.as_str())
        .uri(uri.as_str())
        .version(hyper::Version::HTTP_2);
    for (name, value) in headers {
        let name = hyper::header::HeaderName::from_bytes(name.as_bytes())
            .with_context(|| format!("invalid header name {name:?}"))?;
        let value = hyper::header::HeaderValue::from_str(value)
            .with_context(|| format!("invalid value for header {name}"))?;
        builder
            .headers_mut()
            .context("request builder rejected an earlier part")?
            .append(name, value);
    }
    let request = builder
        .body(Full::new(body.unwrap_or_default()))
        .context("build HTTP/2 request")?;

    let (mut sender, connection) =
        hyper::client::conn::http2::handshake(SpawnExecutor, TokioIo::new(io))
            .await
            .context("HTTP/2 handshake (prior knowledge)")?;
    let _driver = ConnectionDriver(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let result = async {
        let response = sender
            .send_request(request)
            .await
            .context("send HTTP/2 request")?;
        let (parts, body) = response.into_parts();
        let collected = Limited::new(body, max_body).collect().await.map_err(|e| {
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                anyhow!("response body exceeds the {max_body}-byte limit; refused")
            } else {
                anyhow!("read HTTP/2 response body: {e}")
            }
        })?;
        Ok(hyper::Response::from_parts(parts, collected.to_bytes()))
    }
    .await;
    result
}
