use super::wire::Request;
use crate::protocol::ConnectContext;
use anyhow::{ensure, Context, Result};
use rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use std::sync::Arc;
#[derive(Clone)]
pub struct Config {
    pub tls: Arc<ClientConfig>,
    pub name: ServerName<'static>,
    pub host: String,
    pub port: u16,
}
impl Config {
    pub fn from_context(ctx: &ConnectContext) -> Result<Self> {
        let optional = |name| {
            ctx.startup_params
                .as_ref()
                .map(|p| p.get_optional_string(name))
                .transpose()
                .map(Option::flatten)
        };
        let remote = url::Url::parse(&format!("gemini://{}", ctx.remote_addr))
            .context("Invalid remote address")?;
        ensure!(
            remote.username().is_empty()
                && remote.password().is_none()
                && remote.path().is_empty()
                && remote.query().is_none()
                && remote.fragment().is_none(),
            "Remote address must contain only host:port"
        );
        let host = optional("server_name")?
            .unwrap_or_else(|| remote.host().map(|h| h.to_string()).unwrap_or_default());
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let name = ServerName::try_from(host.clone()).context("Invalid TLS server name")?;
        let mut roots = RootCertStore::empty();
        if let Some(pem) = optional("custom_ca_cert_pem")? {
            ensure!(pem.len() <= 256 * 1024, "CA PEM exceeds256KiB");
            let certs = rustls_pemfile::certs(&mut pem.as_bytes())
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(
                !certs.is_empty() && certs.len() <= 32,
                "Expected1..32 CA certificates"
            );
            for cert in certs {
                roots.add(cert)?;
            }
        } else {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        let tls =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
        Ok(Self {
            tls: Arc::new(tls),
            name,
            host,
            port: remote.port().unwrap_or(1965),
        })
    }
    pub fn validate(&self, r: &Request) -> Result<()> {
        let host = r.url.host().context("Missing URL host")?.to_string();
        ensure!(host.trim_start_matches('[').trim_end_matches(']').eq_ignore_ascii_case(&self.host)&&r.url.port().unwrap_or(1965)==self.port,"Request URL must match the configured server_name and remote port; open another client for another endpoint");
        Ok(())
    }
}
