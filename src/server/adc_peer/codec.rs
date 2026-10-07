//! Direct ADC/ADCS peer file transfers; content belongs to request handlers.
use crate::server::{
    adc::codec::{cid, encode, hash, words},
    p2p_support::{
        files::{content, decoded_result, MAX_FILE},
        DeviceSession, Framer, ReadStream, ScannerSession, Stream,
    },
};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub const MAX_COMMAND: usize = 64 * 1024;
pub fn identity() -> String {
    use rand::RngCore;
    let mut random = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut random);
    hash(&random)
}
pub fn validate(v: &Value) -> Result<()> {
    ensure!(v["type"] == "adc_peer_get", "unknown ADC peer action");
    let id = v["identifier"].as_str().context("identifier")?;
    ensure!(
        !id.is_empty() && id.len() <= 4096 && !id.contains(['\0', '\r', '\n']),
        "identifier"
    );
    ensure!(v["offset"].as_u64().is_some(), "offset");
    let len = v["length"].as_i64().context("length")?;
    ensure!(
        len == -1 || (0..=MAX_FILE as i64).contains(&len),
        "transfer too large"
    );
    Ok(())
}
pub struct Device {
    frame: Framer,
    supported: bool,
    identified: bool,
    cid: String,
}
impl Device {
    pub fn with_cid(cid: String) -> Self {
        Self {
            frame: Framer::default(),
            supported: false,
            identified: false,
            cid,
        }
    }
}
impl Default for Device {
    fn default() -> Self {
        Self {
            frame: Framer::default(),
            supported: false,
            identified: false,
            cid: identity(),
        }
    }
}
#[async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut ReadStream) -> Result<Vec<u8>> {
        self.frame.delimited(s, b'\n', MAX_COMMAND).await
    }
    fn receive(&mut self, b: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        let w = words(b)?;
        if w[0] == "CSUP" {
            ensure!(
                !self.supported
                    && w.iter().any(|v| v == "ADBASE")
                    && w.iter().any(|v| v == "ADTIGR"),
                "ADC BASE/TIGR required"
            );
            self.supported = true;
            return Ok((
                format!("CSUP ADBASE ADTIGR\nCINF ID{}\n", self.cid).into_bytes(),
                None,
            ));
        }
        if w[0] == "CSTA" {
            let code = w.get(1).context("status code")?;
            ensure!(
                code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()),
                "invalid status"
            );
            ensure!(!code.starts_with('2'), "peer fatal status");
            return Ok((vec![], None));
        }
        if w[0] == "CINF" {
            ensure!(self.supported && !self.identified, "peer negotiation order");
            cid(w
                .iter()
                .skip(1)
                .find_map(|w| w.strip_prefix("ID"))
                .context("CID required")?)?;
            self.identified = true;
            return Ok((vec![], None));
        }
        ensure!(
            self.supported && self.identified,
            "peer negotiation incomplete"
        );
        if w[0] != "CGET" {
            return Ok((b"CSTA 140 Unsupported\n".to_vec(), None));
        }
        ensure!(w.len() == 5 && w[1] == "file", "selected file request only");
        let offset: u64 = w[3].parse()?;
        let length: i64 = w[4].parse()?;
        let v = json!({"type":"adc_peer_get","operation":"get","identifier":w[2],"offset":offset,"length":length});
        validate(&v)?;
        Ok((vec![], Some(v)))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        match content(r, a) {
            Ok(data) => {
                let mut bytes = encode(
                    "CSND",
                    &[
                        "file".into(),
                        r["identifier"].as_str().context("identifier")?.into(),
                        r["offset"].to_string(),
                        data.len().to_string(),
                    ],
                )?;
                bytes.extend(data);
                Ok(bytes)
            }
            Err(_) => Ok(b"CSTA 151 File\\snot\\savailable\n".to_vec()),
        }
    }
}
pub struct Scanner {
    frame: Framer,
    cid: String,
    token: Option<String>,
}
impl Scanner {
    pub fn with_identity(id: Option<String>, token: Option<String>) -> Result<Self> {
        let id = id.unwrap_or_else(identity);
        cid(&id)?;
        if let Some(t) = &token {
            ensure!(
                !t.is_empty() && t.len() <= 128 && !t.contains([' ', '\n', '\r', '\0']),
                "invalid token"
            );
        }
        Ok(Self {
            frame: Framer::default(),
            cid: id,
            token,
        })
    }
}
impl Default for Scanner {
    fn default() -> Self {
        Self {
            frame: Framer::default(),
            cid: identity(),
            token: None,
        }
    }
}
#[async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut Stream) -> Result<()> {
        s.write_all(
            format!(
                "CSUP ADBASE ADTIGR\nCINF ID{}{}\n",
                self.cid,
                self.token
                    .as_ref()
                    .map_or(String::new(), |t| format!(" TO{t}"))
            )
            .as_bytes(),
        )
        .await?;
        let mut supported = false;
        for _ in 0..16 {
            let w = words(&self.frame.delimited(s, b'\n', MAX_COMMAND).await?)?;
            match w[0].as_str() {
                "CSUP" => {
                    ensure!(
                        w.iter().any(|v| v == "ADBASE") && w.iter().any(|v| v == "ADTIGR"),
                        "peer capabilities"
                    );
                    supported = true;
                }
                "CINF" => {
                    ensure!(supported, "peer order");
                    cid(w
                        .iter()
                        .skip(1)
                        .find_map(|w| w.strip_prefix("ID"))
                        .context("CID required")?)?;
                    return Ok(());
                }
                "CSTA" => {
                    let code = w.get(1).context("status code")?;
                    ensure!(!code.starts_with('2'), "peer refused connection");
                }
                _ => {}
            }
        }
        bail!("peer handshake exceeded cap")
    }
    async fn idle(&mut self, s: &mut Stream) -> Result<Option<Value>> {
        let b = self.frame.delimited(s, b'\n', MAX_COMMAND).await?;
        if b == b"\n" {
            return Ok(None);
        }
        let w = words(&b)?;
        ensure!(
            w[0] == "CSTA" && w.get(1).is_some_and(|c| c.starts_with('0')),
            "unexpected peer frame"
        );
        Ok(None)
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        validate(a)?;
        let id = a["identifier"].as_str().context("identifier")?;
        s.write_all(&encode(
            "CGET",
            &[
                "file".into(),
                id.into(),
                a["offset"].to_string(),
                a["length"].to_string(),
            ],
        )?)
        .await?;
        let w = words(&self.frame.delimited(s, b'\n', MAX_COMMAND).await?)?;
        if w[0] == "CSTA" {
            return Ok(json!({"error":w.get(1),"message":w.get(2)}));
        }
        ensure!(
            w.len() == 5 && w[0] == "CSND" && w[1] == "file" && w[2] == id,
            "transfer response"
        );
        ensure!(
            w[3].parse::<u64>()? == a["offset"].as_u64().context("offset")?,
            "offset mismatch"
        );
        let count: usize = w[4].parse()?;
        ensure!(count <= MAX_FILE, "transfer exceeds cap");
        if let Some(length) = a["length"].as_i64().filter(|n| *n >= 0) {
            ensure!(count == length as usize, "length mismatch");
        }
        let mut data = vec![0; count];
        s.read_exact(&mut data).await?;
        decoded_result(id, data, a["expected_tth"].as_str())
    }
}
