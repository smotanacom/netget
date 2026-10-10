//! Stratum V1 on the wire, for both roles: newline-delimited JSON-RPC messages, and the share
//! arithmetic a pool and a miner must agree on byte for byte — the coinbase a job describes,
//! the block header a share commits to, its double SHA-256, and its difficulty.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// One JSON-RPC line, at most.
pub const MAX_LINE: usize = 16 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of extranonce2 the pool asks miners to roll.
pub const EXTRANONCE2_SIZE: usize = 4;
/// Bytes of extranonce1 the pool assigns each connection.
pub const EXTRANONCE1_SIZE: usize = 4;
/// How far past the job's ntime a share may roll its timestamp, in seconds.
pub const NTIME_ROLL: u32 = 7200;
/// Merkle branches a job may carry (a block of 2^16 transactions).
pub const MAX_BRANCHES: usize = 16;
/// Coinbase halves are at most this long.
pub const MAX_COINBASE: usize = 4096;
/// The coinbase message a pool job carries, in bytes.
pub const MAX_COINBASE_MESSAGE: usize = 64;
/// Difficulty 1: the target 0x00000000FFFF0000…, as a number.
pub const DIFF1: f64 = 26959535291011309493156476344723991336010898738574164086137773096960.0;

/// Newline-delimited JSON values from one side of a connection.
pub struct Lines<R> {
    reader: BufReader<R>,
    line: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Lines<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            line: Vec::new(),
        }
    }

    /// The next JSON value; None at a clean end of stream. A line over `MAX_LINE`, or one
    /// that is not JSON, is an error: the peer is not speaking Stratum.
    pub async fn next(&mut self, idle: Duration) -> Result<Option<Value>> {
        loop {
            self.line.clear();
            loop {
                let buf = tokio::time::timeout(idle, self.reader.fill_buf())
                    .await
                    .context("Stratum peer idle")??;
                if buf.is_empty() {
                    if self.line.is_empty() {
                        return Ok(None);
                    }
                    bail!("Stratum stream ended inside a line");
                }
                let (take, done) = match buf.iter().position(|b| *b == b'\n') {
                    Some(i) => (i + 1, true),
                    None => (buf.len(), false),
                };
                let content = self.line.len() + take - usize::from(done);
                ensure!(
                    content <= MAX_LINE,
                    "Stratum line longer than {MAX_LINE} bytes"
                );
                self.line.extend_from_slice(&buf[..take]);
                self.reader.consume(take);
                if done {
                    break;
                }
            }
            let text = std::str::from_utf8(&self.line).context("Stratum line is not UTF-8")?;
            if text.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(text.trim())
                .map(Some)
                .context("Stratum line is not JSON");
        }
    }
}

pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Value,
        error: Value,
    },
}

pub fn parse(v: Value) -> Result<Message> {
    let obj = v
        .as_object()
        .context("a Stratum message is a JSON object")?;
    let id = obj.get("id").cloned().unwrap_or(Value::Null);
    match obj.get("method") {
        Some(Value::String(method)) => {
            let params = obj.get("params").cloned().unwrap_or(json!([]));
            Ok(if id.is_null() {
                Message::Notification {
                    method: method.clone(),
                    params,
                }
            } else {
                Message::Request {
                    id,
                    method: method.clone(),
                    params,
                }
            })
        }
        Some(_) => bail!("method must be a string"),
        None => Ok(Message::Response {
            id,
            result: obj.get("result").cloned().unwrap_or(Value::Null),
            error: obj.get("error").cloned().unwrap_or(Value::Null),
        }),
    }
}

pub fn line(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(v).unwrap_or_default();
    out.push(b'\n');
    out
}

/// `[code, message, null]`, Stratum's error shape (20 other, 21 job not found, 22 duplicate,
/// 23 low difficulty, 24 unauthorized worker, 25 not subscribed).
pub fn error(code: u32, message: &str) -> Value {
    json!([code, message, null])
}

pub fn sha256d(data: &[u8]) -> [u8; 32] {
    Sha256::digest(Sha256::digest(data)).into()
}

