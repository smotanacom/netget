//! The HTTP round trip the HTTP-family clients share, on both targets.
//!
//! Natively a [`FetchClient`] wraps the `reqwest::Client` each protocol already builds (its own
//! timeout, the literal-IP resolver bypass, its connection pool), and every call is reqwest's
//! own: what goes on the wire is exactly what reqwest sends. In the browser build reqwest cannot
//! serve (its wasm backend is the browser's `fetch`, which is not `Send` and cannot reach the
//! page's virtual loopback), so the same calls go through [`transport`] — hyper's HTTP/1.1
//! client over `tokio::net::TcpStream`, which on wasm32 is `crates/netget-tokio-wasm`'s virtual
//! network. The client logic above the round trip is one copy.
//!
//! The API is the subset of reqwest's the clients use — `get`/`post`/…, `header`, `query`,
//! `json`, `form`, `body`, `basic_auth`, `timeout`, `send`, and on the response `status`, `headers`,
//! `text`, `json`, `bytes`, `chunk` — so converting a client is a type change, not a rewrite.
//!
//! The transport backend is also constructible natively ([`FetchClient::transport`]), which is
//! how `tests/client/http/fetch_client_test.rs` pins it down without a browser.
//!
//! Differences on the transport, stated: no TLS (`https://` is refused with
//! [`transport::HTTPS_UNSUPPORTED`] — call [`check_url`] where a client takes its target so the
//! refusal comes at connect rather than on the first request), no redirects, one connection per
//! request, and the whole response body is read up front against [`FetchClient`]'s body bound
//! (8 MiB unless [`FetchClient::with_max_body`] says otherwise), so `chunk()` yields it once.

pub mod transport;

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use hyper::header::{HeaderMap, CONTENT_TYPE};
use hyper::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// An HTTP client: reqwest natively, [`transport`] in the browser (or wherever
/// [`FetchClient::transport`] is asked for).
#[derive(Clone)]
pub struct FetchClient {
    backend: Backend,
}

#[derive(Clone)]
enum Backend {
    #[cfg(not(target_arch = "wasm32"))]
    Reqwest(reqwest::Client, usize),
    Transport {
        timeout: Duration,
        max_body: usize,
        user_agent: Option<String>,
        wire: transport::Wire,
    },
}

