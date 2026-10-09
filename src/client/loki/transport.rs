use crate::server::loki::codec::{self, PushBatch};
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    client::conn::http1,
    header::{
        AUTHORIZATION, CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST,
        RETRY_AFTER, TRANSFER_ENCODING,
    },
    Request, Uri,
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use std::time::Duration;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_HEADERS: usize = 64;
pub const MAX_RESPONSE_HEADER_BYTES: usize = 32 * 1024;
#[derive(Clone)]
pub struct Origin {
    pub authority: String,
    pub connect_addr: String,
}
impl Origin {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(
            !value.contains(['#', '@']),
            "HTTP origin cannot contain credentials/fragment"
        );
        let value = if value.starts_with("http://") {
            value.to_string()
        } else {
            format!("http://{value}")
        };
        let uri: Uri = value.parse().context("invalid HTTP origin")?;
        ensure!(
            uri.scheme_str() == Some("http")
                && uri.query().is_none()
                && matches!(uri.path(), "" | "/"),
            "cleartext HTTP origin only, without path/query"
        );
        let authority = uri.authority().context("HTTP host required")?;
        ensure!(authority.as_str().len() <= 1024, "HTTP origin byte limit");
        let host = authority.host();
        let connect_addr = format!("{}:{}", host, authority.port_u16().unwrap_or(3100));
        Ok(Self {
            authority: authority.as_str().to_string(),
            connect_addr,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PushError {
    pub message: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct PushResponse {
    pub tenant_id: String,
    pub encoding: codec::Encoding,
    pub stream_count: usize,
    pub entry_count: usize,
    pub status: u16,
    pub error: Option<PushError>,
    pub retry_after_seconds: Option<u16>,
}
pub async fn write(
    origin: Origin,
    token: Option<String>,
    batch: PushBatch,
    body: Vec<u8>,
) -> Result<PushResponse> {
    tokio::time::timeout(IO_TIMEOUT, async move {
        let socket = tokio::net::TcpStream::connect(&origin.connect_addr).await?;
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(MAX_RESPONSE_HEADERS)
            .max_buf_size(MAX_RESPONSE_HEADER_BYTES)
            .handshake(TokioIo::new(socket))
            .await?;
        let mut request = Request::builder()
            .method("POST")
            .uri("/loki/api/v1/push")
            .header(HOST, &origin.authority)
            .header(CONNECTION, "close")
            .header(
                CONTENT_TYPE,
                if batch.encoding == codec::Encoding::SnappyProtobuf {
                    "application/x-protobuf"
                } else {
                    "application/json"
                },
            );
        if batch.encoding == codec::Encoding::GzipJson {
            request = request.header(CONTENT_ENCODING, "gzip");
        }
        if let Some(tenant) = &batch.tenant_id {
            request = request.header("X-Scope-OrgID", tenant);
        }
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request.body(Full::new(Bytes::from(body)))?;
        let exchange = async {
            let response = sender.send_request(request).await?;
            let (parts, body) = response.into_parts();
            ensure!(
                parts.headers.len() <= MAX_RESPONSE_HEADERS
                    && parts
                        .headers
                        .iter()
                        .map(|(k, v)| k.as_str().len() + v.as_bytes().len() + 4)
                        .sum::<usize>()
                        <= MAX_RESPONSE_HEADER_BYTES,
                "response header count/byte limit"
            );
            ensure!(
                parts.headers.get_all(RETRY_AFTER).iter().count() <= 1,
                "duplicate Retry-After"
            );
            let retry = parts
                .headers
                .get(RETRY_AFTER)
                .map(|h| {
                    h.to_str()
                        .context("invalid Retry-After")?
                        .parse::<u16>()
                        .context("only numeric Retry-After supported")
                })
                .transpose()?;
            if let Some(n) = retry {
                ensure!(
                    (1..=3600).contains(&n) && matches!(parts.status.as_u16(), 429 | 503),
                    "invalid Retry-After range/status"
                );
            }
            ensure!(
                parts.headers.get_all(CONTENT_ENCODING).iter().count() <= 1,
                "duplicate response encoding"
            );
            ensure!(
                parts
                    .headers
                    .get(CONTENT_ENCODING)
                    .map(|v| v.as_bytes() == b"identity")
                    .unwrap_or(true),
                "compressed HTTP responses unsupported"
            );
            let bytes = Limited::new(body, MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|_| anyhow::anyhow!("invalid/oversized response body"))?
                .to_bytes();
            let status = parts.status.as_u16();
            let error = if status == 204 {
                ensure!(
                    bytes.is_empty()
                        && retry.is_none()
                        && parts
                            .headers
                            .get_all(CONTENT_LENGTH)
                            .iter()
                            .all(|h| h.as_bytes() == b"0")
                        && !parts.headers.contains_key(TRANSFER_ENCODING),
                    "204 response must be empty"
                );
                None
            } else {
                ensure!(
                    [260, 400, 401, 403, 404, 405, 408, 413, 415, 422, 429, 500, 503]
                        .contains(&status),
                    "unsupported Loki push status"
                );
                let message = std::str::from_utf8(&bytes).context("Loki error must be UTF-8")?;
                ensure!(
                    !message.is_empty()
                        && message.len() <= 4096
                        && !message
                            .chars()
                            .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')),
                    "error text byte/control limit"
                );
                let error = PushError {
                    message: message.to_owned(),
                };
                Some(error)
            };
            Ok::<_, anyhow::Error>(PushResponse {
                tenant_id: batch.tenant_id.unwrap_or_else(|| "fake".into()),
                encoding: batch.encoding,
                stream_count: batch.streams.len(),
                entry_count: batch.streams.iter().map(|s| s.entries.len()).sum(),
                status,
                error,
                retry_after_seconds: retry,
            })
        };
        tokio::pin!(connection);
        tokio::pin!(exchange);
        // Both futures own the single TCP exchange; dropping either path cancels IO.
        tokio::select! {
            result = &mut exchange => result,
            result = &mut connection => {
                result.context("HTTP driver failed")?;
                exchange.await
            }
        }
    })
    .await
    .context("Loki exchange deadline exceeded")?
}
