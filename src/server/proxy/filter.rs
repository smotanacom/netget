//! Proxy filtering and modification system
//!
//! This module provides sophisticated request/response filtering and modification
//! capabilities for the MITM proxy.
//!
//! # Certificate Modes
//!
//! The proxy supports three certificate modes:
//!
//! 1. **Generate** - Generate a self-signed CA certificate on-demand for full MITM
//! 2. **LoadFromFile** - Load CA certificate from specified path for full MITM
//! 3. **None** - No MITM, pass-through origin certificates (allow/block only)
//!
//! ## Full MITM Mode (Generate or LoadFromFile)
//!
//! When a certificate is configured:
//! - HTTPS traffic is decrypted and re-encrypted
//! - LLM sees: method, URL, path, headers, body content
//! - LLM can: pass, block, or modify (headers, URL, body)
//! - HTTP traffic is also fully inspectable and modifiable
//!
//! ## Pass-Through Mode (None)
//!
//! When no certificate is configured:
//! - HTTPS traffic passes through with origin certificate (no decryption)
//! - LLM sees: destination host, destination port, SNI (Server Name Indication)
//! - LLM can: allow or block only (no modifications possible)
//! - HTTP traffic is still fully inspectable and modifiable (no TLS involved)

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Certificate mode for MITM proxy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum CertificateMode {
    /// Generate a self-signed CA certificate on-demand
    Generate,

    /// Load CA certificate from file
    LoadFromFile {
        /// Path to CA certificate file (PEM format)
        cert_path: PathBuf,
        /// Path to CA private key file (PEM format)
        key_path: PathBuf,
    },

    /// No MITM - pass through origin certificates
    /// LLM can only allow/block based on connection info
    None,
}

impl Default for CertificateMode {
    fn default() -> Self {
        CertificateMode::Generate
    }
}

/// Information available to LLM for HTTPS connection (pass-through mode)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpsConnectionInfo {
    /// Destination host (from CONNECT request or SNI)
    pub destination_host: String,
    /// Destination port
    pub destination_port: u16,
    /// SNI (Server Name Indication) from TLS handshake if available
    pub sni: Option<String>,
    /// Client IP address
    pub client_addr: String,
}

/// Full request information available in MITM mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullRequestInfo {
    /// HTTP method (GET, POST, etc.)
    pub method: String,
    /// Full URL
    pub url: String,
    /// URL path
    pub path: String,
    /// Host header
    pub host: String,
    /// Request headers
    pub headers: HashMap<String, String>,
    /// Request body (may be empty)
    pub body: Vec<u8>,
    /// Client IP address
    pub client_addr: String,
}

/// Full response information available in MITM mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullResponseInfo {
    /// HTTP status code
    pub status: u16,
    /// Response headers
    pub headers: HashMap<String, String>,
    /// Response body (may be empty)
    pub body: Vec<u8>,
    /// Originating request host
    pub request_host: String,
    /// Originating request path
    pub request_path: String,
}

/// Filter configuration for intercepting requests
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFilter {
    /// Optional regex to match against host/server (e.g., "^api\\.example\\.com$")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_regex: Option<String>,

    /// Optional regex to match against URL path (e.g., "^/api/v1/.*")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_regex: Option<String>,

    /// Optional regex to match against HTTP method (e.g., "^(POST|PUT)$")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method_regex: Option<String>,

    /// Optional regex to match against request headers (format: "Header-Name: value-pattern")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_regex: Option<String>,

    /// Optional regex to match against request body content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_regex: Option<String>,
}

/// Filter configuration for intercepting HTTPS connections (pass-through mode)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpsConnectionFilter {
    /// Optional regex to match against destination host
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_regex: Option<String>,

    /// Optional regex to match against destination port
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port_regex: Option<String>,

    /// Optional regex to match against TLS SNI (Server Name Indication)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sni_regex: Option<String>,

    /// Optional regex to match against client address
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_addr_regex: Option<String>,
}

/// Filter configuration for intercepting responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFilter {
    /// Optional regex to match against response status code (e.g., "^(4|5)\\d{2}$" for errors)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_regex: Option<String>,

    /// Optional regex to match against response headers (format: "Header-Name: value-pattern")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_regex: Option<String>,

    /// Optional regex to match against response body content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_regex: Option<String>,

    /// Optional regex to match against originating request host (to filter responses by request)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_host_regex: Option<String>,

    /// Optional regex to match against originating request path
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_path_regex: Option<String>,
}