/// A hash as block explorers and RPC show it: byte-reversed hex.
pub fn display_hex(hash: &[u8; 32]) -> String {
    let mut h = *hash;
    h.reverse();
    hex::encode(h)
}

/// The difficulty a header hash meets: difficulty 1's target over the hash as a number.
pub fn difficulty(hash: &[u8; 32]) -> f64 {
    let value = hash
        .iter()
        .rev()
        .fold(0f64, |acc, b| acc * 256.0 + f64::from(*b));
    if value == 0.0 {
        f64::INFINITY
    } else {
        DIFF1 / value
    }
}

/// A 32-bit field as Stratum writes it: big-endian hex of the number.
pub fn u32_hex(v: u32) -> String {
    format!("{v:08x}")
}

pub fn parse_u32_hex(s: &str, what: &str) -> Result<u32> {
    ensure!(s.len() == 8, "{what} must be 8 hex digits");
    u32::from_str_radix(s, 16).with_context(|| format!("{what} is not hex"))
}

/// One unit of work: what `mining.notify` carries.
#[derive(Clone, Debug)]
pub struct Job {
    pub job_id: String,
    /// The previous block hash in header byte order.
    pub prevhash: [u8; 32],
    pub coinb1: Vec<u8>,
    pub coinb2: Vec<u8>,
    pub branches: Vec<[u8; 32]>,
    pub version: u32,
    pub nbits: u32,
    pub ntime: u32,
    pub clean: bool,
}

/// Stratum's prevhash: header order with every 32-bit word byte-swapped.
fn swap_words(b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..8 {
        for j in 0..4 {
            out[i * 4 + j] = b[i * 4 + 3 - j];
        }
    }
    out
}

fn hex32(s: &str, what: &str) -> Result<[u8; 32]> {
    let v = hex::decode(s).with_context(|| format!("{what} is not hex"))?;
    v.try_into()
        .map_err(|_| anyhow::anyhow!("{what} must be 32 bytes"))
}

impl Job {
    pub fn from_notify(p: &Value) -> Result<Job> {
        let a = p
            .as_array()
            .context("mining.notify params must be an array")?;
        ensure!(a.len() >= 9, "mining.notify has {} params, not 9", a.len());
        let s = |i: usize, what: &str| -> Result<&str> {
            a[i].as_str()
                .with_context(|| format!("{what} must be a string"))
        };
        let coinb1 = hex::decode(s(2, "coinb1")?).context("coinb1 is not hex")?;
        let coinb2 = hex::decode(s(3, "coinb2")?).context("coinb2 is not hex")?;
        ensure!(
            coinb1.len() <= MAX_COINBASE && coinb2.len() <= MAX_COINBASE,
            "coinbase longer than {MAX_COINBASE} bytes"
        );
        let branches = a[4]
            .as_array()
            .context("merkle branches must be an array")?
            .iter()
            .map(|b| hex32(b.as_str().unwrap_or_default(), "merkle branch"))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            branches.len() <= MAX_BRANCHES,
            "more than {MAX_BRANCHES} merkle branches"
        );
        Ok(Job {
            job_id: s(0, "job_id")?.to_string(),
            prevhash: swap_words(&hex32(s(1, "prevhash")?, "prevhash")?),
            coinb1,
            coinb2,
            branches,
            version: parse_u32_hex(s(5, "version")?, "version")?,
            nbits: parse_u32_hex(s(6, "nbits")?, "nbits")?,
            ntime: parse_u32_hex(s(7, "ntime")?, "ntime")?,
            clean: a[8].as_bool().unwrap_or(false),
        })
    }

    pub fn notify_params(&self) -> Value {
        json!([
            self.job_id,
            hex::encode(swap_words(&self.prevhash)),
            hex::encode(&self.coinb1),
            hex::encode(&self.coinb2),
            self.branches.iter().map(hex::encode).collect::<Vec<_>>(),
            u32_hex(self.version),
            u32_hex(self.nbits),
            u32_hex(self.ntime),
            self.clean,
        ])
    }

    /// The previous block hash as RPC shows it.
    pub fn prev_display(&self) -> String {
        display_hex(&self.prevhash)
    }

    pub fn merkle_root(&self, extranonce1: &[u8], extranonce2: &[u8]) -> [u8; 32] {
        let mut coinbase = Vec::with_capacity(
            self.coinb1.len() + extranonce1.len() + extranonce2.len() + self.coinb2.len(),
        );
        coinbase.extend_from_slice(&self.coinb1);
        coinbase.extend_from_slice(extranonce1);
        coinbase.extend_from_slice(extranonce2);
        coinbase.extend_from_slice(&self.coinb2);
        let mut root = sha256d(&coinbase);
        for b in &self.branches {
            let mut pair = [0u8; 64];
            pair[..32].copy_from_slice(&root);
            pair[32..].copy_from_slice(b);
            root = sha256d(&pair);
        }
        root
    }

    /// The 80-byte header for a share, with the nonce left zero.
    pub fn header(&self, merkle_root: &[u8; 32], ntime: u32) -> [u8; 80] {
        let mut h = [0u8; 80];
        h[0..4].copy_from_slice(&self.version.to_le_bytes());
        h[4..36].copy_from_slice(&self.prevhash);
        h[36..68].copy_from_slice(merkle_root);
        h[68..72].copy_from_slice(&ntime.to_le_bytes());
        h[72..76].copy_from_slice(&self.nbits.to_le_bytes());
        h
    }

    /// The hash a share (extranonce2, ntime, nonce) commits to.
    pub fn share_hash(
        &self,
        extranonce1: &[u8],
        extranonce2: &[u8],
        ntime: u32,
        nonce: u32,
    ) -> [u8; 32] {
        let mut h = self.header(&self.merkle_root(extranonce1, extranonce2), ntime);
        h[76..80].copy_from_slice(&nonce.to_le_bytes());
        sha256d(&h)
    }
}

