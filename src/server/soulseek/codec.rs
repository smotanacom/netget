//! Selected legacy Soulseek central messages. Application data is handler supplied.
use crate::server::p2p_support::{DeviceSession, Framer, ReadStream, ScannerSession, Stream};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use md5::{Digest, Md5};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
pub const MAX_COMMAND: usize = 64 * 1024;
pub fn digest(text: &str) -> String {
    format!("{:x}", Md5::digest(text.as_bytes()))
}
pub fn length(b: &[u8]) -> Result<usize> {
    let n = u32::from_le_bytes(b.try_into()?) as usize;
    ensure!(
        (4..=MAX_COMMAND - 4).contains(&n),
        "invalid Soulseek length"
    );
    Ok(n + 4)
}
pub struct Reader<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}
impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("length overflow")?;
        ensure!(end <= self.bytes.len(), "truncated message");
        let b = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(b)
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    pub fn boolean(&mut self) -> Result<bool> {
        let b = self.take(1)?[0];
        ensure!(b <= 1, "invalid boolean");
        Ok(b == 1)
    }
    pub fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        ensure!(n <= 4096, "string too long");
        let s = std::str::from_utf8(self.take(n)?)?;
        ensure!(!s.contains('\0'), "NUL string");
        Ok(s.into())
    }
    pub fn end(&self) -> Result<()> {
        ensure!(self.pos == self.bytes.len(), "unexpected message suffix");
        Ok(())
    }
}
pub fn string(out: &mut Vec<u8>, s: &str) -> Result<()> {
    ensure!(s.len() <= 4096 && !s.contains('\0'), "invalid string");
    out.extend((s.len() as u32).to_le_bytes());
    out.extend(s.as_bytes());
    Ok(())
}
pub fn packet(code: u32, data: Vec<u8>) -> Result<Vec<u8>> {
    ensure!(data.len() <= MAX_COMMAND - 8, "packet too long");
    let mut b = ((data.len() + 4) as u32).to_le_bytes().to_vec();
    b.extend(code.to_le_bytes());
    b.extend(data);
    Ok(b)
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    let s = v[k].as_str().with_context(|| format!("{k} required"))?;
    ensure!(
        !s.is_empty() && s.len() <= 4096 && !s.contains('\0'),
        "invalid {k}"
    );
    Ok(s)
}
pub fn request(v: &Value) -> Result<(u32, Vec<u8>)> {
    let mut b = vec![];
    let code = match v["type"].as_str().context("type")? {
        "soulseek_login" => {
            let u = text(v, "username")?;
            let p = text(v, "password")?;
            ensure!(
                u.len() <= 30 && u.is_ascii() && u.bytes().all(|c| c >= 32) && u.trim() == u,
                "invalid username"
            );
            string(&mut b, u)?;
            string(&mut b, p)?;
            b.extend(177u32.to_le_bytes());
            string(&mut b, &digest(&format!("{u}{p}")))?;
            b.extend(1u32.to_le_bytes());
            1
        }
        "soulseek_rooms" => 64,
        "soulseek_join" => {
            string(&mut b, text(v, "room")?)?;
            b.extend(0u32.to_le_bytes());
            14
        }
        "soulseek_leave" => {
            string(&mut b, text(v, "room")?)?;
            15
        }
        "soulseek_chat" => {
            string(&mut b, text(v, "room")?)?;
            string(&mut b, text(v, "message")?)?;
            13
        }
        "soulseek_status" => {
            string(&mut b, text(v, "username")?)?;
            7
        }
        "soulseek_address" => {
            string(&mut b, text(v, "username")?)?;
            3
        }
        "soulseek_search" => {
            b.extend(
                crate::server::p2p_support::number(v, "ticket", u32::MAX as u64)?.to_le_bytes()
                    [..4]
                    .iter(),
            );
            string(&mut b, text(v, "query")?)?;
            26
        }
        _ => bail!("unknown Soulseek action"),
    };
    Ok((code, b))
}
pub fn validate(v: &Value) -> Result<()> {
    request(v)?;
    Ok(())
}
#[derive(Default)]
pub struct Device {
    frame: Framer,
    username: Option<String>,
    pending: Option<(String, String)>,
}
#[async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut ReadStream) -> Result<Vec<u8>> {
        self.frame.sized(s, 4, MAX_COMMAND, length).await
    }
    fn receive(&mut self, b: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        let mut r = Reader::new(&b[4..]);
        let code = r.u32()?;
        let mut v = json!({"code":code});
        if code == 1 {
            ensure!(
                self.username.is_none() && self.pending.is_none(),
                "duplicate login"
            );
            let u = r.string()?;
            let p = r.string()?;
            let version = r.u32()?;
            let hash = r.string()?;
            let minor = r.u32()?;
            ensure!(hash == digest(&format!("{u}{p}")), "login hash mismatch");
            validate(&json!({"type":"soulseek_login","username":u,"password":p}))?;
            v = json!({"code":1,"operation":"login","username":u,"password":p,"client_version":version,"minor_version":minor});
            self.pending = Some((u, digest(&p)));
        } else {
            ensure!(self.username.is_some(), "login required");
            match code {
                2 => {
                    let _port = r.u32()?;
                    if r.pos < r.bytes.len() {
                        r.u32()?;
                        r.u32()?;
                    }
                    r.end()?;
                    return Ok((vec![], None));
                }
                28 => {
                    ensure!(r.u32()? <= 2, "invalid status");
                    r.end()?;
                    return Ok((vec![], None));
                }
                32 => {
                    r.end()?;
                    return Ok((packet(32, vec![])?, None));
                }
                64 => v["operation"] = json!("rooms"),
                3 | 7 => {
                    v["operation"] = json!(if code == 3 { "address" } else { "status" });
                    v["username"] = json!(r.string()?);
                }
                13 => {
                    v["operation"] = json!("chat");
                    v["room"] = json!(r.string()?);
                    v["message"] = json!(r.string()?);
                }
                14 | 15 => {
                    v["operation"] = json!(if code == 14 { "join" } else { "leave" });
                    v["room"] = json!(r.string()?);
                    if code == 14 && r.pos < r.bytes.len() {
                        ensure!(r.u32()? == 0, "private rooms outside scope");
                    }
                }
                26 => {
                    v["operation"] = json!("search");
                    v["ticket"] = json!(r.u32()?);
                    v["query"] = json!(r.string()?);
                }
                _ => return Ok((vec![], None)),
            }
        }
        r.end()?;
        Ok((vec![], Some(v)))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let empty = json!({});
        let a = a.unwrap_or(&empty);
        let code = r["code"].as_u64().context("code")? as u32;
        let mut b = vec![];
        match code {
            1 => {
                let (u, hash) = self.pending.take().context("no pending login")?;
                let accepted = a["accepted"] == true;
                b.push(u8::from(accepted));
                if accepted {
                    string(&mut b, a["greeting"].as_str().unwrap_or("NetGet"))?;
                    b.extend([1, 0, 0, 127]);
                    string(&mut b, &hash)?;
                    b.push(0);
                    self.username = Some(u);
                } else {
                    string(&mut b, a["error"].as_str().unwrap_or("INVALIDPASS"))?;
                }
            }
            64 => {
                let rooms = a["rooms"].as_array().cloned().unwrap_or_default();
                ensure!(rooms.len() <= 128, "too many rooms");
                b.extend((rooms.len() as u32).to_le_bytes());
                for room in &rooms {
                    string(&mut b, room.as_str().context("room name")?)?;
                }
                b.extend((rooms.len() as u32).to_le_bytes());
                for _ in &rooms {
                    b.extend(0u32.to_le_bytes());
                }
                b.extend([0; 20]);
            }
            14 => {
                string(&mut b, r["room"].as_str().context("room")?)?;
                b.extend([0; 20]);
            }
            15 => string(&mut b, r["room"].as_str().context("room")?)?,
            13 => {
                string(&mut b, r["room"].as_str().context("room")?)?;
                string(&mut b, self.username.as_deref().context("username")?)?;
                string(&mut b, r["message"].as_str().context("message")?)?;
            }
            7 => {
                string(&mut b, r["username"].as_str().context("username")?)?;
                let status = a["status"].as_u64().unwrap_or(0);
                ensure!(status <= 2, "status");
                b.extend((status as u32).to_le_bytes());
                b.push(0);
            }
            3 => {
                string(&mut b, r["username"].as_str().context("username")?)?;
                let ip: std::net::Ipv4Addr = a["ip"].as_str().unwrap_or("0.0.0.0").parse()?;
                b.extend(ip.octets().iter().rev());
                let port = a["port"].as_u64().unwrap_or(0);
                ensure!(port <= 65535, "port");
                b.extend((port as u32).to_le_bytes());
            }
            26 => return Ok(vec![]),
            _ => bail!("unknown reply"),
        };
        packet(code, b)
    }
}
#[derive(Default)]
pub struct Scanner {
    frame: Framer,
    authenticated: bool,
}
fn response(b: &[u8]) -> Result<Value> {
    let mut r = Reader::new(&b[4..]);
    let code = r.u32()?;
    let mut v = json!({"code":code});
    match code {
        1 => {
            let success = r.boolean()?;
            v["success"] = json!(success);
            if success {
                v["greeting"] = json!(r.string()?);
                let ip = r.take(4)?;
                v["ip"] = json!(std::net::Ipv4Addr::new(ip[3], ip[2], ip[1], ip[0]).to_string());
                r.string()?;
                v["privileged"] = json!(r.boolean()?);
            } else {
                v["reason"] = json!(r.string()?);
            }
        }
        3 => {
            v["username"] = json!(r.string()?);
            let ip = r.take(4)?;
            v["ip"] = json!(std::net::Ipv4Addr::new(ip[3], ip[2], ip[1], ip[0]).to_string());
            v["port"] = json!(r.u32()?);
            if r.pos < r.bytes.len() {
                r.u32()?;
                r.take(2)?;
            }
        }
        7 => {
            v["username"] = json!(r.string()?);
            let s = r.u32()?;
            ensure!(s <= 2, "status");
            v["status"] = json!(s);
            v["privileged"] = json!(r.boolean()?);
        }
        13 => {
            v["room"] = json!(r.string()?);
            v["username"] = json!(r.string()?);
            v["message"] = json!(r.string()?);
        }
        15 => v["room"] = json!(r.string()?),
        64 => {
            let count = r.u32()? as usize;
            ensure!(count <= 128, "room count");
            let mut rooms = vec![];
            for _ in 0..count {
                rooms.push(r.string()?);
            }
            ensure!(r.u32()? as usize == count, "room count mismatch");
            let mut counts = vec![];
            for _ in 0..count {
                counts.push(r.u32()?);
            }
            v["rooms"] = json!(rooms);
            v["user_counts"] = json!(counts);
            for _ in 0..5 {
                ensure!(r.u32()? == 0, "private rooms outside scope");
            }
        }
        14 => {
            v["room"] = json!(r.string()?);
            let n = r.u32()?;
            ensure!(n <= 128, "room users cap");
            let mut users = vec![];
            for _ in 0..n {
                users.push(r.string()?);
            }
            let mut status = vec![];
            ensure!(r.u32()? == n, "user status count");
            for _ in 0..n {
                let x = r.u32()?;
                ensure!(x <= 2, "user status");
                status.push(x);
            }
            ensure!(r.u32()? == n, "statistics count");
            let mut stats = vec![];
            for _ in 0..n {
                stats.push(json!({"speed":r.u32()?,"uploads":r.u64()?,"files":r.u32()?,"folders":r.u32()?}));
            }
            ensure!(r.u32()? == n, "slot count");
            let mut slots = vec![];
            for _ in 0..n {
                slots.push(r.u32()?);
            }
            ensure!(r.u32()? == n, "country count");
            let mut countries = vec![];
            for _ in 0..n {
                countries.push(r.string()?);
            }
            v["users"] = json!(users);
            v["status"] = json!(status);
            v["stats"] = json!(stats);
            v["slots_free"] = json!(slots);
            v["countries"] = json!(countries);
            if r.pos < r.bytes.len() {
                v["owner"] = json!(r.string()?);
                let n = r.u32()?;
                ensure!(n <= 128, "operator count");
                let mut operators = vec![];
                for _ in 0..n {
                    operators.push(r.string()?);
                }
                v["operators"] = json!(operators);
            }
        }
        26 => {
            v["username"] = json!(r.string()?);
            v["ticket"] = json!(r.u32()?);
            v["query"] = json!(r.string()?);
        }
        32 => {}
        _ => {
            use base64::Engine;
            v["payload_base64"] =
                json!(base64::engine::general_purpose::STANDARD
                    .encode(r.take(r.bytes.len() - r.pos)?));
        }
    }
    r.end()?;
    Ok(v)
}
#[async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, _: &mut Stream) -> Result<()> {
        Ok(())
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        let (code, b) = request(a)?;
        ensure!(code == 1 || self.authenticated, "login required");
        s.write_all(&packet(code, b)?).await?;
        if code == 26 {
            return Ok(json!({"sent":true,"ticket":a["ticket"]}));
        }
        let mut announcements = vec![];
        let mut v = loop {
            let incoming = response(&self.frame.sized(s, 4, MAX_COMMAND, length).await?)?;
            if incoming["code"] == code {
                break incoming;
            }
            ensure!(announcements.len() < 32, "announcement cap");
            announcements.push(incoming);
        };
        if !announcements.is_empty() {
            v["announcements"] = json!(announcements);
        }
        if code == 1 {
            self.authenticated = v["success"] == true;
        }
        Ok(v)
    }
    async fn idle(&mut self, s: &mut Stream) -> Result<Option<Value>> {
        Ok(Some(response(
            &self.frame.sized(s, 4, MAX_COMMAND, length).await?,
        )?))
    }
}