impl FetchClient {
    /// Wrap a reqwest client with the shared buffered-response bound.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_reqwest(client: reqwest::Client) -> Self {
        Self {
            backend: Backend::Reqwest(client, transport::MAX_RESPONSE_BODY_BYTES),
        }
    }

    /// The hyper transport: one connection per request, each exchange bounded by `timeout`
    /// and the response body by [`transport::MAX_RESPONSE_BODY_BYTES`].
    pub fn transport(timeout: Duration) -> Self {
        Self {
            backend: Backend::Transport {
                timeout,
                max_body: transport::MAX_RESPONSE_BODY_BYTES,
                user_agent: None,
                wire: transport::Wire::Http1,
            },
        }
    }

    /// Bound buffered `bytes`, `text` and `json` responses on both backends.
    /// Native `chunk()` consumers still stream without an aggregate download cap.
    pub fn with_max_body(mut self, max_body: usize) -> Self {
        match &mut self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Reqwest(_, bound) => *bound = max_body,
            Backend::Transport {
                max_body: bound, ..
            } => *bound = max_body,
        }
        self
    }

    /// Send `User-Agent: user_agent` on every transport request that sets none of its own —
    /// what reqwest's `ClientBuilder::user_agent` does for a reqwest-backed client, whose
    /// builder already carries it.
    #[allow(irrefutable_let_patterns)]
    pub fn with_user_agent(mut self, user_agent: &str) -> Self {
        if let Backend::Transport { user_agent: ua, .. } = &mut self.backend {
            *ua = Some(user_agent.to_string());
        }
        self
    }

    /// Speak HTTP/2 with prior knowledge on the transport (cleartext h2c, no upgrade), as
    /// reqwest's `ClientBuilder::http2_prior_knowledge` does for a reqwest-backed client,
    /// whose builder already carries it.
    #[allow(irrefutable_let_patterns)]
    pub fn http2_prior_knowledge(mut self) -> Self {
        if let Backend::Transport { wire, .. } = &mut self.backend {
            *wire = transport::Wire::Http2PriorKnowledge;
        }
        self
    }

    pub fn request(&self, method: Method, url: &str) -> FetchRequest {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Reqwest(client, max_body) => FetchRequest {
                inner: RequestInner::Reqwest(client.request(method, url), *max_body),
            },
            Backend::Transport {
                timeout,
                max_body,
                user_agent,
                wire,
            } => FetchRequest {
                inner: RequestInner::Transport(TransportRequest {
                    method,
                    url: url.to_string(),
                    headers: Vec::new(),
                    user_agent: user_agent.clone(),
                    wire: *wire,
                    body: None,
                    timeout: *timeout,
                    max_body: *max_body,
                    error: None,
                }),
            },
        }
    }

    pub fn get(&self, url: &str) -> FetchRequest {
        self.request(Method::GET, url)
    }

    pub fn post(&self, url: &str) -> FetchRequest {
        self.request(Method::POST, url)
    }

    pub fn put(&self, url: &str) -> FetchRequest {
        self.request(Method::PUT, url)
    }

    pub fn delete(&self, url: &str) -> FetchRequest {
        self.request(Method::DELETE, url)
    }

    pub fn head(&self, url: &str) -> FetchRequest {
        self.request(Method::HEAD, url)
    }

    pub fn patch(&self, url: &str) -> FetchRequest {
        self.request(Method::PATCH, url)
    }
}

/// Refuse a target the browser transport cannot reach, with the reason; natively every URL
/// reqwest accepts is accepted. Call it where a client takes its base URL, so an `https://`
/// target in the browser fails at connect, naming why, rather than on its first request.
pub fn check_url(url: &str) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    transport::parse_http_url(url)?;
    #[cfg(not(target_arch = "wasm32"))]
    let _ = url;
    Ok(())
}

/// `scheme://host[:port]` of `url`, host lowercased and a default port dropped, so two
/// spellings of one origin compare equal and two origins never do.
pub fn origin_of(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).with_context(|| format!("{url:?} is not a URL"))?;
    anyhow::ensure!(
        matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
        "{url:?} is not an http(s) URL with a host"
    );
    Ok(parsed.origin().ascii_serialization())
}

/// Resolve an action's `path` against the client's base URL, refusing any other origin.
///
/// The HTTP-family clients accept an absolute `path`, and until September 2026 they sent
/// it wherever it pointed — together with the startup `default_headers` (API keys,
/// cookies) and, for WebDAV, the `auth` credential as `Authorization: Basic`. The model
/// reads the peer's responses, so a prompt-injected "fetch http://attacker/" exfiltrated
/// the operator's credential to any host in one request, and `http://169.254.169.254/`
/// reached the metadata service from inside the network the client ran on. A client is
/// bound to one origin: its `remote_addr`. An absolute `path` on that origin is accepted
/// (it is what a model copying a `Location` header produces); any other origin is refused
/// by name, so the repair loop can see it and the operator can open a client for that
/// host on purpose. A `base_url` without a scheme is `http://`.
pub fn resolve_same_origin(base_url: &str, path: &str) -> Result<String> {
    let base = if base_url.contains("://") {
        base_url.to_string()
    } else {
        format!("http://{base_url}")
    };
    let is_absolute = path.starts_with("http://") || path.starts_with("https://");
    if !is_absolute {
        return Ok(format!("{base}{path}"));
    }
    let bound = origin_of(&base)?;
    let asked = origin_of(path)?;
    anyhow::ensure!(
        bound == asked,
        "path {path:?} is on {asked}, but this client is bound to {bound}; use a path \
         relative to that origin, or open a client for {asked}"
    );
    Ok(path.to_string())
}

