//! libp2p's Noise channel: `Noise_XX_25519_ChaChaPoly_SHA256` with an empty prologue, each
//! message framed by a 16-bit big-endian length, and the libp2p handshake payload (the
//! identity key and its signature over the Noise static key) in messages 2 and 3.
use super::wire::{self, PbValue};
use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use ring::aead::{self, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use sha2::{Digest, Sha256};
use std::future::Future;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use x25519_dalek::{PublicKey, StaticSecret};

const PROTOCOL_NAME: &[u8; 32] = b"Noise_XX_25519_ChaChaPoly_SHA256";
const SIG_PREFIX: &[u8] = b"noise-libp2p-static-key:";
const TAG: usize = 16;
/// Largest Noise message (its 16-bit length prefix).
pub const MAX_NOISE_MESSAGE: usize = 65535;
/// Largest plaintext one transport message carries.
pub const MAX_PLAINTEXT: usize = MAX_NOISE_MESSAGE - TAG;

/// This node's libp2p identity.
pub struct Identity {
    pub key: SigningKey,
    pub public_key_proto: Vec<u8>,
    pub peer_id: Vec<u8>,
}

impl Identity {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let key = SigningKey::from_bytes(&seed);
        let public_key_proto = wire::ed25519_public_key_proto(&key.verifying_key().to_bytes());
        let peer_id = wire::peer_id(&public_key_proto);
        Self {
            key,
            public_key_proto,
            peer_id,
        }
    }
    pub fn random() -> Self {
        Self::from_seed(rand::random())
    }
    pub fn peer_id_string(&self) -> String {
        wire::peer_id_string(&self.peer_id)
    }
}

struct Cipher {
    key: Option<LessSafeKey>,
    n: u64,
}

impl Cipher {
    fn empty() -> Self {
        Self { key: None, n: 0 }
    }
    fn new(k: &[u8]) -> Self {
        let key = UnboundKey::new(&CHACHA20_POLY1305, &k[..32]).expect("32-byte key");
        Self {
            key: Some(LessSafeKey::new(key)),
            n: 0,
        }
    }
    fn nonce(&mut self) -> Result<Nonce> {
        ensure!(self.n < u64::MAX, "Noise nonce exhausted");
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.n.to_le_bytes());
        self.n += 1;
        Ok(Nonce::assume_unique_for_key(n))
    }
    fn encrypt(&mut self, ad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        if self.key.is_none() {
            return Ok(plaintext.to_vec());
        }
        let nonce = self.nonce()?;
        let mut buf = plaintext.to_vec();
        self.key
            .as_ref()
            .expect("checked above")
            .seal_in_place_append_tag(nonce, aead::Aad::from(ad), &mut buf)
            .map_err(|_| anyhow::anyhow!("Noise encryption failed"))?;
        Ok(buf)
    }
    fn decrypt(&mut self, ad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
        if self.key.is_none() {
            return Ok(ciphertext.to_vec());
        }
        let nonce = self.nonce()?;
        let mut buf = ciphertext.to_vec();
        let n = self
            .key
            .as_ref()
            .expect("checked above")
            .open_in_place(nonce, aead::Aad::from(ad), &mut buf)
            .map_err(|_| {
                anyhow::anyhow!("Noise decryption failed (wrong key or tampered message)")
            })?
            .len();
        buf.truncate(n);
        Ok(buf)
    }
}

struct Symmetric {
    ck: [u8; 32],
    h: [u8; 32],
    cipher: Cipher,
}

fn hkdf2(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut okm = [0u8; 64];
    Hkdf::<Sha256>::new(Some(ck), ikm)
        .expand(&[], &mut okm)
        .expect("64 bytes is a valid HKDF length");
    let (a, b) = okm.split_at(32);
    (a.try_into().unwrap(), b.try_into().unwrap())
}

impl Symmetric {
    fn new() -> Self {
        let h = *PROTOCOL_NAME;
        let mut s = Self {
            ck: h,
            h,
            cipher: Cipher::empty(),
        };
        s.mix_hash(&[]); // the empty prologue
        s
    }
    fn mix_hash(&mut self, data: &[u8]) {
        let mut d = Sha256::new();
        d.update(self.h);
        d.update(data);
        self.h = d.finalize().into();
    }
    fn mix_key(&mut self, ikm: &[u8]) {
        let (ck, k) = hkdf2(&self.ck, ikm);
        self.ck = ck;
        self.cipher = Cipher::new(&k);
    }
    fn encrypt_and_hash(&mut self, p: &[u8]) -> Result<Vec<u8>> {
        let c = self.cipher.encrypt(&self.h, p)?;
        self.mix_hash(&c);
        Ok(c)
    }
    fn decrypt_and_hash(&mut self, c: &[u8]) -> Result<Vec<u8>> {
        let p = self.cipher.decrypt(&self.h, c)?;
        self.mix_hash(c);
        Ok(p)
    }
    fn split(&self) -> (Cipher, Cipher) {
        let (a, b) = hkdf2(&self.ck, &[]);
        (Cipher::new(&a), Cipher::new(&b))
    }
}

