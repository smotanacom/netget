//! ADC BASE/TIGR hub negotiation and selected chat/search routing.
use crate::server::p2p_support::{DeviceSession, Framer, ReadStream, ScannerSession, Stream};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
};
use tokio::{io::AsyncWriteExt, sync::mpsc};
pub const MAX_COMMAND: usize = 64 * 1024;
pub fn escape(text: &str) -> Result<String> {
    ensure!(
        text.len() <= 8192 && !text.contains('\0'),
        "ADC field too large or contains NUL"
    );
    Ok(text
        .replace('\\', "\\\\")
        .replace(' ', "\\s")
        .replace('\n', "\\n"))
}
pub fn unescape(text: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            out.push(match chars.next() {
                Some('\\') => '\\',
                Some('s') => ' ',
                Some('n') => '\n',
                _ => bail!("invalid ADC escape"),
            });
        } else {
            out.push(ch);
        }
    }
    ensure!(!out.contains(['\0', '\r']), "invalid ADC field");
    Ok(out)
}
pub fn words(frame: &[u8]) -> Result<Vec<String>> {
    ensure!(
        frame.ends_with(b"\n") && frame.len() <= MAX_COMMAND,
        "ADC framing"
    );
    let line = std::str::from_utf8(&frame[..frame.len() - 1])?;
    ensure!(!line.contains(['\r', '\0']) && line.len() >= 4, "ADC line");
    let words: Vec<_> = line.split(' ').map(unescape).collect::<Result<_>>()?;
    ensure!(
        words.len() <= 128
            && words[0].len() == 4
            && words[0].bytes().all(|b| b.is_ascii_uppercase()),
        "ADC command"
    );
    Ok(words)
}
pub fn encode(cmd: &str, args: &[String]) -> Result<Vec<u8>> {
    let mut line = cmd.to_string();
    for arg in args {
        line.push(' ');
        line.push_str(&escape(arg)?);
    }
    line.push('\n');
    ensure!(line.len() <= MAX_COMMAND, "ADC command cap");
    Ok(line.into_bytes())
}
pub fn hash(bytes: &[u8]) -> String {
    use tiger::{Digest, Tiger};
    crate::server::p2p_support::base32(&Tiger::digest(bytes))
}
fn sid(value: u32) -> String {
    let v = (value & 0xfffff) << 4;
    crate::server::p2p_support::base32(&v.to_be_bytes()[1..])[..4].to_string()
}
pub fn valid_sid(text: &str) -> bool {
    text.len() == 4
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
}
pub fn cid(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len() == 39, "ADC CID/PID length");
    let mut out = Vec::new();
    let mut value = 0u32;
    let mut bits = 0;
    for ch in text.bytes() {
        let n = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'2'..=b'7' => ch - b'2' + 26,
            _ => bail!("invalid ADC base32"),
        };
        value = (value << 5) | u32::from(n);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((value >> bits) as u8);
        }
    }
    ensure!(
        out.len() == 24 && (value & 7) == 0,
        "noncanonical ADC identity"
    );
    Ok(out)
}
struct User {
    nickname: String,
    info: Vec<u8>,
    tx: mpsc::Sender<Vec<u8>>,
    stop: tokio_util::sync::CancellationToken,
}
#[derive(Default)]
pub struct Hub {
    next: AtomicU32,
    users: Mutex<HashMap<String, User>>,
}
impl Hub {
    fn broadcast(&self, except: &str, bytes: Vec<u8>) {
        if let Ok(users) = self.users.lock() {
            for (id, user) in users.iter() {
                if id != except {
                    if user.tx.try_send(bytes.clone()).is_err() {
                        user.stop.cancel();
                    }
                }
            }
        }
    }
}
pub struct Device {
    frame: Framer,
    hub: Arc<Hub>,
    sid: String,
    supported: bool,
    joined: bool,
    tx: mpsc::Sender<Vec<u8>>,
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    pending: Option<(String, Vec<u8>)>,
    stop: tokio_util::sync::CancellationToken,
}
impl Device {
    pub fn new(hub: Arc<Hub>) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let id = sid(hub.next.fetch_add(1, Ordering::Relaxed));
        Self {
            frame: Framer::default(),
            hub,
            sid: id,
            supported: false,
            joined: false,
            tx,
            rx: Some(rx),
            pending: None,
            stop: tokio_util::sync::CancellationToken::new(),
        }
    }
}
impl Default for Device {
    fn default() -> Self {
        Self::new(Arc::new(Hub::default()))
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        if self.joined {
            if let Ok(mut users) = self.hub.users.lock() {
                users.remove(&self.sid);
            }
            self.hub
                .broadcast(&self.sid, format!("IQUI {}\n", self.sid).into_bytes());
        }
    }
}
#[async_trait]
impl DeviceSession for Device {
    fn outgoing(&mut self) -> Option<mpsc::Receiver<Vec<u8>>> {
        self.rx.take()
    }
    fn greeting(&mut self) -> Result<Vec<u8>> {
        Ok(format!(
            "ISUP ADBASE ADTIGR\nISID {}\nIINF NINetGet DESelected\\shub\\ssimulator VE1.0\n",
            self.sid
        )
        .into_bytes())
    }
    async fn read(&mut self, s: &mut ReadStream) -> Result<Vec<u8>> {
        tokio::select! {r=self.frame.delimited(s,b'\n',MAX_COMMAND)=>r,_=self.stop.cancelled()=>bail!("slow hub peer disconnected")}
    }
    fn receive(&mut self, b: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        let w = words(b)?;
        if w[0] == "HSUP" {
            ensure!(
                !self.supported
                    && w.iter().any(|v| v == "ADBASE")
                    && w.iter().any(|v| v == "ADTIGR"),
                "ADC BASE/TIGR required"
            );
            self.supported = true;
            return Ok((vec![], None));
        }
        ensure!(
            self.supported && w.get(1) == Some(&self.sid),
            "ADC negotiation or SID mismatch"
        );
        if w[0] == "BINF" && !self.joined {
            let tags = |tag: &str| w.iter().skip(2).find_map(|word| word.strip_prefix(tag));
            let pid = cid(tags("PD").context("ADC PID required")?)?;
            let public = tags("ID").context("ADC CID required")?;
            ensure!(hash(&pid) == public, "CID/PID mismatch");
            let nick = tags("NI").context("nickname required")?;
            ensure!(
                !nick.is_empty() && nick.len() <= 64 && !nick.contains(['\n', '\r']),
                "invalid nickname"
            );
            let args: Vec<_> = w
                .iter()
                .skip(1)
                .filter(|v| !v.starts_with("PD"))
                .cloned()
                .collect();
            self.pending = Some((nick.into(), encode("BINF", &args)?));
            return Ok((
                vec![],
                Some(json!({"operation":"identify","sid":self.sid,"cid":public,"nickname":nick})),
            ));
        }
        ensure!(self.joined, "ADC identification not accepted");
        let operation = match w[0].as_str() {
            "BMSG" | "DMSG" => "chat",
            "BSCH" => "search",
            "DRES" => "search_result",
            "DCTM" => "connect",
            "DRCM" => "reverse_connect",
            _ => return Ok((b"ISTA 140 Unsupported\n".to_vec(), None)),
        };
        let direct = w[0].starts_with('D');
        if direct {
            ensure!(
                w.len() >= 4 && valid_sid(&w[2]),
                "invalid direct destination"
            );
        }
        let args = &w[if direct { 3 } else { 2 }..];
        match operation {
            "connect" => {
                ensure!(
                    args.len() == 3 && matches!(args[0].as_str(), "ADC/1.0" | "ADCS/1.0"),
                    "connect syntax"
                );
                let port: u16 = args[1].parse()?;
                ensure!(
                    port > 0 && !args[2].is_empty() && args[2].len() <= 128,
                    "connect endpoint/token"
                );
            }
            "reverse_connect" => ensure!(
                args.len() == 2
                    && matches!(args[0].as_str(), "ADC/1.0" | "ADCS/1.0")
                    && !args[1].is_empty()
                    && args[1].len() <= 128,
                "reverse-connect syntax"
            ),
            "chat" => ensure!(!args.is_empty(), "chat message required"),
            "search" | "search_result" => ensure!(
                !args.is_empty() && args.iter().all(|a| a.len() >= 2),
                "search tags required"
            ),
            _ => {}
        }
        Ok((
            vec![],
            Some(
                json!({"operation":operation,"sid":self.sid,"destination":if direct{Some(&w[2])}else{None},"arguments":w.iter().skip(if direct{3}else{2}).collect::<Vec<_>>(),"wire":std::str::from_utf8(b)?}),
            ),
        ))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let accepted = a.is_some_and(|a| a["accepted"] == true);
        if r["operation"] == "identify" {
            if !accepted {
                self.pending = None;
                return Ok(b"ISTA 223 Identification\\srefused\n".to_vec());
            }
            let (nickname, info) = self.pending.take().context("missing pending identity")?;
            let mut users = self
                .hub
                .users
                .lock()
                .map_err(|_| anyhow::anyhow!("hub lock"))?;
            if users.values().any(|u| u.nickname == nickname) {
                return Ok(b"ISTA 222 Nickname\\sin\\suse\n".to_vec());
            }
            ensure!(users.len() < 256, "hub full");
            let mut initial = Vec::new();
            for user in users.values() {
                initial.extend_from_slice(&user.info);
                if user.tx.try_send(info.clone()).is_err() {
                    user.stop.cancel();
                }
            }
            initial.extend_from_slice(&info);
            users.insert(
                self.sid.clone(),
                User {
                    nickname,
                    info,
                    tx: self.tx.clone(),
                    stop: self.stop.clone(),
                },
            );
            self.joined = true;
            return Ok(initial);
        }
        if !accepted {
            return Ok(b"ISTA 140 Operation\\srefused\n".to_vec());
        }
        let bytes = r["wire"].as_str().context("wire")?.as_bytes().to_vec();
        if let Some(destination) = r["destination"].as_str() {
            let users = self
                .hub
                .users
                .lock()
                .map_err(|_| anyhow::anyhow!("hub lock"))?;
            if let Some(user) = users.get(destination) {
                user.tx
                    .try_send(bytes)
                    .map_err(|_| anyhow::anyhow!("destination queue full"))?;
            } else {
                return Ok(b"ISTA 140 Unknown\\sdestination\n".to_vec());
            }
        } else {
            self.hub.broadcast("", bytes);
        }
        Ok(vec![])
    }
}
pub struct Scanner {
    frame: Framer,
    sid: String,
    pid: String,
    cid: String,
    nick: String,
}
impl Default for Scanner {
    fn default() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut bytes);
        let public = hash(&bytes);
        Self {
            frame: Framer::default(),
            sid: String::new(),
            pid: crate::server::p2p_support::base32(&bytes),
            nick: format!("NetGet{}", &public[..6]),
            cid: public,
        }
    }
}
pub fn validate(v: &Value) -> Result<()> {
    match v["type"].as_str() {
        Some("adc_chat") => {
            escape(v["message"].as_str().context("message")?)?;
            if let Some(sid) = v["destination"].as_str() {
                ensure!(valid_sid(sid), "destination SID");
            }
        }
        Some("adc_search") => {
            escape(v["query"].as_str().context("query")?)?;
        }
        Some("adc_connect") | Some("adc_reverse_connect") => {
            ensure!(
                valid_sid(v["destination"].as_str().context("destination")?),
                "destination SID"
            );
            if v["type"] == "adc_connect" {
                ensure!(
                    (1..=65535).contains(&v["port"].as_u64().context("port")?),
                    "port"
                );
            }
            escape(v["token"].as_str().context("token")?)?;
        }
        Some("adc_search_result") => {
            ensure!(
                valid_sid(v["destination"].as_str().context("destination")?),
                "destination SID"
            );
            escape(v["identifier"].as_str().context("identifier")?)?;
            crate::server::p2p_support::number(v, "size", u64::MAX)?;
            escape(v["tth"].as_str().context("tth")?)?;
            escape(v["token"].as_str().context("token")?)?;
        }
        _ => bail!("unknown ADC action"),
    }
    Ok(())
}
#[async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut Stream) -> Result<()> {
        s.write_all(b"HSUP ADBASE ADTIGR\n").await?;
        let mut supports = false;
        let mut identified = false;
        for _ in 0..64 {
            let w = words(&self.frame.delimited(s, b'\n', MAX_COMMAND).await?)?;
            match w[0].as_str() {
                "ISUP" => {
                    ensure!(
                        w.iter().any(|v| v == "ADBASE") && w.iter().any(|v| v == "ADTIGR"),
                        "hub lacks BASE/TIGR"
                    );
                    supports = true;
                }
                "ISID" => {
                    ensure!(supports && w.len() == 2 && valid_sid(&w[1]), "invalid ISID");
                    self.sid = w[1].clone();
                    let info = encode(
                        "BINF",
                        &[
                            self.sid.clone(),
                            format!("ID{}", self.cid),
                            format!("PD{}", self.pid),
                            format!("NI{}", self.nick),
                            "SS0".into(),
                            "SF0".into(),
                            "SL1".into(),
                            "VENE1.0".into(),
                            "HN1".into(),
                            "HR0".into(),
                            "HO0".into(),
                        ],
                    )?;
                    s.write_all(&info).await?;
                }
                "BINF" if w.get(1) == Some(&self.sid) => identified = true,
                "ISTA" if w.get(1).is_some_and(|code| code.starts_with('2')) => {
                    bail!("ADC identification refused")
                }
                "IGPA" => bail!("password-authenticated ADC hubs are outside this selected scope"),
                _ => {}
            }
            if identified {
                return Ok(());
            }
        }
        bail!("ADC handshake exceeded 64 frames")
    }
    fn connected(&self) -> Value {
        json!({"sid":self.sid,"cid":self.cid,"nickname":self.nick})
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        validate(a)?;
        let bytes = match a["type"].as_str().context("type")? {
            "adc_chat" => {
                if let Some(dest) = a["destination"].as_str() {
                    encode(
                        "DMSG",
                        &[
                            self.sid.clone(),
                            dest.into(),
                            a["message"].as_str().context("message")?.into(),
                        ],
                    )?
                } else {
                    encode(
                        "BMSG",
                        &[
                            self.sid.clone(),
                            a["message"].as_str().context("message")?.into(),
                        ],
                    )?
                }
            }
            "adc_search" => encode(
                "BSCH",
                &[
                    self.sid.clone(),
                    format!("AN{}", a["query"].as_str().context("query")?),
                    "TO1".into(),
                ],
            )?,
            "adc_connect" => encode(
                "DCTM",
                &[
                    self.sid.clone(),
                    a["destination"].as_str().context("destination")?.into(),
                    if a["secure"] == true {
                        "ADCS/1.0".into()
                    } else {
                        "ADC/1.0".into()
                    },
                    a["port"].to_string(),
                    a["token"].as_str().context("token")?.into(),
                ],
            )?,
            "adc_reverse_connect" => encode(
                "DRCM",
                &[
                    self.sid.clone(),
                    a["destination"].as_str().context("destination")?.into(),
                    if a["secure"] == true {
                        "ADCS/1.0".into()
                    } else {
                        "ADC/1.0".into()
                    },
                    a["token"].as_str().context("token")?.into(),
                ],
            )?,
            "adc_search_result" => encode(
                "DRES",
                &[
                    self.sid.clone(),
                    a["destination"].as_str().context("destination")?.into(),
                    format!("FN{}", a["identifier"].as_str().context("identifier")?),
                    format!("SI{}", a["size"]),
                    format!("TR{}", a["tth"].as_str().context("tth")?),
                    format!("TO{}", a["token"].as_str().context("token")?),
                ],
            )?,
            _ => bail!("ADC action"),
        };
        s.write_all(&bytes).await?;
        Ok(json!({"sent":true}))
    }
    async fn idle(&mut self, s: &mut Stream) -> Result<Option<Value>> {
        let w = words(&self.frame.delimited(s, b'\n', MAX_COMMAND).await?)?;
        Ok(Some(json!({"command":w[0],"arguments":&w[1..]})))
    }
}
