//! TLS for RadSec (RFC 6614), shared by the server and the client: PEM files from startup
//! parameters, mutual authentication when a CA is given, and the RFC's default shared secret.
use anyhow::{bail, ensure, Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::{fs::File, io::BufReader, sync::Arc, time::Duration};

/// RFC 6614 §2.3: over TLS the shared secret is no longer a secret, and is "radsec".
pub const DEFAULT_SECRET: &str = "radsec";
/// The TLS handshake, from connect or accept to the first application byte.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let file = File::open(path).with_context(|| format!("cannot open certificate file {path}"))?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("{path} is not a PEM certificate file"))?;
    ensure!(!certs.is_empty(), "{path} holds no certificate");
    Ok(certs)
}

pub fn key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let file = File::open(path).with_context(|| format!("cannot open private key file {path}"))?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .with_context(|| format!("{path} is not a PEM private key file"))?
        .with_context(|| format!("{path} holds no private key"))
}

fn roots(ca_file: &str) -> Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs(ca_file)? {
        roots
            .add(cert)
            .with_context(|| format!("{ca_file}: a certificate rustls cannot use as a root"))?;
    }
    Ok(roots)
}

/// The server's TLS: its certificate and key (a fresh self-signed one when neither is given),
/// and, with `ca_file`, a client certificate that chains to it is **required**.
pub fn server_config(
    certificate_file: Option<&str>,
    private_key_file: Option<&str>,
    ca_file: Option<&str>,
) -> Result<Arc<rustls::ServerConfig>> {
    provider();
    let (chain, key) = match (certificate_file, private_key_file) {
        (Some(c), Some(k)) => (certs(c)?, key(k)?),
        (None, None) => {
            let spec = crate::server::tls_cert_manager::CertificateSpec {
                common_name: "netget-radsec".into(),
                san_dns_names: vec!["localhost".into()],
                organizational_unit: Some("RadSec".into()),
                ..Default::default()
            };
            let (cert, pair) = crate::server::tls_cert_manager::generate_self_signed_cert(&spec)?;
            let key = PrivateKeyDer::try_from(pair.serialize_der())
                .map_err(|e| anyhow::anyhow!("generated key: {e}"))?;
            (vec![CertificateDer::from(cert.der().to_vec())], key)
        }
        _ => bail!("give certificate_file and private_key_file together, or neither"),
    };
    let builder = rustls::ServerConfig::builder();
    let builder = match ca_file {
        Some(ca) => {
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots(ca)?))
                .build()
                .context("cannot build the client certificate verifier")?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(
        builder
            .with_single_cert(chain, key)
            .context("the certificate and private key do not match")?,
    ))
}

/// The client's TLS: the server must chain to `ca_file` and match `server_name`; a client
/// certificate is presented when one is given.
pub fn client_config(
    ca_file: &str,
    certificate_file: Option<&str>,
    private_key_file: Option<&str>,
) -> Result<Arc<rustls::ClientConfig>> {
    provider();
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots(ca_file)?);
    let config = match (certificate_file, private_key_file) {
        (Some(c), Some(k)) => builder
            .with_client_auth_cert(certs(c)?, key(k)?)
            .context("the client certificate and private key do not match")?,
        (None, None) => builder.with_no_client_auth(),
        _ => bail!("give certificate_file and private_key_file together, or neither"),
    };
    Ok(Arc::new(config))
}

pub fn server_name(name: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(name.to_string())
        .with_context(|| format!("{name:?} is not a valid server name"))
}
