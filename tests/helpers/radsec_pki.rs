//! A throwaway PKI for RadSec tests: a CA, a server certificate for localhost/127.0.0.1, a
//! client certificate, and a second CA with its own client — the stranger mutual TLS must refuse.
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    SanType,
};
use std::path::{Path, PathBuf};

pub struct Pki {
    pub ca: PathBuf,
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    pub stranger_cert: PathBuf,
    pub stranger_key: PathBuf,
}

fn ca(name: &str) -> (CertificateParams, KeyPair, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let pem = params.self_signed(&key).unwrap().pem();
    (params, key, pem)
}

fn leaf(
    cn: &str,
    sans: &[&str],
    usage: ExtendedKeyUsagePurpose,
    issuer: &Issuer<'_, KeyPair>,
) -> (String, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, cn);
    for s in sans {
        params.subject_alt_names.push(match s.parse() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName((*s).try_into().unwrap()),
        });
    }
    params.extended_key_usages = vec![usage];
    let key = KeyPair::generate().unwrap();
    let cert = params.signed_by(&key, issuer).unwrap();
    (cert.pem(), key.serialize_pem())
}

pub fn make(dir: &Path) -> Pki {
    let (ca_params, ca_key, ca_pem) = ca("NetGet RadSec test CA");
    let issuer = Issuer::new(ca_params, ca_key);
    let (server_cert, server_key) = leaf(
        "localhost",
        &["localhost", "127.0.0.1"],
        ExtendedKeyUsagePurpose::ServerAuth,
        &issuer,
    );
    let (client_cert, client_key) = leaf(
        "radsec-test-client",
        &["client.test"],
        ExtendedKeyUsagePurpose::ClientAuth,
        &issuer,
    );
    let (other_params, other_key, _) = ca("Some other CA");
    let other = Issuer::new(other_params, other_key);
    let (stranger_cert, stranger_key) = leaf(
        "stranger",
        &["stranger.test"],
        ExtendedKeyUsagePurpose::ClientAuth,
        &other,
    );
    let write = |name: &str, pem: &str| {
        let p = dir.join(name);
        std::fs::write(&p, pem).unwrap();
        p
    };
    Pki {
        ca: write("ca.pem", &ca_pem),
        server_cert: write("server.pem", &server_cert),
        server_key: write("server.key", &server_key),
        client_cert: write("client.pem", &client_cert),
        client_key: write("client.key", &client_key),
        stranger_cert: write("stranger.pem", &stranger_cert),
        stranger_key: write("stranger.key", &stranger_key),
    }
}
