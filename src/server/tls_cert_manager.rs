//! TLS certificate management for DoT and DoH servers
//!
//! Provides LLM-controlled certificate generation with self-signed fallback.

use anyhow::{Context, Result};
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tracing::{debug, info};

/// Certificate specification from LLM or defaults
#[derive(Debug, Clone)]
pub struct CertificateSpec {
    /// Common name (CN) for the certificate
    pub common_name: String,
    /// Subject Alternative Names (DNS names)
    pub san_dns_names: Vec<String>,
    /// Certificate validity period in days
    pub validity_days: i64,
    /// Organization name
    pub organization: Option<String>,
    /// Organizational unit
    pub organizational_unit: Option<String>,
}

impl Default for CertificateSpec {
    fn default() -> Self {
        Self {
            common_name: "netget-dns-server".to_string(),
            san_dns_names: vec!["localhost".to_string(), "*.local".to_string()],
            validity_days: 365,
            organization: Some("NetGet".to_string()),
            organizational_unit: Some("DNS Server".to_string()),
        }
    }
}

impl CertificateSpec {
    /// Create a new certificate spec from LLM parameters
    pub fn from_llm_params(
        common_name: Option<String>,
        san_dns_names: Option<Vec<String>>,
        validity_days: Option<i64>,
        organization: Option<String>,
        organizational_unit: Option<String>,
    ) -> Self {
        let defaults = Self::default();
        Self {
            common_name: common_name.unwrap_or(defaults.common_name),
            san_dns_names: san_dns_names.unwrap_or(defaults.san_dns_names),
            validity_days: validity_days.unwrap_or(defaults.validity_days),
            organization: organization.or(defaults.organization),
            organizational_unit: organizational_unit.or(defaults.organizational_unit),
        }
    }
}

/// Generate a self-signed TLS certificate based on spec
/// Returns both the Certificate and its KeyPair
pub fn generate_self_signed_cert(spec: &CertificateSpec) -> Result<(Certificate, KeyPair)> {
    info!("Generating self-signed TLS certificate");
    debug!("Certificate spec: {:?}", spec);

    let mut params = CertificateParams::default();

    // Set distinguished name
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, &spec.common_name);

    if let Some(ref org) = spec.organization {
        dn.push(DnType::OrganizationName, org);
    }

    if let Some(ref ou) = spec.organizational_unit {
        dn.push(DnType::OrganizationalUnitName, ou);
    }

    params.distinguished_name = dn;

    // Add Subject Alternative Names
    params.subject_alt_names = spec
        .san_dns_names
        .iter()
        .map(|name| SanType::DnsName(name.to_string().try_into().unwrap()))
        .collect();

    // Set validity period
    let now = OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + Duration::days(spec.validity_days);

    // Generate key pair and self-sign
    let key_pair = KeyPair::generate().context("Failed to generate key pair")?;

    let cert = params
        .self_signed(&key_pair)
        .context("Failed to create self-signed certificate")?;

    info!(
        "Successfully generated self-signed certificate for CN={}",
        spec.common_name
    );

    Ok((cert, key_pair))
}

/// Create a rustls ServerConfig from an rcgen Certificate and KeyPair
pub fn create_rustls_server_config(
    cert: &Certificate,
    key_pair: &KeyPair,
) -> Result<Arc<ServerConfig>> {
    // Get the certificate DER
    let cert_der = cert.der();
    let cert_der_owned = CertificateDer::from(cert_der.to_vec());

    // Get the private key DER from the KeyPair
    let key_der_vec = key_pair.serialize_der();
    let key_der_owned = PrivateKeyDer::try_from(key_der_vec)
        .map_err(|e| anyhow::anyhow!("Failed to parse private key DER: {}", e))?;

    // Install the default crypto provider (ring) for rustls if not already installed
    // This is required for ServerConfig::builder() to work
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build rustls ServerConfig
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der_owned], key_der_owned)
        .context("Failed to create rustls ServerConfig")?;

    Ok(Arc::new(config))
}

