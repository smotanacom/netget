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
    Reqwest(reqwest::Client),
    Transport {
        timeout: Duration,
        max_body: usize,
    },
}

impl FetchClient {
    /// Wrap a reqwest client. Every request is reqwest's, unchanged.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_reqwest(client: reqwest::Client) -> Self {
        Self {
            backend: Backend::Reqwest(client),
        }
    }

    /// The hyper transport: one connection per request, each exchange bounded by `timeout`
    /// and the response body by [`transport::MAX_RESPONSE_BODY_BYTES`].
    pub fn transport(timeout: Duration) -> Self {
        Self {
            backend: Backend::Transport {
                timeout,
                max_body: transport::MAX_RESPONSE_BODY_BYTES,
            },
        }
    }

    /// Bound the transport's response body at `max_body` bytes instead. A reqwest-backed
    /// client is unchanged: its callers bound what they read themselves.
    #[allow(irrefutable_let_patterns)]
    pub fn with_max_body(mut self, max_body: usize) -> Self {
        if let Backend::Transport {
            max_body: bound, ..
        } = &mut self.backend
        {
            *bound = max_body;
        }
        self
    }

    pub fn request(&self, method: Method, url: &str) -> FetchRequest {
        match &self.backend {
            #[cfg(not(target_arch = "wasm32"))]
            Backend::Reqwest(client) => FetchRequest {
                inner: RequestInner::Reqwest(client.request(method, url)),
            },
            Backend::Transport { timeout, max_body } => FetchRequest {
                inner: RequestInner::Transport(TransportRequest {
                    method,
                    url: url.to_string(),
                    headers: Vec::new(),
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

/// One request being built. Errors in a header or a body are held until [`Self::send`], as
/// reqwest's builder does.
pub struct FetchRequest {
    inner: RequestInner,
}

enum RequestInner {
    #[cfg(not(target_arch = "wasm32"))]
    Reqwest(reqwest::RequestBuilder),
    Transport(TransportRequest),
}

struct TransportRequest {
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Bytes>,
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.header(name.as_ref(), value.as_ref())),
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.json(value)),
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.query(query)),
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.form(value)),
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.body(body)),
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.basic_auth(user, password)),
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
            RequestInner::Reqwest(builder) => Self {
                inner: RequestInner::Reqwest(builder.timeout(timeout)),
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
            RequestInner::Reqwest(builder) => Ok(FetchResponse {
                inner: ResponseInner::Reqwest(builder.send().await?),
            }),
            RequestInner::Transport(req) => {
                if let Some(error) = req.error {
                    return Err(error);
                }
                let response = transport::fetch_response(
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
    Reqwest(reqwest::Response),
    Transport {
        status: StatusCode,
        headers: HeaderMap,
        body: Option<Bytes>,
    },
}

impl FetchResponse {
    pub fn status(&self) -> StatusCode {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response) => response.status(),
            ResponseInner::Transport { status, .. } => *status,
        }
    }

    pub fn headers(&self) -> &HeaderMap {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response) => response.headers(),
            ResponseInner::Transport { headers, .. } => headers,
        }
    }

    /// The body's length as the response declared it, where it did.
    pub fn content_length(&self) -> Option<u64> {
        match &self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response) => response.content_length(),
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
            ResponseInner::Reqwest(response) => Ok(response.chunk().await?),
            ResponseInner::Transport { body, .. } => Ok(body.take().filter(|b| !b.is_empty())),
        }
    }

    /// The whole body.
    pub async fn bytes(self) -> Result<Bytes> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response) => Ok(response.bytes().await?),
            ResponseInner::Transport { body, .. } => Ok(body.unwrap_or_default()),
        }
    }

    /// The whole body as text. reqwest decodes by the declared charset; the transport reads
    /// UTF-8, lossily.
    pub async fn text(self) -> Result<String> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response) => Ok(response.text().await?),
            ResponseInner::Transport { body, .. } => {
                Ok(String::from_utf8_lossy(&body.unwrap_or_default()).into_owned())
            }
        }
    }

    /// The whole body parsed as JSON.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        match self.inner {
            #[cfg(not(target_arch = "wasm32"))]
            ResponseInner::Reqwest(response) => Ok(response.json().await?),
            ResponseInner::Transport { body, .. } => {
                serde_json::from_slice(&body.unwrap_or_default())
                    .context("decode the response body as JSON")
            }
        }
    }
}
