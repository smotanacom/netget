//! The test CA behind the ACME server: a P-256 root generated at startup, CSR checks and
//! issuance with rcgen.
use anyhow::{ensure, Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use ring::rand::SecureRandom;

pub struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

/// What a CSR asks for.
pub struct CsrSummary {
    pub dns_names: Vec<String>,
    pub common_name: Option<String>,
    pub other_names: usize,
    params: CertificateSigningRequestParams,
}

/// An issued certificate.
pub struct Issued {
    pub der: Vec<u8>,
    pub pem: String,
    pub serial: String,
    pub not_after: String,
}

impl Ca {
    pub fn new(name: &str) -> Result<Self> {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name.push(DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::hours(1);
        params.not_after = now + time::Duration::days(3650);
        Ok(Self {
            issuer: CertifiedIssuer::self_signed(params, key)?,
        })
    }

    pub fn pem(&self) -> String {
        self.issuer.pem()
    }

    /// Parse a DER CSR and verify its self-signature.
    pub fn read_csr(der: &[u8]) -> Result<CsrSummary> {
        ensure!(der.len() <= 16 * 1024, "CSR over 16 KiB");
        let params = CertificateSigningRequestParams::from_der(&der.to_vec().into())
            .ok()
            .context("CSR does not parse or its signature does not verify")?;
        let mut dns_names = Vec::new();
        let mut other_names = 0;
        for san in &params.params.subject_alt_names {
            match san {
                SanType::DnsName(n) => dns_names.push(n.as_str().to_ascii_lowercase()),
                _ => other_names += 1,
            }
        }
        let common_name = params
            .params
            .distinguished_name
            .get(&DnType::CommonName)
            .map(dn_text);
        Ok(CsrSummary {
            dns_names,
            common_name,
            other_names,
            params,
        })
    }

    pub fn issue(&self, csr: CsrSummary, names: &[String], validity_days: u32) -> Result<Issued> {
        let mut params = CertificateParams::new(names.to_vec())?;
        params
            .distinguished_name
            .push(DnType::CommonName, names[0].as_str());
        let mut serial = [0u8; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut serial)
            .ok()
            .context("no randomness")?;
        serial[0] &= 0x7f;
        params.serial_number = Some(SerialNumber::from_slice(&serial));
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::minutes(1);
        params.not_after = now + time::Duration::days(validity_days as i64);
        // NoCa omits basicConstraints: an explicit cA FALSE is a DER default and is refused
        // by strict parsers (python cryptography, so certbot).
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        params.use_authority_key_identifier_extension = true;
        let not_after = params
            .not_after
            .format(&time::format_description::well_known::Rfc3339)?;
        let request = CertificateSigningRequestParams {
            params,
            public_key: csr.params.public_key,
        };
        let cert = request.signed_by(&self.issuer)?;
        Ok(Issued {
            der: cert.der().to_vec(),
            pem: format!("{}{}", cert.pem(), self.issuer.pem()),
            serial: serial.iter().map(|b| format!("{b:02x}")).collect(),
            not_after,
        })
    }
}

fn dn_text(v: &rcgen::DnValue) -> String {
    match v {
        rcgen::DnValue::Utf8String(s) => s.clone(),
        rcgen::DnValue::PrintableString(s) => s.as_str().to_owned(),
        rcgen::DnValue::Ia5String(s) => s.as_str().to_owned(),
        rcgen::DnValue::TeletexString(s) => s.as_str().to_owned(),
        _ => String::new(),
    }
}