/// Generate a default self-signed certificate and create rustls ServerConfig
pub fn generate_default_tls_config() -> Result<Arc<ServerConfig>> {
    let spec = CertificateSpec::default();
    let (cert, key_pair) = generate_self_signed_cert(&spec)?;
    create_rustls_server_config(&cert, &key_pair)
}

/// Like [`generate_default_tls_config`], but advertising `alpn` in the TLS handshake.
///
/// Deliberately a separate function rather than a change to the default: `dot`, `tls` and
/// `quic` share that config and document themselves as ALPN-less, and advertising a protocol
/// they do not speak would break clients that select on it.
///
/// A server that negotiates nothing forces every client to assume the protocol out of band.
/// That is why `doh` needs this: RFC 8484 runs over HTTP/2, and an HTTP/2 client that is not
/// told `h2` during the handshake will either fall back to HTTP/1.1 — which this server does
/// not speak — or refuse the connection outright.
pub fn generate_default_tls_config_with_alpn(alpn: &[&str]) -> Result<Arc<ServerConfig>> {
    let config = generate_default_tls_config()?;
    // Freshly built above, so this is the only reference and the unwrap cannot fire.
    let mut config = Arc::try_unwrap(config)
        .map_err(|_| anyhow::anyhow!("TLS config was unexpectedly shared before ALPN was set"))?;
    config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
    Ok(Arc::new(config))
}

/// Generate a custom TLS configuration from LLM-specified parameters
pub fn generate_custom_tls_config(
    common_name: Option<String>,
    san_dns_names: Option<Vec<String>>,
    validity_days: Option<i64>,
    organization: Option<String>,
    organizational_unit: Option<String>,
) -> Result<Arc<ServerConfig>> {
    let spec = CertificateSpec::from_llm_params(
        common_name,
        san_dns_names,
        validity_days,
        organization,
        organizational_unit,
    );

    let (cert, key_pair) = generate_self_signed_cert(&spec)?;
    create_rustls_server_config(&cert, &key_pair)
}

/// Load TLS configuration from certificate and key files
pub fn load_tls_config_from_files(cert_path: &str, key_path: &str) -> Result<Arc<ServerConfig>> {
    info!("Loading TLS certificate from files");
    debug!("Certificate path: {}", cert_path);
    debug!("Key path: {}", key_path);

    // Read certificate file
    let cert_pem = std::fs::read_to_string(cert_path)
        .with_context(|| format!("Failed to read certificate file: {}", cert_path))?;

    // Read key file
    let key_pem = std::fs::read_to_string(key_path)
        .with_context(|| format!("Failed to read private key file: {}", key_path))?;

    // Parse certificate
    let cert_der = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse certificate PEM")?;

    if cert_der.is_empty() {
        return Err(anyhow::anyhow!("No certificates found in certificate file"));
    }

    // Parse private key
    let mut key_reader = key_pem.as_bytes();
    let key_der = rustls_pemfile::private_key(&mut key_reader)
        .context("Failed to parse private key PEM")?
        .ok_or_else(|| anyhow::anyhow!("No private key found in key file"))?;

    // Build rustls ServerConfig
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_der, key_der)
        .context("Failed to create rustls ServerConfig from loaded certificate")?;

    info!("Successfully loaded TLS certificate from files");

    Ok(Arc::new(config))
}

/// Create TLS configuration from startup parameters
///
/// If cert_path and key_path are provided, loads certificate from files.
/// Otherwise, generates a self-signed certificate with optional customization.
pub fn create_tls_config(
    cert_path: Option<&str>,
    key_path: Option<&str>,
    common_name: Option<String>,
    san_dns_names: Option<Vec<String>>,
    validity_days: Option<i64>,
    organization: Option<String>,
    organizational_unit: Option<String>,
) -> Result<Arc<ServerConfig>> {
    match (cert_path, key_path) {
        (Some(cert), Some(key)) => {
            // Load from files
            load_tls_config_from_files(cert, key)
        }
        (Some(_), None) | (None, Some(_)) => Err(anyhow::anyhow!(
            "Both cert_path and key_path must be provided together"
        )),
        (None, None) => {
            // Generate self-signed certificate
            if common_name.is_some()
                || san_dns_names.is_some()
                || validity_days.is_some()
                || organization.is_some()
                || organizational_unit.is_some()
            {
                // Use custom parameters
                generate_custom_tls_config(
                    common_name,
                    san_dns_names,
                    validity_days,
                    organization,
                    organizational_unit,
                )
            } else {
                // Use defaults
                generate_default_tls_config()
            }
        }
    }
}