/// Actions that can be taken on a filtered request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum RequestAction {
    /// Pass the request through unmodified
    Pass,

    /// Block the request and return an error response
    Block {
        /// HTTP status code to return (default: 403)
        #[serde(default = "default_block_status")]
        status: u16,
        /// Response body (default: "Request blocked by proxy")
        #[serde(default = "default_block_body")]
        body: String,
    },

    /// Modify the request before forwarding
    Modify {
        /// Headers to add or modify (key-value pairs)
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,

        /// Headers to remove (list of header names)
        #[serde(skip_serializing_if = "Option::is_none")]
        remove_headers: Option<Vec<String>>,

        /// New URL path (replaces entire path)
        #[serde(skip_serializing_if = "Option::is_none")]
        new_path: Option<String>,

        /// Query parameters to add/modify
        #[serde(skip_serializing_if = "Option::is_none")]
        query_params: Option<HashMap<String, String>>,

        /// Complete body replacement
        #[serde(skip_serializing_if = "Option::is_none")]
        new_body: Option<String>,

        /// Regex-based body replacement (pattern -> replacement)
        #[serde(skip_serializing_if = "Option::is_none")]
        body_replacements: Option<Vec<RegexReplacement>>,
    },
}

/// Actions that can be taken on a filtered response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ResponseAction {
    /// Pass the response through unmodified
    Pass,

    /// Block the response and return a different one
    Block {
        /// HTTP status code to return (default: 502)
        #[serde(default = "default_response_block_status")]
        status: u16,
        /// Response body (default: "Response blocked by proxy")
        #[serde(default = "default_response_block_body")]
        body: String,
    },

    /// Modify the response before returning to client
    Modify {
        /// New status code
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<u16>,

        /// Headers to add or modify (key-value pairs)
        #[serde(skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,

        /// Headers to remove (list of header names)
        #[serde(skip_serializing_if = "Option::is_none")]
        remove_headers: Option<Vec<String>>,

        /// Complete body replacement
        #[serde(skip_serializing_if = "Option::is_none")]
        new_body: Option<String>,

        /// Regex-based body replacement (pattern -> replacement)
        #[serde(skip_serializing_if = "Option::is_none")]
        body_replacements: Option<Vec<RegexReplacement>>,
    },
}

/// Regex-based text replacement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegexReplacement {
    /// Regex pattern to match
    pub pattern: String,
    /// Replacement text (supports capture groups like $1, $2)
    pub replacement: String,
}

/// Actions available for HTTPS connections in pass-through mode (no MITM)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum HttpsConnectionAction {
    /// Allow the HTTPS connection to proceed
    Allow,

    /// Block the HTTPS connection
    Block {
        /// Optional message to log
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Complete proxy filter configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyFilterConfig {
    /// Certificate mode for MITM proxy
    #[serde(default)]
    pub certificate_mode: CertificateMode,

    /// List of request filters (if empty, intercept all requests)
    #[serde(default)]
    pub request_filters: Vec<RequestFilter>,

    /// List of response filters (if empty, intercept all responses)
    #[serde(default)]
    pub response_filters: Vec<ResponseFilter>,

    /// List of HTTPS connection filters (pass-through mode only)
    #[serde(default)]
    pub https_connection_filters: Vec<HttpsConnectionFilter>,

    /// If true, only intercept requests that match filters (default: intercept all)
    #[serde(default)]
    pub request_filter_mode: FilterMode,

    /// If true, only intercept responses that match filters (default: intercept all)
    #[serde(default)]
    pub response_filter_mode: FilterMode,

    /// Filter mode for HTTPS connections (pass-through mode)
    #[serde(default)]
    pub https_connection_filter_mode: FilterMode,
}

impl Default for ProxyFilterConfig {
    fn default() -> Self {
        Self {
            certificate_mode: CertificateMode::None, // Default to pass-through mode
            request_filters: Vec::new(),
            response_filters: Vec::new(),
            https_connection_filters: Vec::new(),
            request_filter_mode: FilterMode::All,
            response_filter_mode: FilterMode::All,
            https_connection_filter_mode: FilterMode::All,
        }
    }
}

/// Filter mode determines behavior when no filters match
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FilterMode {
    /// Intercept everything (consult LLM for all requests/responses)
    #[default]
    All,

    /// Only intercept if filters match (pass through others without LLM)
    MatchOnly,

    /// Don't intercept anything (pass everything through)
    None,
}

// Default values for serde
/// Headers the proxy frames itself when it rewrites a message. A model-added
/// `Content-Length` beside the one the proxy computes, or a `Transfer-Encoding: chunked`
/// over a body the proxy writes whole, is the ambiguity request-smuggling and
/// response-desync attacks are built on, so neither may be added through `headers`
/// (`remove_headers` may still name them).
pub const FRAMING_HEADERS: [&str; 2] = ["content-length", "transfer-encoding"];

/// A header name is an RFC 9110 token.
///
/// The names and values in `handle_request_modify` / `handle_response_modify` come from the
/// model, and the model reads the peer's own request, so a prompt-injected value such as
/// `"x\r\nContent-Length: 0\r\n\r\nGET /admin HTTP/1.1"` would otherwise be written into
/// the forwarded message verbatim — one header turning into a second request upstream, or a
/// second response to the client. These checks are applied where the action is executed and
/// again where the bytes are written.
pub fn check_header_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 256
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
        "header name {name:?} is not an HTTP token"
    );
    Ok(())
}

