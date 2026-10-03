//! Certificate inputs from startup parameters must return errors rather than panic or
//! silently discard requested identities. These tests generate keys on the CPU only.

use netget::protocol::StartupParams;
use netget::server::tls_cert_manager::{
    extract_tls_config_from_params, generate_self_signed_cert, get_tls_startup_parameters,
    CertificateSpec,
};
use serde_json::json;

#[test]
fn certificate_rejects_non_ascii_dns_names_without_panicking() {
    let spec = CertificateSpec {
        san_dns_names: vec!["éxample.test".to_owned()],
        ..CertificateSpec::default()
    };
    let error = generate_self_signed_cert(&spec)
        .err()
        .expect("invalid SAN accepted");
    assert!(error.to_string().contains("Invalid certificate DNS name"));
}

#[test]
fn certificate_rejects_invalid_and_overflowing_lifetimes() {
    for validity_days in [0, -1, i64::MIN, i64::MAX, i64::MAX / 86_400] {
        let spec = CertificateSpec {
            validity_days,
            ..CertificateSpec::default()
        };
        let error = generate_self_signed_cert(&spec)
            .err()
            .unwrap_or_else(|| panic!("invalid validity_days={validity_days} accepted"));
        assert!(error.to_string().contains("validity_days"));
    }
}

#[test]
fn certificate_accepts_positive_lifetime_and_ascii_wildcard_sans() {
    let spec = CertificateSpec {
        validity_days: 1,
        san_dns_names: vec!["localhost".to_owned(), "*.example.test".to_owned()],
        ..CertificateSpec::default()
    };
    let (certificate, key) = generate_self_signed_cert(&spec).expect("valid certificate");
    assert!(!certificate.der().is_empty());
    assert!(!key.serialize_der().is_empty());
}

#[test]
fn tls_startup_rejects_non_string_sans_instead_of_dropping_them() {
    for invalid in [json!(42), json!(null), json!({"name": "example.test"})] {
        let params = StartupParams::new(
            json!({"tls_enabled": true, "san_dns_names": ["localhost", invalid]}),
            get_tls_startup_parameters(),
        )
        .expect("declared parameters");
        let error = extract_tls_config_from_params(&params)
            .expect_err("invalid SAN array element accepted");
        assert!(error
            .to_string()
            .contains("san_dns_names[1] must be a string"));
    }
}
