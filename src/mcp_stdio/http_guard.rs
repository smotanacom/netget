//! Admission policy for the MCP Streamable HTTP endpoint (`--mcp-http`).
//!
//! Every tool on that endpoint is a control-plane verb — `start_server` accepts
//! `event_handlers` whose `script` handlers run `python3 -c <code>` as the operator —
//! so whoever can POST to `/mcp` can run code. rmcp's transport checks `Accept` and
//! `Content-Type` and nothing else: no `Origin`, no `Host`, no credential. The MCP
//! specification requires a localhost server to validate `Origin` precisely because a
//! web page can reach a loopback port: a plain cross-origin POST is stopped by the
//! browser's preflight, but DNS rebinding makes the page same-origin with
//! `127.0.0.1:PORT` and then `fetch("/mcp")` carries no `Origin` at all. What such a
//! request does carry is the rebound `Host` — the attacker's hostname — which is why
//! the loopback policy below checks both.
//!
//! Two modes, decided once at startup:
//!
//! - **Loopback, no token** (the default): the listener must be on a loopback address,
//!   `Host` (or the HTTP/2 `:authority`) must name a loopback host or the literal bind
//!   address, and an `Origin`, when a browser sends one, must be a loopback origin.
//! - **Token** (`--mcp-token` / `NETGET_MCP_TOKEN`): every request must carry
//!   `Authorization: Bearer <token>`. A rebound or cross-origin page cannot know the
//!   token, so `Host` and `Origin` are not restricted, which is what lets a web-based
//!   MCP client on another origin use the endpoint. Binding anything but a loopback
//!   address *requires* this mode; `HttpGuard::new` refuses otherwise.
//!
//! Independently of either mode, a request body is capped at
//! [`MAX_REQUEST_BODY_BYTES`]: rmcp collects the whole body before parsing it, and
//! axum's default body limit applies to extractors, not to a nested service reading
//! the raw body.

use std::net::IpAddr;

use http::header::{AUTHORIZATION, CONTENT_LENGTH, HOST, ORIGIN};
use http::{HeaderMap, StatusCode, Uri};

/// Environment variable read when `--mcp-token` is not passed.
pub const TOKEN_ENV: &str = crate::cli::MCP_TOKEN_ENV;

/// Largest request body accepted on `/mcp`. A JSON-RPC tool call is a few kilobytes;
/// 4 MiB leaves room for a large `event_handlers` table or `initial_memory`.
pub const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Why a request was turned away, with the status to answer it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub reason: &'static str,
}

impl Refusal {
    fn forbidden(reason: &'static str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            reason,
        }
    }
}

/// The admission policy for one listener.
#[derive(Debug, Clone)]
pub struct HttpGuard {
    bind: IpAddr,
    token: Option<String>,
}

impl HttpGuard {
    /// Build the policy for a listener on `bind`.
    ///
    /// Refuses a non-loopback bind without a token: that configuration would expose an
    /// unauthenticated code-execution endpoint to the whole network, and the old
    /// `--listen-addr` help text ("IP address to bind servers to") led operators to it
    /// while trying to expose their *protocol* servers.
    pub fn new(bind: IpAddr, token: Option<String>) -> anyhow::Result<Self> {
        if let Some(token) = &token {
            anyhow::ensure!(
                !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()),
                "the MCP token must be non-empty printable ASCII without spaces"
            );
        }
        if !bind.is_loopback() && token.is_none() {
            anyhow::bail!(
                "--mcp-http on {} would expose an unauthenticated control plane to the network \
                 (every MCP tool runs with this process's privileges, and start_server accepts \
                 script handlers). Bind a loopback address, or set --mcp-token / {} and require \
                 a bearer token.",
                bind,
                TOKEN_ENV
            );
        }
        Ok(Self { bind, token })
    }

    /// Whether requests must carry a bearer token.
    pub fn requires_token(&self) -> bool {
        self.token.is_some()
    }

    /// Decide whether a request may reach the MCP service.
    ///
    /// `uri` supplies the HTTP/2 `:authority` when there is no `Host` header.
    pub fn check(&self, headers: &HeaderMap, uri: &Uri) -> Result<(), Refusal> {
        if let Some(length) = headers
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            if length > MAX_REQUEST_BODY_BYTES as u64 {
                return Err(Refusal {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    reason: "request body exceeds the MCP endpoint's limit",
                });
            }
        }

        if let Some(expected) = &self.token {
            let presented = headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(bearer_of);
            return match presented {
                Some(presented) if constant_time_eq(presented.as_bytes(), expected.as_bytes()) => {
                    Ok(())
                }
                _ => Err(Refusal {
                    status: StatusCode::UNAUTHORIZED,
                    reason: "missing or invalid bearer token",
                }),
            };
        }

        // Loopback mode: the request must *name* a loopback host. A DNS-rebound page
        // reaches this socket with its own hostname in Host.
        let authority = headers
            .get(HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| uri.authority().map(|a| a.as_str().to_owned()));
        match authority.as_deref().map(host_of) {
            Some(host) if self.is_local_host(host) => {}
            Some(_) => {
                return Err(Refusal::forbidden(
                    "Host does not name this loopback listener (DNS rebinding?)",
                ))
            }
            None => return Err(Refusal::forbidden("request names no host")),
        }

        // Browsers send Origin on cross-site requests and on every POST; a page served
        // from anywhere but loopback has no business here without a token.
        if let Some(origin) = headers.get(ORIGIN) {
            let origin = origin.to_str().unwrap_or("");
            let allowed = origin
                .split_once("://")
                .map(|(_, rest)| self.is_local_host(host_of(rest)))
                .unwrap_or(false);
            if !allowed {
                return Err(Refusal::forbidden(
                    "Origin is not a loopback origin; set --mcp-token to serve other origins",
                ));
            }
        }
        Ok(())
    }

    fn is_local_host(&self, host: &str) -> bool {
        let host = host.trim_matches(|c| c == '[' || c == ']');
        if host.eq_ignore_ascii_case("localhost") {
            return true;
        }
        match host.parse::<IpAddr>() {
            Ok(ip) => ip.is_loopback() || ip == self.bind,
            Err(_) => false,
        }
    }
}

/// `scheme://host:port/path` or `host:port` → `host` (brackets kept for IPv6, the
/// caller strips them). A bracketed IPv6 literal may itself contain colons, so the
/// port is split off only after the closing bracket.
fn host_of(authority: &str) -> &str {
    let authority = authority.split('/').next().unwrap_or("");
    if let Some(rest) = authority.strip_prefix('[') {
        return match rest.find(']') {
            Some(end) => &authority[..end + 2],
            None => authority,
        };
    }
    authority.split(':').next().unwrap_or("")
}

fn bearer_of(value: &str) -> Option<&str> {
    let (scheme, token) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Compare without an early exit on the first differing byte, so timing does not
/// reveal how much of the token a guess got right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
