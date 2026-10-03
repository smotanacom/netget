use crate::server::prometheus_remote_write::codec::WriteBatch;
use anyhow::{ensure, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    client::conn::http1,
    header::{AUTHORIZATION, CONNECTION, CONTENT_ENCODING, CONTENT_TYPE, HOST, USER_AGENT},
    Request, Uri,
};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::time::Duration;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const MIN_BACKOFF: Duration = Duration::from_millis(100);
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);
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
            value.to_owned()
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
        Ok(Self {
            authority: authority.as_str().into(),
            connect_addr: format!(
                "{}:{}",
                authority.host(),
                authority.port_u16().unwrap_or(9090)
            ),
        })
    }
}
#[derive(Clone)]
pub struct Config {
    pub origin: Origin,
    pub token: Option<String>,
    pub path: String,
    pub retry_429: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct WriteResponse {
    pub series_count: usize,
    pub sample_count: usize,
    pub status: u16,
    pub accepted: bool,
    pub attempts: u64,
    pub durable_storage_confirmed: bool,
}
pub async fn write(config: Config, batch: WriteBatch, body: Vec<u8>) -> Result<WriteResponse> {
    let mut attempts = 0u64;
    let mut backoff = MIN_BACKOFF;
    loop {
        attempts = attempts.saturating_add(1);
        match write_once(&config, &body).await {
            Ok(status) if (500..600).contains(&status) || (status == 429 && config.retry_429) => {}
            Ok(status) => {
                return Ok(WriteResponse {
                    series_count: batch.series.len(),
                    sample_count: batch.series.iter().map(|s| s.samples.len()).sum(),
                    status,
                    accepted: (200..300).contains(&status),
                    attempts,
                    durable_storage_confirmed: false,
                })
            }
            // A transport failure can occur after remote acceptance. Retrying the
            // identical batch is intentional; no exactly-once/dedup claim is made.
            Err(_) => {}
        }
        // The caller owns this future. Disconnect/removal drops both the current
        // exchange and backoff; one batch remains bounded in volatile memory only.
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(MAX_BACKOFF);
    }
}
pub async fn write_once(config: &Config, body: &[u8]) -> Result<u16> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let socket = tokio::net::TcpStream::connect(&config.origin.connect_addr).await?;
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(MAX_RESPONSE_HEADERS)
            .max_buf_size(MAX_RESPONSE_HEADER_BYTES)
            .handshake(TokioIo::new(socket))
            .await?;
        let mut request = Request::builder()
            .method("POST")
            .uri(&config.path)
            .header(HOST, &config.origin.authority)
            .header(CONNECTION, "close")
            .header(CONTENT_TYPE, "application/x-protobuf")
            .header(CONTENT_ENCODING, "snappy")
            .header(USER_AGENT, concat!("netget/", env!("CARGO_PKG_VERSION")))
            .header("X-Prometheus-Remote-Write-Version", "0.1.0");
        if let Some(token) = &config.token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request.body(Full::new(Bytes::copy_from_slice(body)))?;
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
            // The v1 response body is reserved and MUST be ignored, including
            // successful nonempty/binary bodies. Bounded draining validates HTTP
            // framing without treating text, encodings or Retry-After as schema.
            Limited::new(body, MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|_| anyhow::anyhow!("invalid/oversized response body"))?;
            Ok::<_, anyhow::Error>(parts.status.as_u16())
        };
        tokio::pin!(connection);
        tokio::pin!(exchange);
        tokio::select! {
            result = &mut exchange => result,
            result = &mut connection => { result.context("HTTP driver failed")?; exchange.await }
        }
    })
    .await
    .context("remote write exchange deadline exceeded")?
}
