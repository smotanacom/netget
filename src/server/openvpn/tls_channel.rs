//! TLS for the OpenVPN control channel.
//!
//! OpenVPN's control channel is a perfectly ordinary TLS session whose records
//! are carried inside `P_CONTROL_V1` packets instead of over a stream socket.
//! That is why this module uses `rustls::ServerConnection` directly rather than
//! `tokio-rustls`: there is no `AsyncRead`/`AsyncWrite` to wrap. Records are fed
//! in with `read_tls` and taken out with `write_tls`, and the reliability layer
//! in [`super::reliable`] supplies the ordering and retransmission that a stream
//! socket would otherwise have provided.
//!
//! # Certificates and how a client is expected to trust one
//!
//! A fresh self-signed P-256 certificate is generated per server run. There is
//! no shipped key and nothing is written to disk. A client trusts it the way
//! OpenVPN 2.6+ documents for self-signed setups — `--peer-fingerprint` with the
//! SHA-256 digest of the certificate — which the server logs at startup so an
//! operator can paste it into a client config.
//!
//! Client certificates are **not** requested. This server authenticates nobody
//! at the TLS layer; what a peer sends in the key-method-2 exchange is reported
//! to the model, which decides.

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// A control-channel TLS configuration and the fingerprint a client needs to
/// trust it.
pub struct ControlChannelTls {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the DER certificate, uppercase and colon-separated: the exact
    /// spelling OpenVPN's `--peer-fingerprint` expects.
    pub fingerprint: String,
}

/// Build the control-channel TLS configuration.
///
/// The provider is named explicitly rather than taken from the process default.
/// `ServerConfig::builder()` panics when zero or several providers are
/// installed, and this code runs inside a binary that may have linked more than
/// one; the `openvpn` feature pulls in `rustls`'s `ring` provider, so ask for
/// that one by name and the outcome does not depend on what else is compiled in.
pub fn build_control_channel_tls() -> Result<ControlChannelTls> {
    let (cert, key) = generate_self_signed()?;

    let fingerprint = fingerprint_sha256(&cert);

    let provider = rustls::crypto::ring::default_provider();
    let config = ServerConfig::builder_with_provider(Arc::new(provider))
        // OpenVPN 2.6+ defaults to a TLS 1.2 floor and negotiates 1.3 where it
        // can. Offering both means an older client is not refused at the
        // ClientHello, which on this transport looks like a dead server rather
        // than a version mismatch.
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .context("Failed to select TLS protocol versions for the OpenVPN control channel")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("Failed to build the OpenVPN control-channel TLS configuration")?;

    Ok(ControlChannelTls {
        config: Arc::new(config),
        fingerprint,
    })
}

/// A fresh self-signed certificate. Never persisted, never reused across runs.
fn generate_self_signed() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    use rcgen::{CertificateParams, KeyPair};

    let mut params = CertificateParams::new(vec!["netget-openvpn".to_string()])
        .context("Failed to build certificate parameters")?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "NetGet OpenVPN");

    let key_pair = KeyPair::generate().context("Failed to generate a certificate key pair")?;
    let cert = params
        .self_signed(&key_pair)
        .context("Failed to self-sign the control-channel certificate")?;

    Ok((
        CertificateDer::from(cert.der().to_vec()),
        PrivateKeyDer::Pkcs8(key_pair.serialize_der().into()),
    ))
}

/// `--peer-fingerprint` spelling: uppercase hex, colon-separated.
fn fingerprint_sha256(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
}

/// Human-readable name of the negotiated TLS version, for logs and event data.
pub fn protocol_version_name(conn: &rustls::ServerConnection) -> String {
    match conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3".to_string(),
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2".to_string(),
        Some(other) => format!("{:?}", other),
        None => "unknown".to_string(),
    }
}

/// Human-readable name of the negotiated cipher suite.
pub fn cipher_suite_name(conn: &rustls::ServerConnection) -> String {
    conn.negotiated_cipher_suite()
        .map(|s| format!("{:?}", s.suite()))
        .unwrap_or_else(|| "unknown".to_string())
}