fn dh(secret: &StaticSecret, public: &[u8; 32]) -> [u8; 32] {
    secret.diffie_hellman(&PublicKey::from(*public)).to_bytes()
}

async fn read_frame(tcp: &mut TcpStream) -> Result<Vec<u8>> {
    let len = tcp.read_u16().await? as usize;
    let mut b = vec![0u8; len];
    tcp.read_exact(&mut b).await?;
    Ok(b)
}

async fn write_frame(tcp: &mut TcpStream, b: &[u8]) -> Result<()> {
    ensure!(b.len() <= MAX_NOISE_MESSAGE, "Noise message too long");
    let mut out = (b.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(b);
    tcp.write_all(&out).await?;
    Ok(())
}

fn payload(id: &Identity, static_pub: &[u8; 32]) -> Vec<u8> {
    let mut msg = SIG_PREFIX.to_vec();
    msg.extend_from_slice(static_pub);
    let sig = id.key.sign(&msg).to_bytes();
    let mut out = Vec::new();
    wire::pb_bytes(&mut out, 1, &id.public_key_proto);
    wire::pb_bytes(&mut out, 2, &sig);
    out
}

/// Check the remote's handshake payload against its Noise static key; its peer id.
fn verify_payload(p: &[u8], remote_static: &[u8; 32]) -> Result<Vec<u8>> {
    let mut key = None;
    let mut sig = None;
    for (f, v) in wire::pb_fields(p)? {
        match (f, v) {
            (1, PbValue::Bytes(b)) => key = Some(b),
            (2, PbValue::Bytes(b)) => sig = Some(b),
            _ => {}
        }
    }
    let key_proto = key.context("handshake payload has no identity key")?;
    let sig = sig.context("handshake payload has no signature")?;
    let ed = wire::parse_public_key(key_proto)?;
    let vk = VerifyingKey::from_bytes(&ed).context("invalid Ed25519 identity key")?;
    let sig = ed25519_dalek::Signature::from_slice(sig).context("signature must be 64 bytes")?;
    let mut msg = SIG_PREFIX.to_vec();
    msg.extend_from_slice(remote_static);
    vk.verify(&msg, &sig)
        .context("the identity signature over the Noise static key does not verify")?;
    Ok(wire::peer_id(key_proto))
}

/// The secured connection, before it is split for the multiplexer.
pub struct NoiseConn {
    pub reader: NoiseReader,
    pub writer: NoiseWriter,
    pub remote_peer: Vec<u8>,
}

/// Run the handshake on a TCP connection that has just selected `/noise`.
pub async fn handshake(
    mut tcp: TcpStream,
    id: &Identity,
    initiator: bool,
    expected_peer: Option<&[u8]>,
) -> Result<NoiseConn> {
    let s = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let s_pub = PublicKey::from(&s).to_bytes();
    let e = StaticSecret::random_from_rng(rand::rngs::OsRng);
    let e_pub = PublicKey::from(&e).to_bytes();
    let mut st = Symmetric::new();
    let take32 = |b: &[u8], at: usize| -> Result<[u8; 32]> {
        b.get(at..at + 32)
            .and_then(|x| x.try_into().ok())
            .context("Noise message too short")
    };
    let (remote_static, remote_payload) = if initiator {
        // -> e
        st.mix_hash(&e_pub);
        let mut m1 = e_pub.to_vec();
        m1.extend(st.encrypt_and_hash(&[])?);
        write_frame(&mut tcp, &m1).await?;
        // <- e, ee, s, es, payload
        let m2 = read_frame(&mut tcp).await?;
        let re = take32(&m2, 0)?;
        st.mix_hash(&re);
        st.mix_key(&dh(&e, &re));
        ensure!(m2.len() >= 32 + 32 + TAG, "Noise message 2 too short");
        let rs: [u8; 32] = st
            .decrypt_and_hash(&m2[32..32 + 32 + TAG])?
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad static key"))?;
        st.mix_key(&dh(&e, &rs));
        let p = st.decrypt_and_hash(&m2[32 + 32 + TAG..])?;
        // -> s, se, payload
        let mut m3 = st.encrypt_and_hash(&s_pub)?;
        st.mix_key(&dh(&s, &re));
        m3.extend(st.encrypt_and_hash(&payload(id, &s_pub))?);
        // Verify before sending our identity on, so a wrong peer learns nothing more.
        let peer = verify_payload(&p, &rs)?;
        if let Some(want) = expected_peer {
            ensure!(
                peer == want,
                "dialled {} but the remote is {}",
                wire::peer_id_string(want),
                wire::peer_id_string(&peer)
            );
        }
        write_frame(&mut tcp, &m3).await?;
        (rs, peer)
    } else {
        // <- e
        let m1 = read_frame(&mut tcp).await?;
        let re = take32(&m1, 0)?;
        st.mix_hash(&re);
        st.decrypt_and_hash(&m1[32..])?;
        // -> e, ee, s, es, payload
        st.mix_hash(&e_pub);
        let mut m2 = e_pub.to_vec();
        st.mix_key(&dh(&e, &re));
        m2.extend(st.encrypt_and_hash(&s_pub)?);
        st.mix_key(&dh(&s, &re));
        m2.extend(st.encrypt_and_hash(&payload(id, &s_pub))?);
        write_frame(&mut tcp, &m2).await?;
        // <- s, se, payload
        let m3 = read_frame(&mut tcp).await?;
        ensure!(m3.len() >= 32 + TAG, "Noise message 3 too short");
        let rs: [u8; 32] = st
            .decrypt_and_hash(&m3[..32 + TAG])?
            .try_into()
            .map_err(|_| anyhow::anyhow!("bad static key"))?;
        st.mix_key(&dh(&e, &rs));
        let p = st.decrypt_and_hash(&m3[32 + TAG..])?;
        let peer = verify_payload(&p, &rs)?;
        (rs, peer)
    };
    let _ = remote_static;
    let (c1, c2) = st.split();
    let (send, recv) = if initiator { (c1, c2) } else { (c2, c1) };
    let (r, w) = tcp.into_split();
    Ok(NoiseConn {
        reader: NoiseReader {
            tcp: r,
            cipher: recv,
            buf: Vec::new(),
            pos: 0,
        },
        writer: NoiseWriter {
            tcp: w,
            cipher: send,
        },
        remote_peer: remote_payload,
    })
}

pub struct NoiseReader {
    tcp: OwnedReadHalf,
    cipher: Cipher,
    buf: Vec<u8>,
    pos: usize,
}

impl NoiseReader {
    async fn fill(&mut self) -> Result<()> {
        let len = match self.tcp.read_u16().await {
            Ok(l) => l as usize,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => bail!("connection closed"),
            Err(e) => return Err(e.into()),
        };
        ensure!(len >= TAG, "Noise transport message shorter than its tag");
        let mut c = vec![0u8; len];
        self.tcp.read_exact(&mut c).await?;
        self.buf = self.cipher.decrypt(&[], &c)?;
        self.pos = 0;
        Ok(())
    }
    pub async fn read_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < out.len() {
            if self.pos == self.buf.len() {
                self.fill().await?;
                continue;
            }
            let n = (out.len() - done).min(self.buf.len() - self.pos);
            out[done..done + n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            done += n;
        }
        Ok(())
    }
}

pub struct NoiseWriter {
    tcp: OwnedWriteHalf,
    cipher: Cipher,
}

impl NoiseWriter {
    pub async fn write_all(&mut self, b: &[u8]) -> Result<()> {
        let mut out = Vec::with_capacity(b.len() + (b.len() / MAX_PLAINTEXT + 1) * (TAG + 2));
        for chunk in b.chunks(MAX_PLAINTEXT) {
            let c = self.cipher.encrypt(&[], chunk)?;
            out.extend_from_slice(&(c.len() as u16).to_be_bytes());
            out.extend(c);
        }
        self.tcp.write_all(&out).await?;
        Ok(())
    }
    pub async fn shutdown(&mut self) {
        let _ = self.tcp.shutdown().await;
    }
}

impl wire::Io for NoiseConn {
    fn read_exact<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        self.reader.read_exact(buf)
    }
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> impl Future<Output = Result<()>> + Send + 'a {
        self.writer.write_all(buf)
    }
}
