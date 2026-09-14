//! Certificate cache for MITM proxy
//!
//! Generates and caches per-domain leaf certificates signed by a CA certificate.
//! This allows the proxy to present valid-looking certificates for any domain
//! when performing TLS Man-in-the-Middle interception.
//!
//! # Where the CA comes from, and what touches the disk
//!
//! The CA is generated fresh in memory every time a proxy server is started
//! (`CertificateMode::Generate`). There is no fixed or hardcoded key, nothing is
//! read from a well-known path, and neither the CA key, the per-domain leaf keys,
//! nor any intercepted plaintext is ever written to disk by this module. Stopping
//! the server discards the CA, so a client that trusted one run's CA will reject
//! the next run's.
//!
//! The only way a CA certificate reaches the filesystem is when the operator
//! explicitly passes the `ca_export_path` startup parameter, which writes the CA
//! *certificate* (public, safe to distribute) and never the private key.
//!
//! Interception only works for clients that have been told to trust that CA, so
//! this cannot silently intercept an unmodified client: without the trust grant
//! the TLS handshake fails at the client with an unknown-issuer error.

use anyhow::{Context, Result};
use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::collections::HashMap;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use tokio::sync::RwLock;
use tracing::{debug, info, trace};

/// Certificate cache entry.
///
/// Both halves of the identity are stored together: a leaf certificate is only
/// usable with the exact key whose public half it certifies, so caching one
/// without the other (or regenerating one of them on a cache hit) produces a
/// certificate/key pair that rustls rejects.
struct CachedCert {
    /// DER encoding of the leaf certificate
    cert_der: Vec<u8>,
    /// DER encoding of the private key that this certificate certifies
    key_der: Vec<u8>,
    /// When this certificate was generated
    generated_at: crate::utils::clock::Instant,
}

/// Certificate cache for dynamically generated leaf certificates
pub struct CertificateCache {
    /// Root CA certificate (used to sign leaf certificates)
    ca_cert: Arc<Certificate>,
    /// Root CA private key
    ca_key_pair: Arc<KeyPair>,
    /// The parameters the CA certificate was actually built from.
    ///
    /// Leaf certificates must name this exact issuer, so these are the CA's own
    /// params rather than a reconstruction: a reconstructed distinguished name
    /// that differs from the CA's real subject yields a chain no client can
    /// verify.
    ca_params: Arc<CertificateParams>,
    /// Cache of per-domain certificates (domain -> certificate + matching key)
    cache: Arc<RwLock<HashMap<String, CachedCert>>>,
    /// Certificate TTL in seconds (default: 24 hours)
    cert_ttl_secs: u64,
}

impl CertificateCache {
    /// Create a new certificate cache with a CA certificate.
    ///
    /// `ca_params` must be the parameters `ca_cert` was generated from so that
    /// leaf certificates are issued under the CA's real distinguished name.
    pub fn new(ca_cert: Certificate, ca_key_pair: KeyPair, ca_params: CertificateParams) -> Self {
        Self {
            ca_cert: Arc::new(ca_cert),
            ca_key_pair: Arc::new(ca_key_pair),
            ca_params: Arc::new(ca_params),
            cache: Arc::new(RwLock::new(HashMap::new())),
            cert_ttl_secs: 24 * 60 * 60, // 24 hours
        }
    }

    /// Get or generate a certificate for a specific domain.
    ///
    /// Returns a rustls-ready certificate chain and the private key that matches
    /// it. On a cache hit the stored certificate and its own key are returned
    /// together; nothing is regenerated, because a fresh certificate would
    /// certify a fresh key and no longer match the cached one.
    pub async fn get_or_generate(
        &self,
        domain: &str,
    ) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        // Normalize domain (lowercase, trim)
        let domain_normalized = domain.trim().to_lowercase();