/// A script push of `data`.
fn push(script: &mut Vec<u8>, data: &[u8]) {
    script.push(data.len() as u8);
    script.extend_from_slice(data);
}

/// A pool's job: a coinbase carrying the block height (BIP 34) and `message`, paying nothing
/// to an OP_RETURN output, with room for the extranonces; no other transactions.
pub fn pool_job(
    job_id: String,
    prev_display: &[u8; 32],
    height: u32,
    message: &str,
    nbits: u32,
    ntime: u32,
    clean: bool,
) -> Job {
    let mut prevhash = *prev_display;
    prevhash.reverse();
    let mut height_bytes = height.to_le_bytes().to_vec();
    while height_bytes.len() > 1 && height_bytes.last() == Some(&0) {
        height_bytes.pop();
    }
    if height_bytes.last().is_some_and(|b| b & 0x80 != 0) {
        height_bytes.push(0);
    }
    let message = &message.as_bytes()[..message.len().min(MAX_COINBASE_MESSAGE)];
    let mut script = Vec::new();
    push(&mut script, &height_bytes);
    push(&mut script, message);
    let script_len = script.len() + 1 + EXTRANONCE1_SIZE + EXTRANONCE2_SIZE;
    let mut coinb1 = vec![1, 0, 0, 0, 1];
    coinb1.extend_from_slice(&[0u8; 32]);
    coinb1.extend_from_slice(&[0xff; 4]);
    coinb1.push(script_len as u8);
    coinb1.extend_from_slice(&script);
    coinb1.push((EXTRANONCE1_SIZE + EXTRANONCE2_SIZE) as u8);
    let mut coinb2 = vec![0xff; 4];
    coinb2.push(1);
    coinb2.extend_from_slice(&0u64.to_le_bytes());
    let out_script = [&[0x6a, 6][..], b"netget"].concat();
    coinb2.push(out_script.len() as u8);
    coinb2.extend_from_slice(&out_script);
    coinb2.extend_from_slice(&[0u8; 4]);
    Job {
        job_id,
        prevhash,
        coinb1,
        coinb2,
        branches: Vec::new(),
        version: 0x2000_0000,
        nbits,
        ntime,
        clean,
    }
}

/// A previous block hash as RPC writes it (64 hex digits), or zero when absent.
pub fn parse_prev_display(s: Option<&str>) -> Result<[u8; 32]> {
    match s {
        None | Some("") => Ok([0u8; 32]),
        Some(s) => hex32(s, "prev_hash"),
    }
}