/// A redirect policy that follows up to five hops, none of them to another origin.
///
/// reqwest's default follows ten and strips `Authorization` and `Cookie` on a host change,
/// but not a startup `X-Api-Key`, and a redirect to an internal address is the second half
/// of the SSRF that `resolve_same_origin` closes on the first request. A 3xx to another
/// origin is returned to the model as the response it is.
#[cfg(not(target_arch = "wasm32"))]
pub fn same_origin_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let first = attempt
            .previous()
            .first()
            .map(|u| u.origin().ascii_serialization());
        let next = attempt.url().origin().ascii_serialization();
        if attempt.previous().len() >= 5 || first.as_deref() != Some(next.as_str()) {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

/// One request being built. Errors in a header or a body are held until [`Self::send`], as
/// reqwest's builder does.
pub struct FetchRequest {
    inner: RequestInner,
}

enum RequestInner {
    #[cfg(not(target_arch = "wasm32"))]
    Reqwest(reqwest::RequestBuilder, usize),
    Transport(TransportRequest),
}

struct TransportRequest {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
    user_agent: Option<String>,
    wire: transport::Wire,
    timeout: Duration,
    max_body: usize,
    error: Option<anyhow::Error>,
}

impl TransportRequest {
    fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name))
    }

    fn fail(&mut self, error: anyhow::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

impl FetchRequest {
    /// Add a header. Adding the same name twice sends both, as reqwest does.
    pub fn header(self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(
                    builder.header(name.as_ref(), value.as_ref()),
                    max_body,
                ),
            },
            RequestInner::Transport(mut req) => {
                req.headers
                    .push((name.as_ref().to_string(), value.as_ref().to_string()));
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// Serialize `value` as the JSON body, with `Content-Type: application/json` unless one
    /// is already set.
    pub fn json<T: Serialize + ?Sized>(self, value: &T) -> Self {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(builder.json(value), max_body),
            },
            RequestInner::Transport(mut req) => {
                match serde_json::to_vec(value) {
                    Ok(body) => {
                        if !req.has_header(CONTENT_TYPE.as_str()) {
                            req.headers.push((
                                CONTENT_TYPE.as_str().to_string(),
                                "application/json".to_string(),
                            ));
                        }
                        req.body = Some(Bytes::from(body));
                    }
                    Err(e) => req.fail(anyhow!("serialize the JSON request body: {e}")),
                }
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// Append `query`, serialized as `application/x-www-form-urlencoded` pairs, to the URL's
    /// query string.
    pub fn query<T: Serialize + ?Sized>(self, query: &T) -> Self {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(builder.query(query), max_body),
            },
            RequestInner::Transport(mut req) => {
                match serde_urlencoded::to_string(query) {
                    Ok(encoded) if encoded.is_empty() => {}
                    Ok(encoded) => {
                        let separator = if req.url.contains('?') { '&' } else { '?' };
                        req.url = format!("{}{separator}{encoded}", req.url);
                    }
                    Err(e) => req.fail(anyhow!("serialize the query string: {e}")),
                }
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// Serialize `value` as an `application/x-www-form-urlencoded` body.
    pub fn form<T: Serialize + ?Sized>(self, value: &T) -> Self {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(builder.form(value), max_body),
            },
            RequestInner::Transport(mut req) => {
                match serde_urlencoded::to_string(value) {
                    Ok(body) => {
                        if !req.has_header(CONTENT_TYPE.as_str()) {
                            req.headers.push((
                                CONTENT_TYPE.as_str().to_string(),
                                "application/x-www-form-urlencoded".to_string(),
                            ));
                        }
                        req.body = Some(Bytes::from(body));
                    }
                    Err(e) => req.fail(anyhow!("serialize the form request body: {e}")),
                }
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// The raw request body.
    pub fn body(self, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(builder.body(body), max_body),
            },
            RequestInner::Transport(mut req) => {
                req.body = Some(body);
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// HTTP Basic authentication, as reqwest writes it: `user:password` (the colon kept when
    /// there is no password), base64, in `Authorization`.
    pub fn basic_auth(
        self,
        user: impl std::fmt::Display,
        password: Option<impl std::fmt::Display>,
    ) -> Self {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(builder.basic_auth(user, password), max_body),
            },
            RequestInner::Transport(mut req) => {
                use base64::Engine;
                let credentials = match password {
                    Some(password) => format!("{user}:{password}"),
                    None => format!("{user}:"),
                };
                let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
                req.headers.push((
                    hyper::header::AUTHORIZATION.as_str().to_string(),
                    format!("Basic {encoded}"),
                ));
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// Bound this request at `timeout` instead of the client's.
    pub fn timeout(self, timeout: Duration) -> Self {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Self {
                inner: RequestInner::Reqwest(builder.timeout(timeout), max_body),
            },
            RequestInner::Transport(mut req) => {
                req.timeout = timeout;
                Self {
                    inner: RequestInner::Transport(req),
                }
            }
        }
    }

    /// Send the request and read the response head (natively) or the whole response (on the
    /// transport). A status that is not 2xx is a response, not an error.
    pub async fn send(self) -> Result<FetchResponse> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            RequestInner::Reqwest(builder, max_body) => Ok(FetchResponse {
                inner: ResponseInner::Reqwest(builder.send().await?, max_body),
            }),
            RequestInner::Transport(mut req) => {
                if let Some(error) = req.error.take() {
                    return Err(error);
                }
                if let Some(user_agent) = req.user_agent.take() {
                    if !req.has_header(hyper::header::USER_AGENT.as_str()) {
                        req.headers
                            .push((hyper::header::USER_AGENT.as_str().to_string(), user_agent));
                    }
                }
                let response = transport::fetch_response_on(
                    req.wire,
                    req.method.as_str(),
                    &req.url,
                    &req.headers,
                    req.body,
                    req.timeout,
                    req.max_body,
                )
                .await?;
                let (parts, body) = response.into_parts();
                Ok(FetchResponse {
                    inner: ResponseInner::Transport {
                        status: parts.status,
                        version: parts.version,
                        headers: parts.headers,
                        body: Some(body),
                    },
                })
            }
        }
    }
}

/// A response. Natively reqwest's, streamed as reqwest streams it; on the transport already
/// read in full (bounded), so `chunk` yields the whole body once.
pub struct FetchResponse {
    inner: ResponseInner,
}

enum ResponseInner {
    #[cfg(not(target_arch = "wasm32"))]
    Reqwest(reqwest::Response, usize),
    Transport {
        status: StatusCode,
        version: hyper::Version,
        headers: HeaderMap,
        body: Option<Bytes>,
    },
}

impl FetchResponse {
    pub fn status(&self) -> StatusCode {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, _) => response.status(),
            ResponseInner::Transport { status, .. } => *status,
        }
    }

    /// The HTTP version the response came in.
    pub fn version(&self) -> hyper::Version {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, _) => response.version(),
            ResponseInner::Transport { version, .. } => *version,
        }
    }

    pub fn headers(&self) -> &HeaderMap {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, _) => response.headers(),
            ResponseInner::Transport { headers, .. } => headers,
        }
    }

    /// The body's length as the response declared it, where it did.
    pub fn content_length(&self) -> Option<u64> {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, _) => response.content_length(),
            ResponseInner::Transport { body, headers, .. } => {
                body.as_ref().map(|b| b.len() as u64).or_else(|| {
                    headers
                        .get(hyper::header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                })
            }
        }
    }

    /// The next piece of the body, `None` at its end.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        match &mut self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, _) => Ok(response.chunk().await?),
            ResponseInner::Transport { body, .. } => Ok(body.take().filter(|b| !b.is_empty())),
        }
    }

    /// The whole body.
    pub async fn bytes(self) -> Result<Bytes> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, limit) => read_response_bytes(response, limit).await,
            ResponseInner::Transport { body, .. } => Ok(body.unwrap_or_default()),
        }
    }

    /// The whole body as text. reqwest decodes by the declared charset; the transport reads
    /// UTF-8, lossily.
    pub async fn text(self) -> Result<String> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, limit) => read_response_text(response, limit).await,
            ResponseInner::Transport { body, .. } => {
                Ok(String::from_utf8_lossy(&body.unwrap_or_default()).into_owned())
            }
        }
    }

    /// The whole body parsed as JSON.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response, limit) => read_response_json(response, limit).await,
            ResponseInner::Transport { body, .. } => {
                serde_json::from_slice(&body.unwrap_or_default())
                    .context("decode the response body as JSON")
            }
        }
    }
}

