//! HTTP Signatures as the fediverse uses them (draft-cavage-http-signatures-12,
//! `rsa-sha256`, the form Mastodon and every Fedify fallback speak): signing outgoing
//! requests, verifying incoming ones, and the `Digest` and `Date` headers they cover.
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey, LineEnding};
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A request whose `Date` is further than this from now is refused (Mastodon allows 12 h).
pub const MAX_CLOCK_SKEW_SECS: i64 = 12 * 3600;
/// A `Signature` header is at most this long.
pub const MAX_SIGNATURE_HEADER: usize = 4096;
pub const KEY_BITS: usize = 2048;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

pub fn generate_key() -> Result<RsaPrivateKey> {
    Ok(RsaPrivateKey::new(&mut rand::rngs::OsRng, KEY_BITS)?)
}

pub fn public_pem(key: &RsaPrivateKey) -> Result<String> {
    Ok(RsaPublicKey::from(key).to_public_key_pem(LineEnding::LF)?)
}

/// A PEM public key: SPKI (`BEGIN PUBLIC KEY`, what everyone publishes) or PKCS#1.
pub fn parse_public_pem(pem: &str) -> Result<RsaPublicKey> {
    RsaPublicKey::from_public_key_pem(pem.trim())
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem.trim()))
        .context("publicKeyPem is not an RSA public key")
}

/// `SHA-256=<base64>` of a body.
pub fn digest(body: &[u8]) -> String {
    format!("SHA-256={}", b64().encode(Sha256::digest(body)))
}

pub fn http_date() -> String {
    httpdate_now()
}

fn httpdate_now() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// The signing string over `headers` (lower-case names, `(request-target)` included).
fn signing_string(
    headers: &[&str],
    method: &str,
    path: &str,
    values: &HashMap<String, String>,
) -> Result<String> {
    let mut lines = Vec::new();
    for h in headers {
        if *h == "(request-target)" {
            lines.push(format!(
                "(request-target): {} {}",
                method.to_lowercase(),
                path
            ));
        } else {
            let v = values
                .get(*h)
                .with_context(|| format!("signed header {h} is missing from the request"))?;
            lines.push(format!("{h}: {v}"));
        }
    }
    Ok(lines.join("\n"))
}

/// The `Signature` header for a request with these header values (lower-case names).
pub fn sign(
    key: &RsaPrivateKey,
    key_id: &str,
    method: &str,
    path: &str,
    values: &HashMap<String, String>,
) -> Result<String> {
    let headers: Vec<&str> = if values.contains_key("digest") {
        vec!["(request-target)", "host", "date", "digest"]
    } else {
        vec!["(request-target)", "host", "date"]
    };
    let text = signing_string(&headers, method, path, values)?;
    let sig = SigningKey::<Sha256>::new(key.clone()).sign(text.as_bytes());
    Ok(format!(
        "keyId=\"{key_id}\",algorithm=\"rsa-sha256\",headers=\"{}\",signature=\"{}\"",
        headers.join(" "),
        b64().encode(sig.to_bytes())
    ))
}

/// A parsed `Signature` header.
pub struct Parsed {
    pub key_id: String,
    pub headers: Vec<String>,
    pub signature: Vec<u8>,
}

pub fn parse(header: &str) -> Result<Parsed> {
    ensure!(
        header.len() <= MAX_SIGNATURE_HEADER,
        "Signature header too long"
    );
    let mut fields = HashMap::new();
    for part in header.split(',') {
        let (k, v) = part
            .trim()
            .split_once('=')
            .context("malformed Signature header")?;
        fields.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
    }
    let algorithm = fields
        .get("algorithm")
        .map(String::as_str)
        .unwrap_or("hs2019");
    ensure!(
        matches!(algorithm, "rsa-sha256" | "hs2019"),
        "signature algorithm {algorithm} is not supported (rsa-sha256 only)"
    );
    Ok(Parsed {
        key_id: fields
            .get("keyId")
            .cloned()
            .context("Signature has no keyId")?,
        headers: fields
            .get("headers")
            .map(|h| h.split_whitespace().map(str::to_lowercase).collect())
            .unwrap_or_else(|| vec!["date".into()]),
        signature: b64()
            .decode(
                fields
                    .get("signature")
                    .context("Signature has no signature")?,
            )
            .context("signature is not base64")?,
    })
}

/// Check a request's signature with the signer's key, and that it covers what it must:
/// the request target, `host` and `date`, and the body's `digest` on a POST, which must
/// match the body.
pub fn verify(
    parsed: &Parsed,
    key_pem: &str,
    method: &str,
    path: &str,
    values: &HashMap<String, String>,
    body: Option<&[u8]>,
) -> Result<()> {
    for must in ["(request-target)", "host", "date"] {
        ensure!(
            parsed.headers.iter().any(|h| h == must),
            "the signature does not cover {must}"
        );
    }
    if let Some(body) = body {
        ensure!(
            parsed.headers.iter().any(|h| h == "digest"),
            "the signature does not cover the body's digest"
        );
        let claimed = values.get("digest").context("no Digest header")?;
        let ours = digest(body);
        ensure!(
            claimed.split(',').any(|d| d.trim() == ours),
            "the Digest header does not match the body"
        );
    }
    let date = values.get("date").context("no Date header")?;
    let when = chrono::DateTime::parse_from_rfc2822(date).context("Date is not an HTTP date")?;
    let skew = (chrono::Utc::now().timestamp() - when.timestamp()).abs();
    ensure!(skew <= MAX_CLOCK_SKEW_SECS, "Date is {skew} s from now");
    let headers: Vec<&str> = parsed.headers.iter().map(String::as_str).collect();
    let text = signing_string(&headers, method, path, values)?;
    let key = parse_public_pem(key_pem)?;
    let sig = Signature::try_from(parsed.signature.as_slice())
        .context("signature has the wrong length")?;
    if VerifyingKey::<Sha256>::new(key)
        .verify(text.as_bytes(), &sig)
        .is_err()
    {
        bail!("the signature does not verify against {}", parsed.key_id);
    }
    Ok(())
}
