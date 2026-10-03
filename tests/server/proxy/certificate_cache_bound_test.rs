//! Unique CONNECT destinations must not retain unbounded certificate/key pairs.

use netget::server::proxy::cert_cache::{CertificateCache, MAX_CACHED_CERTIFICATES};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

#[tokio::test]
async fn certificate_cache_evicts_the_oldest_pair_at_its_capacity() {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().expect("CA key");
    let ca = params.self_signed(&key).expect("CA certificate");
    let cache = CertificateCache::new(ca, key, params);
    let (first, _) = cache
        .get_or_generate("first.example.test")
        .await
        .expect("first certificate");
    for index in 1..MAX_CACHED_CERTIFICATES {
        cache
            .get_or_generate(&format!("host-{index}.example.test"))
            .await
            .expect("fill cache");
    }
    let (still_first, _) = cache
        .get_or_generate("first.example.test")
        .await
        .expect("cache hit");
    assert_eq!(
        first, still_first,
        "cache hit before capacity must preserve the identity"
    );
    let (newest, newest_key) = cache
        .get_or_generate("newest.example.test")
        .await
        .expect("overflow certificate");
    assert_eq!(
        cache.get_stats().await.total_certificates,
        MAX_CACHED_CERTIFICATES
    );
    let (regenerated, _) = cache
        .get_or_generate("first.example.test")
        .await
        .expect("regenerate evicted certificate");
    assert_ne!(
        first, regenerated,
        "the oldest certificate should have been evicted"
    );
    let (cached_newest, cached_key) = cache
        .get_or_generate("newest.example.test")
        .await
        .expect("recent certificate retained");
    assert_eq!(newest, cached_newest);
    assert_eq!(newest_key.secret_der(), cached_key.secret_der());
    assert_eq!(
        cache.get_stats().await.total_certificates,
        MAX_CACHED_CERTIFICATES
    );
}