/// A [`FetchClient`] failure as a `std::error::Error`, for crates whose HTTP hook requires one
/// (oauth2's and openidconnect's `request_async` / `discover_async`).
#[derive(Debug)]
pub struct FetchError(pub anyhow::Error);

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for FetchError {}

/// One request given as raw parts, for a crate's own HTTP hook to call: the method, the URL,
/// the headers as (name, value bytes) and the body, back as the status, the headers and the
/// body. Header values that are not visible ASCII are sent lossily; the hooks that call this
/// (OAuth2 and OpenID Connect token, discovery and key-set requests) send none.
pub async fn round_trip_parts(
    client: &FetchClient,
    method: &str,
    url: &str,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
) -> std::result::Result<(u16, Vec<(String, Vec<u8>)>, Bytes), FetchError> {
    let method: Method = method
        .parse()
        .map_err(|e| FetchError(anyhow!("invalid HTTP method {method:?}: {e}")))?;
    let mut request = client.request(method, url);
    for (name, value) in &headers {
        request = request.header(name, String::from_utf8_lossy(value));
    }
    if !body.is_empty() {
        request = request.body(body);
    }
    let response = request.send().await.map_err(FetchError)?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec()))
        .collect();
    let body = response.bytes().await.map_err(FetchError)?;
    Ok((status, headers, body))
}

