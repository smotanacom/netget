//! Shared semantic HTTP fields and explicit memory bounds.
use anyhow::{ensure, Context, Result};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};
pub const MAX_BODY: usize = 8 * 1024 * 1024;
pub const MAX_HEADERS: usize = 32 * 1024;
pub const MAX_INBOUND_BYTES: usize = MAX_BODY + 2 * MAX_HEADERS;
fn header_size(headers: &HeaderMap) -> usize {
    headers.iter().fold(0usize, |size, (key, value)| {
        size.saturating_add(key.as_str().len() + value.as_bytes().len() + 32)
    })
}
pub fn check_headers(headers: &HeaderMap) -> Result<()> {
    ensure!(
        header_size(headers) <= MAX_HEADERS,
        "HTTP3 field section exceeds 32 KiB"
    );
    for (k, _) in headers {
        ensure!(
            !matches!(
                k.as_str(),
                "connection" | "keep-alive" | "proxy-connection" | "transfer-encoding" | "upgrade"
            ),
            "HTTP3 forbids connection-specific headers"
        );
    }
    Ok(())
}
/// Outgoing field sections include pseudo-fields, even when the peer advertises
/// a larger limit. h3 enforces the full decoded bound on incoming sections.
pub fn check_field_section(headers: &HeaderMap, pseudo: &[(&str, &str)]) -> Result<()> {
    check_headers(headers)?;
    let size = pseudo
        .iter()
        .fold(header_size(headers), |size, (key, value)| {
            size.saturating_add(key.len() + value.len() + 32)
        });
    ensure!(size <= MAX_HEADERS, "HTTP3 field section exceeds 32 KiB");
    Ok(())
}
pub fn parse_headers(value: &Value) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    if value.is_null() {
        return Ok(headers);
    }
    let object = value
        .as_object()
        .context("HTTP3 headers must be an object")?;
    let mut size = 0usize;
    for (key, value) in object {
        let name = HeaderName::from_bytes(key.as_bytes())?;
        let values: Vec<&str> = if let Some(s) = value.as_str() {
            vec![s]
        } else {
            value
                .as_array()
                .context("Header must be a string or string array")?
                .iter()
                .map(|v| v.as_str().context("Header array must contain strings"))
                .collect::<Result<_>>()?
        };
        for v in values {
            size = size.saturating_add(key.len() + v.len() + 32);
            ensure!(size <= MAX_HEADERS, "HTTP3 field section exceeds 32 KiB");
            headers.append(name.clone(), HeaderValue::from_str(v)?);
        }
    }
    check_headers(&headers)?;
    Ok(headers)
}
pub fn header_json(headers: &HeaderMap) -> Result<Map<String, Value>> {
    check_headers(headers)?;
    let mut object = Map::new();
    for key in headers.keys() {
        let values: Vec<_> = headers
            .get_all(key)
            .iter()
            .map(|v| v.to_str().map(str::to_owned))
            .collect::<std::result::Result<_, _>>()?;
        object.insert(
            key.to_string(),
            if values.len() == 1 {
                Value::String(values[0].clone())
            } else {
                serde_json::json!(values)
            },
        );
    }
    Ok(object)
}
pub fn validate_length(headers: &HeaderMap, actual: usize) -> Result<()> {
    for value in headers.get_all("content-length") {
        ensure!(
            value.to_str()?.parse::<usize>()? == actual,
            "HTTP3 content-length does not match body"
        );
    }
    Ok(())
}

/// HTTP/3 uses H3_NO_ERROR (0x100), unlike raw QUIC/DoQ's application zero.
/// Close first so the shared transport guards' subsequent Drop is harmless.
pub struct EndpointOwner(pub crate::utils::quic::EndpointGuard);
impl std::ops::Deref for EndpointOwner {
    type Target = quinn::Endpoint;
    fn deref(&self) -> &Self::Target {
        &self.0 .0
    }
}
impl Drop for EndpointOwner {
    fn drop(&mut self) {
        self.0 .0.close(0x100u32.into(), b"HTTP3 endpoint stopped");
    }
}
pub struct ConnectionOwner(pub crate::utils::quic::ConnectionGuard);
impl std::ops::Deref for ConnectionOwner {
    type Target = quinn::Connection;
    fn deref(&self) -> &Self::Target {
        &self.0 .0
    }
}
impl Drop for ConnectionOwner {
    fn drop(&mut self) {
        self.0
             .0
            .close(0x100u32.into(), b"HTTP3 connection stopped");
    }
}
