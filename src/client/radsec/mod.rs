//! RadSec client (RFC 6614): the RADIUS client's transport and turns over one TLS connection.
pub mod actions;

use crate::client::radius::{Link, RadiusClient, Settings};
use crate::protocol::ConnectContext;
use crate::server::radius::packet::read_frames;
use crate::server::radsec::tls;
pub use actions::RadsecClientProtocol;
use anyhow::{ensure, Context, Result};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let get = |k: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten())
    };
    let ca_file = get("ca_file")?.context(
        "the RadSec client needs ca_file, the CA the server's certificate must chain to; it never connects unverified",
    )?;
    let config = tls::client_config(
        &ca_file,
        get("certificate_file")?.as_deref(),
        get("private_key_file")?.as_deref(),
    )?;
    let secret = get("shared_secret")?.unwrap_or_else(|| tls::DEFAULT_SECRET.to_string());
    ensure!(!secret.is_empty(), "shared_secret must not be empty");
    let timeout_ms = params
        .map(|p| p.get_optional_u64("timeout_ms"))
        .transpose()?
        .flatten()
        .unwrap_or(actions::DEFAULT_TIMEOUT_MS);
    ensure!(
        (100..=60_000).contains(&timeout_ms),
        "timeout_ms {timeout_ms} is outside 100-60000"
    );
    let peer = tokio::net::lookup_host(&ctx.remote_addr)
        .await
        .with_context(|| format!("cannot resolve RadSec server {}", ctx.remote_addr))?
        .next()
        .with_context(|| format!("{} resolved to no address", ctx.remote_addr))?;
    let name = match get("server_name")? {
        Some(n) => n,
        None => ctx
            .remote_addr
            .rsplit_once(':')
            .map_or(ctx.remote_addr.as_str(), |(h, _)| h)
            .trim_matches(['[', ']'])
            .to_string(),
    };
    let stream = tokio::time::timeout(tls::HANDSHAKE_TIMEOUT, async {
        let tcp = tokio::net::TcpStream::connect(peer).await?;
        let local = tcp.local_addr()?;
        let tls = tokio_rustls::TlsConnector::from(config)
            .connect(tls::server_name(&name)?, tcp)
            .await
            .context("RadSec TLS handshake failed")?;
        Ok::<_, anyhow::Error>((tls, local))
    })
    .await
    .context("RadSec connect and handshake timed out")??;
    let (tls, local) = stream;
    let (reader, writer) = tokio::io::split(tls);
    let (frames_tx, frames) = mpsc::channel(8);
    ctx.state
        .spawn_client_task(ctx.client_id, read_frames(reader, frames_tx))
        .await;
    RadiusClient::run(
        Link::Stream {
            writer: Box::new(writer),
            frames,
            peer,
        },
        peer,
        peer,
        Settings {
            secret: secret.into_bytes(),
            accounting_port: None,
            timeout: Duration::from_millis(timeout_ms),
            retries: 0,
        },
        ctx.llm_client,
        ctx.state,
        ctx.status_tx,
        ctx.client_id,
    )
    .await;
    Ok(local)
}