        // Check cache first
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(&domain_normalized) {
                let age = cached.generated_at.elapsed().as_secs();
                if age < self.cert_ttl_secs {
                    trace!(
                        "Certificate cache HIT for domain '{}' (age: {}s)",
                        domain_normalized,
                        age
                    );
                    return Self::to_rustls(&cached.cert_der, &cached.key_der);
                } else {
                    debug!(
                        "Certificate cache EXPIRED for domain '{}' (age: {}s > {}s)",
                        domain_normalized, age, self.cert_ttl_secs
                    );
                }
            } else {
                debug!("Certificate cache MISS for domain '{}'", domain_normalized);
            }
        }

        // Generate new certificate
        info!(
            "Generating new leaf certificate for domain '{}'",
            domain_normalized
        );
        let (cert, key_pair) = self.generate_leaf_cert(&domain_normalized)?;

        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();

        // Cache the certificate together with the key it certifies
        {
            let mut cache = self.cache.write().await;
            cache.insert(
                domain_normalized.clone(),
                CachedCert {
                    cert_der: cert_der.clone(),
                    key_der: key_der.clone(),
                    generated_at: crate::utils::clock::Instant::now(),
                },
            );
            debug!(
                "Cached certificate for domain '{}' (cache size: {})",
                domain_normalized,
                cache.len()
            );
        }

        Self::to_rustls(&cert_der, &key_der)
    }

    /// Generate a leaf certificate for a specific domain, signed by the CA
    fn generate_leaf_cert(&self, domain: &str) -> Result<(Certificate, KeyPair)> {
        let mut params = CertificateParams::default();

        // Set distinguished name with the domain as CN
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, domain);
        dn.push(DnType::OrganizationName, "NetGet MITM Proxy");
        params.distinguished_name = dn;

        // Add Subject Alternative Names (both the domain and wildcard)
        params.subject_alt_names = vec![SanType::DnsName(
            domain
                .to_string()
                .try_into()
                .context("Invalid domain name")?,
        )];

        // If domain doesn't start with wildcard, also add wildcard version
        if !domain.starts_with("*.") && !domain.starts_with("www.") {
            // Add wildcard for subdomains (e.g., for example.com, add *.example.com)
            let wildcard_domain = format!("*.{}", domain);
            if let Ok(wildcard_san) = wildcard_domain.try_into() {
                params
                    .subject_alt_names
                    .push(SanType::DnsName(wildcard_san));
            }
        }

        // Add www variant if applicable
        if !domain.starts_with("www.") {
            let www_domain = format!("www.{}", domain);
            if let Ok(www_san) = www_domain.try_into() {
                params.subject_alt_names.push(SanType::DnsName(www_san));
            }
        }

        // Set validity period (24 hours to match cache TTL)
        let now = OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + Duration::days(1);

        // Mark as NOT a CA (this is a leaf certificate)
        params.is_ca = rcgen::IsCa::NoCa;

        // Generate key pair for this certificate
        let key_pair =
            KeyPair::generate().context("Failed to generate key pair for leaf certificate")?;

        // Create an Issuer from the CA's own params and key
        let issuer = rcgen::Issuer::new((*self.ca_params).clone(), self.ca_key_pair.as_ref());

        // Sign this certificate with the CA
        let cert = params
            .signed_by(&key_pair, &issuer)
            .context("Failed to sign leaf certificate with CA")?;

        info!(
            "Successfully generated leaf certificate for domain '{}' (valid for 24h, {} SANs)",
            domain,
            params.subject_alt_names.len()
        );
        trace!("Leaf certificate SANs: {:?}", params.subject_alt_names);

        Ok((cert, key_pair))
    }

    /// Get the CA certificate (for exporting to users)
    pub fn get_ca_cert(&self) -> &Certificate {
        &self.ca_cert
    }

    /// PEM encoding of the CA certificate.
    ///
    /// This is the public certificate only. The CA private key is never exposed
    /// through this API and never written anywhere.
    pub fn ca_cert_pem(&self) -> String {
        self.ca_cert.pem()
    }

    /// Clear expired certificates from the cache
    pub async fn cleanup_expired(&self) {
        let mut cache = self.cache.write().await;
        let initial_size = cache.len();

        cache.retain(|domain, cached| {
            let age = cached.generated_at.elapsed().as_secs();
            if age >= self.cert_ttl_secs {
                debug!(
                    "Removing expired certificate for domain '{}' (age: {}s)",
                    domain, age
                );
                false
            } else {
                true
            }
        });

        let removed = initial_size - cache.len();
        if removed > 0 {
            info!(
                "Cleaned up {} expired certificates from cache (remaining: {})",
                removed,
                cache.len()
            );
        }
    }

    /// Get cache statistics
    pub async fn get_stats(&self) -> CacheStats {
        let cache = self.cache.read().await;
        let total_certs = cache.len();

        let mut expired_count = 0;
        for cached in cache.values() {
            let age = cached.generated_at.elapsed().as_secs();
            if age >= self.cert_ttl_secs {
                expired_count += 1;
            }
        }

        CacheStats {
            total_certificates: total_certs,
            expired_certificates: expired_count,
            valid_certificates: total_certs - expired_count,
        }
    }

    /// Convert cached DER bytes into the owned types rustls wants
    fn to_rustls(
        cert_der: &[u8],
        key_der: &[u8],
    ) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let cert = CertificateDer::from(cert_der.to_vec());
        let key = PrivateKeyDer::try_from(key_der.to_vec())
            .map_err(|e| anyhow::anyhow!("Failed to parse private key DER: {}", e))?;
        Ok((vec![cert], key))
    }
}

/// Certificate cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total_certificates: usize,
    pub expired_certificates: usize,
    pub valid_certificates: usize,
}