/// A header value carries no CR, LF or other control character (HTAB excepted).
pub fn check_header_value(name: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= 8192 && !value.chars().any(|c| c.is_control() && c != '\t'),
        "header {name:?} contains a control character (CR/LF would split the message)"
    );
    Ok(())
}

/// Every added header is a token with a control-free value and is not a framing header.
pub fn check_modify_headers(headers: &HashMap<String, String>) -> anyhow::Result<()> {
    anyhow::ensure!(headers.len() <= 64, "at most 64 headers");
    for (name, value) in headers {
        check_header_name(name)?;
        check_header_value(name, value)?;
        anyhow::ensure!(
            !FRAMING_HEADERS.contains(&name.to_ascii_lowercase().as_str()),
            "header {name:?} is set by the proxy from the body it writes; it cannot be added \
             (it may be named in remove_headers)"
        );
    }
    Ok(())
}

/// A piece of the request target (`new_path`, a query name or value) carries no whitespace
/// or control character: the request line is space-delimited and CRLF-terminated, so either
/// would end the line early.
pub fn check_request_target_part(what: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() <= 8192 && !value.chars().any(|c| c.is_ascii_whitespace() || c.is_control()),
        "{what} {value:?} contains whitespace or a control character; it would end the request line"
    );
    Ok(())
}

/// Every query name and value passes `check_request_target_part`.
pub fn check_query_params(params: &HashMap<String, String>) -> anyhow::Result<()> {
    anyhow::ensure!(params.len() <= 64, "at most 64 query_params");
    for (name, value) in params {
        check_request_target_part("query_params name", name)?;
        check_request_target_part("query_params value", value)?;
    }
    Ok(())
}

fn default_block_status() -> u16 {
    403
}
fn default_block_body() -> String {
    "Request blocked by proxy".to_string()
}
fn default_response_block_status() -> u16 {
    502
}
fn default_response_block_body() -> String {
    "Response blocked by proxy".to_string()
}

