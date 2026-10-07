//! Selected NMDC peer negotiation and ADCGET/ADCSND binary transfers.
pub use crate::server::p2p_support::files::{content, decoded_result, tth, MAX_FILE};
use crate::server::p2p_support::{DeviceSession, Framer, ReadStream, ScannerSession, Stream};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub const MAX_COMMAND: usize = 64 * 1024;
pub const LOCK: &[u8] = b"EXTENDEDPROTOCOLABCABCABCABCABCABC";
fn field(v: &str) -> Result<&str> {
    ensure!(
        !v.is_empty() && v.len() <= 4096 && !v.bytes().any(|b| b <= 32 || matches!(b, b'|' | b'$')),
        "invalid NMDC field"
    );
    Ok(v)
}
pub fn validate(v: &Value) -> Result<()> {
    ensure!(v["type"] == "dc_peer_get", "unknown NMDC peer action");
    field(v["identifier"].as_str().context("identifier required")?)?;
    ensure!(v["offset"].as_u64().is_some(), "offset required");
    let len = v["length"].as_i64().context("length required")?;
    ensure!(
        len == -1 || (0..=MAX_FILE as i64).contains(&len),
        "length exceeds transfer cap"
    );
    Ok(())
}
pub fn transfer_event(command: &str) -> Result<Value> {
    let words: Vec<_> = command.split_whitespace().collect();
    ensure!(
        words.len() == 5 && words[0] == "$ADCGET" && words[1] == "file",
        "unsupported peer request"
    );
    field(words[2])?;
    let offset: u64 = words[3].parse()?;
    let length: i64 = words[4].parse()?;
    ensure!(
        length == -1 || (0..=MAX_FILE as i64).contains(&length),
        "request too large"
    );
    Ok(json!({"operation":"get","identifier":words[2],"offset":offset,"length":length}))
}
#[derive(Default)]
pub struct Device {
    frame: Framer,
    nick: bool,
    key: bool,
    lock: bool,
}
#[async_trait]
impl DeviceSession for Device {
    fn greeting(&mut self) -> Result<Vec<u8>> {
        Ok(format!(
            "$MyNick NetGet|$Lock {} Pk=NetGet|",
            std::str::from_utf8(LOCK)?
        )
        .into_bytes())
    }
    async fn read(&mut self, s: &mut ReadStream) -> Result<Vec<u8>> {
        self.frame.delimited(s, b'|', MAX_COMMAND).await
    }
    fn receive(&mut self, b: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        let cmd = b.strip_suffix(b"|").context("delimiter")?;
        if let Some(nick) = cmd.strip_prefix(b"$MyNick ") {
            ensure!(!nick.is_empty() && nick.len() <= 64, "nickname");
            self.nick = true;
            return Ok((vec![], None));
        }
        if let Some(lock) = cmd.strip_prefix(b"$Lock ") {
            let lock = lock.split(|b| *b == b' ').next().context("lock")?;
            let mut reply = b"$Supports ADCGet TTHF XMLBZList|$Direction Upload 1|$Key ".to_vec();
            reply.extend(crate::client::dc::calculate_dc_key(lock)?);
            reply.push(b'|');
            self.lock = true;
            return Ok((reply, None));
        }
        if let Some(key) = cmd.strip_prefix(b"$Key ") {
            ensure!(
                key == crate::client::dc::calculate_dc_key(LOCK)?,
                "wrong key"
            );
            self.key = true;
            return Ok((vec![], None));
        }
        if cmd.starts_with(b"$Supports ") || cmd.starts_with(b"$Direction Download ") {
            return Ok((vec![], None));
        }
        ensure!(
            self.nick && self.key && self.lock,
            "peer negotiation incomplete"
        );
        let cmd = std::str::from_utf8(cmd)?;
        match transfer_event(cmd) {
            Ok(e) => Ok((vec![], Some(e))),
            Err(_) => Ok((b"$Error Unsupported request|".to_vec(), None)),
        }
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        match content(r, a) {
            Ok(data) => {
                let mut reply = format!(
                    "$ADCSND file {} {} {}|",
                    r["identifier"].as_str().context("identifier")?,
                    r["offset"],
                    data.len()
                )
                .into_bytes();
                reply.extend(data);
                Ok(reply)
            }
            Err(_) => Ok(b"$Error File Not Available|".to_vec()),
        }
    }
}
#[derive(Default)]
pub struct Scanner {
    frame: Framer,
}
#[async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut Stream) -> Result<()> {
        s.write_all(
            format!(
                "$MyNick NetGetClient|$Lock {} Pk=NetGet|",
                std::str::from_utf8(LOCK)?
            )
            .as_bytes(),
        )
        .await?;
        let (mut nick, mut lock, mut key, mut direction) = (false, false, false, false);
        for _ in 0..16 {
            let frame = self.frame.delimited(s, b'|', MAX_COMMAND).await?;
            let cmd = &frame[..frame.len() - 1];
            if cmd.starts_with(b"$MyNick ") {
                nick = true;
            } else if let Some(challenge) = cmd.strip_prefix(b"$Lock ") {
                let mut reply =
                    b"$Supports ADCGet TTHF XMLBZList|$Direction Download 1|$Key ".to_vec();
                reply.extend(crate::client::dc::calculate_dc_key(
                    challenge.split(|b| *b == b' ').next().context("lock")?,
                )?);
                reply.push(b'|');
                s.write_all(&reply).await?;
                lock = true;
            } else if let Some(received) = cmd.strip_prefix(b"$Key ") {
                ensure!(
                    received == crate::client::dc::calculate_dc_key(LOCK)?,
                    "wrong peer key"
                );
                key = true;
            } else if cmd.starts_with(b"$Direction Upload ") {
                direction = true;
            }
            if nick && lock && key && direction {
                return Ok(());
            }
        }
        bail!("peer negotiation exceeded 16 messages")
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        validate(a)?;
        let id = a["identifier"].as_str().context("identifier")?;
        s.write_all(format!("$ADCGET file {} {} {}|", id, a["offset"], a["length"]).as_bytes())
            .await?;
        let frame = self.frame.delimited(s, b'|', MAX_COMMAND).await?;
        let text = std::str::from_utf8(&frame[..frame.len() - 1])?;
        let words: Vec<_> = text.split_whitespace().collect();
        if text.starts_with("$Error ") {
            return Ok(json!({"error":&text[7..]}));
        }
        ensure!(
            words.len() == 5 && words[0] == "$ADCSND" && words[1] == "file" && words[2] == id,
            "unexpected transfer response"
        );
        ensure!(
            words[3].parse::<u64>()? == a["offset"].as_u64().context("offset")?,
            "offset mismatch"
        );
        let count: usize = words[4].parse()?;
        ensure!(count <= MAX_FILE, "transfer exceeds cap");
        if let Some(len) = a["length"].as_i64().filter(|n| *n >= 0) {
            ensure!(count == len as usize, "length mismatch");
        }
        let mut data = vec![0; count];
        s.read_exact(&mut data).await?;
        decoded_result(id, data, a["expected_tth"].as_str())
    }
}
