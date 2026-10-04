//! RFC 7515 flattened JWS as ACME uses it (RFC 8555 §6.2): parsing, verification for ES256,
//! ES384, RS256 and EdDSA, RFC 7638 JWK thumbprints, and ES256 signing for the client.
use anyhow::{bail, ensure, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::signature::{self, KeyPair};
use serde_json::{json, Map, Value};

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    ensure!(
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "not base64url without padding"
    );
    Ok(URL_SAFE_NO_PAD.decode(s)?)
}

pub fn sha256(bytes: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .to_vec()
}

/// One verified-shape ACME request body, before the signature is checked.
#[derive(Debug, Clone)]
pub struct Jws {
    pub alg: String,
    pub nonce: String,
    pub url: String,
    /// Exactly one of `jwk` and `kid` is present.
    pub jwk: Option<Map<String, Value>>,
    pub kid: Option<String>,
    /// The decoded payload; empty for POST-as-GET.
    pub payload: Vec<u8>,
    signing_input: Vec<u8>,
    signature: Vec<u8>,
}

impl Jws {
    pub fn parse(body: &[u8]) -> Result<Self> {
        let v: Value = serde_json::from_slice(body).context("body is not JSON")?;
        let o = v.as_object().context("JWS is a JSON object")?;
        ensure!(
            !o.contains_key("signatures") && !o.contains_key("header"),
            "only the flattened JWS serialization with a protected header is accepted"
        );
        let field = |k: &str| {
            o.get(k)
                .and_then(Value::as_str)
                .with_context(|| format!("JWS {k} missing"))
        };
        let (protected_b64, payload_b64, sig_b64) =
            (field("protected")?, field("payload")?, field("signature")?);
        let protected: Value = serde_json::from_slice(&unb64(protected_b64)?)
            .context("protected header is not JSON")?;
        let p = protected
            .as_object()
            .context("protected header is an object")?;
        let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_owned);
        let alg = s("alg").context("protected header has no alg")?;
        let nonce = s("nonce").context("protected header has no nonce")?;
        let url = s("url").context("protected header has no url")?;
        let jwk = p
            .get("jwk")
            .map(|j| j.as_object().cloned().context("jwk is an object"))
            .transpose()?;
        let kid = s("kid");
        ensure!(jwk.is_some() != kid.is_some(), "exactly one of jwk and kid");
        Ok(Self {
            alg,
            nonce,
            url,
            jwk,
            kid,
            payload: unb64(payload_b64)?,
            signing_input: format!("{protected_b64}.{payload_b64}").into_bytes(),
            signature: unb64(sig_b64)?,
        })
    }

    pub fn payload_json(&self) -> Result<Value> {
        ensure!(!self.payload.is_empty(), "payload is empty");
        serde_json::from_slice(&self.payload).context("payload is not JSON")
    }

    /// Verify against `jwk` (the request's own, or the account's for a `kid` request).
    pub fn verify(&self, jwk: &Map<String, Value>) -> Result<()> {
        let key = PublicKey::from_jwk(jwk)?;
        ensure!(
            key.alg_ok(&self.alg),
            "alg {} does not match the {} key",
            self.alg,
            key.kind()
        );
        key.verify(&self.alg, &self.signing_input, &self.signature)
    }
}

/// The public key of a JWK, validated.
pub enum PublicKey {
    Ec { crv: &'static str, point: Vec<u8> },
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Ed25519(Vec<u8>),
}

impl PublicKey {
    pub fn from_jwk(jwk: &Map<String, Value>) -> Result<Self> {
        let s = |k: &str| {
            jwk.get(k)
                .and_then(Value::as_str)
                .with_context(|| format!("jwk {k} missing"))
        };
        match s("kty")? {
            "EC" => {
                let (crv, len) = match s("crv")? {
                    "P-256" => ("P-256", 32),
                    "P-384" => ("P-384", 48),
                    other => bail!("unsupported EC curve {other}"),
                };
                let (x, y) = (unb64(s("x")?)?, unb64(s("y")?)?);
                ensure!(
                    x.len() == len && y.len() == len,
                    "EC coordinates have the wrong length"
                );
                let mut point = vec![4u8];
                point.extend(x);
                point.extend(y);
                Ok(Self::Ec { crv, point })
            }
            "RSA" => {
                let (n, e) = (unb64(s("n")?)?, unb64(s("e")?)?);
                ensure!(
                    (256..=1024).contains(&n.len()) && !e.is_empty() && e.len() <= 8,
                    "RSA key must be 2048 to 8192 bits"
                );
                Ok(Self::Rsa { n, e })
            }
            "OKP" => {
                ensure!(s("crv")? == "Ed25519", "only Ed25519 OKP keys");
                let x = unb64(s("x")?)?;
                ensure!(x.len() == 32, "Ed25519 key is 32 bytes");
                Ok(Self::Ed25519(x))
            }
            other => bail!("unsupported key type {other}"),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ec { crv, .. } => crv,
            Self::Rsa { .. } => "RSA",
            Self::Ed25519(_) => "Ed25519",
        }
    }

