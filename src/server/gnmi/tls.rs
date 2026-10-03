//! Bounded regular-file TLS inputs; no blocking FIFO open and no verification bypass.
use anyhow::{ensure, Context, Result};
use std::{
    io::{BufReader, Read},
    sync::Arc,
};
pub const MAX_PEM_BYTES: usize = 1024 * 1024;
pub async fn read_pem(path: String) -> Result<Vec<u8>> {
    ensure!(path.len() <= 4096, "TLS path exceeds 4096 bytes");
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let metadata = std::fs::metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_PEM_BYTES as u64,
            "TLS input must be a regular file at most 1 MiB"
        );
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= MAX_PEM_BYTES as u64,
            "TLS input must be a regular file at most 1 MiB"
        );
        let mut bytes = Vec::new();
        file.take(MAX_PEM_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_PEM_BYTES, "TLS input exceeds 1 MiB");
        Ok(bytes)
    })
    .await?
}
pub async fn server(cert: String, key: String) -> Result<Arc<rustls::ServerConfig>> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        let cert = read_pem(cert).await?;
        let key = read_pem(key).await?;
        let certs = rustls_pemfile::certs(&mut BufReader::new(cert.as_slice()))
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            !certs.is_empty() && certs.len() <= 16,
            "TLS requires 1..16 certificates"
        );
        let key = rustls_pemfile::private_key(&mut BufReader::new(key.as_slice()))?
            .context("TLS private key absent")?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        config.alpn_protocols = vec![b"h2".to_vec()];
        Ok::<_, anyhow::Error>(Arc::new(config))
    })
    .await
    .context("TLS credential startup deadline exceeded")?
}