/// Get TLS startup parameters for protocols
pub fn get_tls_startup_parameters() -> Vec<crate::llm::actions::ParameterDefinition> {
    use crate::llm::actions::ParameterDefinition;

    vec![
        ParameterDefinition {
            name: "tls_enabled".to_string(),
            type_hint: "boolean".to_string(),
            description: "Enable TLS/SSL encryption (default: false for HTTP/HTTP2, always true for QUIC)".to_string(),
            required: false,
            example: serde_json::json!(true),
        },
        ParameterDefinition {
            name: "cert_path".to_string(),
            type_hint: "string".to_string(),
            description: "Path to TLS certificate file (PEM format). If not provided, a self-signed certificate will be generated.".to_string(),
            required: false,
            example: serde_json::json!("/path/to/cert.pem"),
        },
        ParameterDefinition {
            name: "key_path".to_string(),
            type_hint: "string".to_string(),
            description: "Path to TLS private key file (PEM format). Required if cert_path is provided.".to_string(),
            required: false,
            example: serde_json::json!("/path/to/key.pem"),
        },
        ParameterDefinition {
            name: "common_name".to_string(),
            type_hint: "string".to_string(),
            description: "Common Name (CN) for self-signed certificate (default: netget-dns-server)".to_string(),
            required: false,
            example: serde_json::json!("example.com"),
        },
        ParameterDefinition {
            name: "san_dns_names".to_string(),
            type_hint: "array".to_string(),
            description: "Subject Alternative Names (DNS names) for self-signed certificate (default: [\"localhost\", \"*.local\"])".to_string(),
            required: false,
            example: serde_json::json!(["example.com", "*.example.com"]),
        },
        ParameterDefinition {
            name: "validity_days".to_string(),
            type_hint: "number".to_string(),
            description: "Certificate validity period in days for self-signed certificate (default: 365)".to_string(),
            required: false,
            example: serde_json::json!(365),
        },
        ParameterDefinition {
            name: "organization".to_string(),
            type_hint: "string".to_string(),
            description: "Organization name for self-signed certificate (default: NetGet)".to_string(),
            required: false,
            example: serde_json::json!("My Organization"),
        },
        ParameterDefinition {
            name: "organizational_unit".to_string(),
            type_hint: "string".to_string(),
            description: "Organizational unit for self-signed certificate".to_string(),
            required: false,
            example: serde_json::json!("IT Department"),
        },
    ]
}

/// Extract TLS configuration from startup parameters
pub fn extract_tls_config_from_params(
    params: &crate::protocol::StartupParams,
) -> Result<Option<Arc<ServerConfig>>> {
    // Check if TLS is enabled
    let tls_enabled = params.get_optional_bool("tls_enabled")?.unwrap_or(false);

    if !tls_enabled {
        return Ok(None);
    }

    // Extract TLS parameters
    let cert_path = params.get_optional_string("cert_path")?;
    let key_path = params.get_optional_string("key_path")?;
    let common_name = params.get_optional_string("common_name")?;
    let san_dns_names = params.get_optional_array("san_dns_names")?.map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    });
    let validity_days = params.get_optional_i64("validity_days")?;
    let organization = params.get_optional_string("organization")?;
    let organizational_unit = params.get_optional_string("organizational_unit")?;

    // Create TLS config
    create_tls_config(
        cert_path.as_deref(),
        key_path.as_deref(),
        common_name,
        san_dns_names,
        validity_days,
        organization,
        organizational_unit,
    )
    .map(Some)
}