/// Read a native buffered response without trusting Content-Length. Streaming callers
/// use `chunk()` directly; buffered model events have a byte cap and whole-read deadline.
#[cfg(not(target_arch = "wasm32"))]
pub async fn read_response_bytes(mut response: reqwest::Response, limit: usize) -> Result<Bytes> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        anyhow::bail!("HTTP response body exceeds the {limit}-byte cap");
    }
    tokio::time::timeout(transport::REQUEST_TIMEOUT, async move {
        let mut body = bytes::BytesMut::new();
        while let Some(chunk) = response.chunk().await? {
            if chunk.len() > limit.saturating_sub(body.len()) {
                anyhow::bail!("HTTP response body exceeds the {limit}-byte cap");
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    })
    .await
    .context("HTTP response body deadline exceeded")?
}

/// Read bounded text while preserving reqwest's Content-Type charset/BOM decoding.
#[cfg(not(target_arch = "wasm32"))]
pub async fn read_response_text(response: reqwest::Response, limit: usize) -> Result<String> {
    let headers = response.headers().clone();
    let status = response.status();
    let body = read_response_bytes(response, limit).await?;
    let mut buffered = hyper::Response::new(body);
    *buffered.headers_mut() = headers;
    *buffered.status_mut() = status;
    Ok(reqwest::Response::from(buffered).text().await?)
}

/// Parse JSON only after the complete response meets the byte and time bounds.
#[cfg(not(target_arch = "wasm32"))]
pub async fn read_response_json<T: DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> Result<T> {
    serde_json::from_slice(&read_response_bytes(response, limit).await?)
        .context("decode the response body as JSON")
}