    fn alg_ok(&self, alg: &str) -> bool {
        matches!(
            (self, alg),
            (Self::Ec { crv: "P-256", .. }, "ES256")
                | (Self::Ec { crv: "P-384", .. }, "ES384")
                | (Self::Rsa { .. }, "RS256")
                | (Self::Ed25519(_), "EdDSA")
        )
    }

    fn verify(&self, alg: &str, msg: &[u8], sig: &[u8]) -> Result<()> {
        let ok = match (self, alg) {
            (Self::Ec { point, .. }, "ES256") => {
                signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point)
                    .verify(msg, sig)
                    .is_ok()
            }
            (Self::Ec { point, .. }, "ES384") => {
                signature::UnparsedPublicKey::new(&signature::ECDSA_P384_SHA384_FIXED, point)
                    .verify(msg, sig)
                    .is_ok()
            }
            (Self::Rsa { n, e }, "RS256") => signature::RsaPublicKeyComponents { n, e }
                .verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig)
                .is_ok(),
            (Self::Ed25519(x), "EdDSA") => {
                signature::UnparsedPublicKey::new(&signature::ED25519, x)
                    .verify(msg, sig)
                    .is_ok()
            }
            _ => false,
        };
        ensure!(ok, "JWS signature does not verify");
        Ok(())
    }
}

/// RFC 7638 thumbprint: SHA-256 over the required members in lexicographic order.
pub fn thumbprint(jwk: &Map<String, Value>) -> Result<String> {
    let s = |k: &str| {
        jwk.get(k)
            .and_then(Value::as_str)
            .with_context(|| format!("jwk {k} missing"))
    };
    let canonical = match s("kty")? {
        "EC" => json!({"crv": s("crv")?, "kty": "EC", "x": s("x")?, "y": s("y")?}),
        "RSA" => json!({"e": s("e")?, "kty": "RSA", "n": s("n")?}),
        "OKP" => json!({"crv": s("crv")?, "kty": "OKP", "x": s("x")?}),
        other => bail!("unsupported key type {other}"),
    };
    // Members are written in lexicographic order and serialised without whitespace.
    Ok(b64(&sha256(canonical.to_string().as_bytes())))
}

/// An ES256 account key for the client.
pub struct AccountKey {
    pair: signature::EcdsaKeyPair,
    rng: ring::rand::SystemRandom,
}

impl AccountKey {
    pub fn generate() -> Result<Self> {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = signature::EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .map_err(|_| anyhow::anyhow!("key generation failed"))?;
        let pair = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8.as_ref(),
            &rng,
        )
        .map_err(|_| anyhow::anyhow!("key generation failed"))?;
        Ok(Self { pair, rng })
    }

    pub fn jwk(&self) -> Map<String, Value> {
        let p = self.pair.public_key().as_ref();
        json!({"crv": "P-256", "kty": "EC", "x": b64(&p[1..33]), "y": b64(&p[33..65])})
            .as_object()
            .cloned()
            .unwrap_or_default()
    }

    /// A flattened JWS for `url`; `kid` selects kid over the embedded jwk; `None` payload is
    /// POST-as-GET.
    pub fn sign(
        &self,
        url: &str,
        nonce: &str,
        kid: Option<&str>,
        payload: Option<&Value>,
    ) -> Result<Value> {
        let mut protected = json!({"alg": "ES256", "nonce": nonce, "url": url});
        match kid {
            Some(k) => protected["kid"] = json!(k),
            None => protected["jwk"] = Value::Object(self.jwk()),
        }
        let protected_b64 = b64(protected.to_string().as_bytes());
        let payload_b64 = payload
            .map(|p| b64(p.to_string().as_bytes()))
            .unwrap_or_default();
        let sig = self
            .pair
            .sign(
                &self.rng,
                format!("{protected_b64}.{payload_b64}").as_bytes(),
            )
            .map_err(|_| anyhow::anyhow!("signing failed"))?;
        Ok(
            json!({"protected": protected_b64, "payload": payload_b64, "signature": b64(sig.as_ref())}),
        )
    }
}