impl RequestFilter {
    /// Check if this filter matches the given request
    pub fn matches(
        &self,
        host: &str,
        path: &str,
        method: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> bool {
        // If any pattern is specified and doesn't match, return false

        if let Some(pattern) = &self.host_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(host) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.path_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(path) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.method_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(method) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.header_regex {
            if let Ok(re) = Regex::new(pattern) {
                // Format headers as "Name: Value" and check if any match
                let headers_text: String = headers
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect::<Vec<_>>()
                    .join("\n");

                if !re.is_match(&headers_text) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.body_regex {
            if let Ok(re) = Regex::new(pattern) {
                if let Ok(body_str) = std::str::from_utf8(body) {
                    if !re.is_match(body_str) {
                        return false;
                    }
                } else {
                    // Binary body doesn't match text pattern
                    return false;
                }
            }
        }

        // All specified patterns matched
        true
    }
}

impl HttpsConnectionFilter {
    /// Check if this filter matches the given HTTPS connection
    pub fn matches(
        &self,
        destination_host: &str,
        destination_port: u16,
        sni: Option<&str>,
        client_addr: &str,
    ) -> bool {
        // If any pattern is specified and doesn't match, return false

        if let Some(pattern) = &self.host_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(destination_host) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.port_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(&destination_port.to_string()) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.sni_regex {
            if let Ok(re) = Regex::new(pattern) {
                if let Some(sni_value) = sni {
                    if !re.is_match(sni_value) {
                        return false;
                    }
                } else {
                    // SNI pattern specified but SNI not available
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.client_addr_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(client_addr) {
                    return false;
                }
            }
        }

        // All specified patterns matched
        true
    }
}

impl ResponseFilter {
    /// Check if this filter matches the given response
    pub fn matches(
        &self,
        status: u16,
        headers: &HashMap<String, String>,
        body: &[u8],
        request_host: Option<&str>,
        request_path: Option<&str>,
    ) -> bool {
        // If any pattern is specified and doesn't match, return false

        if let Some(pattern) = &self.status_regex {
            if let Ok(re) = Regex::new(pattern) {
                if !re.is_match(&status.to_string()) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.header_regex {
            if let Ok(re) = Regex::new(pattern) {
                // Format headers as "Name: Value" and check if any match
                let headers_text: String = headers
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect::<Vec<_>>()
                    .join("\n");

                if !re.is_match(&headers_text) {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.body_regex {
            if let Ok(re) = Regex::new(pattern) {
                if let Ok(body_str) = std::str::from_utf8(body) {
                    if !re.is_match(body_str) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.request_host_regex {
            if let Ok(re) = Regex::new(pattern) {
                if let Some(host) = request_host {
                    if !re.is_match(host) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }

        if let Some(pattern) = &self.request_path_regex {
            if let Ok(re) = Regex::new(pattern) {
                if let Some(path) = request_path {
                    if !re.is_match(path) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }

        // All specified patterns matched
        true
    }
}

impl ProxyFilterConfig {
    /// Check if MITM mode is enabled (certificate available)
    pub fn is_mitm_mode(&self) -> bool {
        !matches!(self.certificate_mode, CertificateMode::None)
    }

    /// Check if a request should be intercepted for LLM consultation
    pub fn should_intercept_request(
        &self,
        host: &str,
        path: &str,
        method: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> bool {
        match self.request_filter_mode {
            FilterMode::All => true,
            FilterMode::None => false,
            FilterMode::MatchOnly => {
                if self.request_filters.is_empty() {
                    // No filters defined, don't intercept
                    false
                } else {
                    // Intercept if any filter matches
                    self.request_filters
                        .iter()
                        .any(|f| f.matches(host, path, method, headers, body))
                }
            }
        }
    }

    /// Check if a response should be intercepted for LLM consultation
    pub fn should_intercept_response(
        &self,
        status: u16,
        headers: &HashMap<String, String>,
        body: &[u8],
        request_host: Option<&str>,
        request_path: Option<&str>,
    ) -> bool {
        match self.response_filter_mode {
            FilterMode::All => true,
            FilterMode::None => false,
            FilterMode::MatchOnly => {
                if self.response_filters.is_empty() {
                    // No filters defined, don't intercept
                    false
                } else {
                    // Intercept if any filter matches
                    self.response_filters
                        .iter()
                        .any(|f| f.matches(status, headers, body, request_host, request_path))
                }
            }
        }
    }

    /// Check if an HTTPS connection (pass-through mode) should be intercepted for LLM consultation
    pub fn should_intercept_https_connection(
        &self,
        destination_host: &str,
        destination_port: u16,
        sni: Option<&str>,
        client_addr: &str,
    ) -> bool {
        // Only applies in pass-through mode (no MITM)
        if self.is_mitm_mode() {
            return false;
        }

        match self.https_connection_filter_mode {
            FilterMode::All => true,
            FilterMode::None => false,
            FilterMode::MatchOnly => {
                if self.https_connection_filters.is_empty() {
                    // No filters defined, don't intercept
                    false
                } else {
                    // Intercept if any filter matches
                    self.https_connection_filters
                        .iter()
                        .any(|f| f.matches(destination_host, destination_port, sni, client_addr))
                }
            }
        }
    }

    /// Check if this HTTP response should be inspected by LLM
    pub fn should_inspect_response(&self, _request_info: &FullRequestInfo) -> bool {
        // For now, inspect all responses in MITM mode
        // TODO: Add response-specific filters based on status code, content-type, etc.
        match self.response_filter_mode {
            FilterMode::None => false,
            FilterMode::All => true,
            FilterMode::MatchOnly => {
                // Check if any response filter matches
                // For now, default to true if filters exist
                !self.response_filters.is_empty()
            }
        }
    }
}
