//! Bounded Gnutella 0.6 descriptor and three-way handshake codecs.
use crate::server::p2p_support::{DeviceSession, Framer, ReadStream, ScannerSession, Stream};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWriteExt};
pub const MAX_COMMAND: usize = 65536 + 23;
const HEADERS: &str =
    "User-Agent: NetGet/0.3\r\nX-Ultrapeer: False\r\nAccept: application/x-gnutella-packets\r\n";
fn size(b: &[u8]) -> Result<usize> {
    let n = u32::from_le_bytes(b[19..23].try_into()?) as usize;
    ensure!(
        n <= 65536 && b[17] <= 16 && b[18] <= 16 && b[17] as u16 + b[18] as u16 <= 16,
        "invalid descriptor limits"
    );
    Ok(n + 23)
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}
fn guid(s: &str) -> Result<[u8; 16]> {
    ensure!(
        s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "GUID requires 32 hex digits"
    );
    let mut out = [0; 16];
    for (i, v) in out.iter_mut().enumerate() {
        *v = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}
fn text(s: &str) -> Result<()> {
    ensure!(
        s.len() <= 4096 && !s.contains(['\0', '\r', '\n']),
        "descriptor string"
    );
    Ok(())
}
fn endpoint(v: &Value) -> Result<Vec<u8>> {
    let port = v["port"].as_u64().unwrap_or(6346);
    ensure!((1..=65535).contains(&port), "port");
    let ip: std::net::Ipv4Addr = v["ip"].as_str().unwrap_or("127.0.0.1").parse()?;
    let mut b = (port as u16).to_le_bytes().to_vec();
    b.extend(ip.octets());
    Ok(b)
}
fn number(v: &Value, k: &str) -> Result<u32> {
    let n = v[k].as_u64().unwrap_or(0);
    ensure!(n <= u32::MAX as u64, "{k}");
    Ok(n as u32)
}
fn packet(id: [u8; 16], kind: u8, ttl: u8, hops: u8, data: Vec<u8>) -> Result<Vec<u8>> {
    ensure!(
        data.len() <= 65536 && ttl <= 16 && hops <= 16 && ttl as u16 + hops as u16 <= 16,
        "descriptor cap"
    );
    let mut b = id.to_vec();
    b.extend([kind, ttl, hops]);
    b.extend((data.len() as u32).to_le_bytes());
    b.extend(data);
    Ok(b)
}
fn command(a: &Value) -> Result<(u8, Vec<u8>)> {
    match a["type"].as_str().context("type")? {
        "gnutella_ping" => Ok((0, vec![])),
        "gnutella_query" => {
            let s = a["query"].as_str().context("query")?;
            ensure!(!s.is_empty(), "empty query");
            text(s)?;
            let mut b = vec![0, 0];
            b.extend(s.as_bytes());
            b.push(0);
            Ok((0x80, b))
        }
        "gnutella_push" => {
            let mut b = guid(a["servent_id"].as_str().context("servent_id")?)?.to_vec();
            b.extend(number(a, "index")?.to_le_bytes());
            let endpoint = endpoint(a)?;
            b.extend(&endpoint[2..]);
            b.extend(&endpoint[..2]);
            Ok((0x40, b))
        }
        _ => bail!("unknown Gnutella action"),
    }
}
pub fn validate(v: &Value) -> Result<()> {
    command(v)?;
    Ok(())
}
fn cstring<'a>(b: &'a [u8], pos: &mut usize) -> Result<&'a str> {
    let suffix = b.get(*pos..).context("truncated string")?;
    let n = suffix
        .iter()
        .position(|b| *b == 0)
        .context("unterminated string")?;
    let s = std::str::from_utf8(&suffix[..n])?;
    text(s)?;
    *pos += n + 1;
    Ok(s)
}
fn decode(b: &[u8]) -> Result<Value> {
    ensure!(
        b.len() >= 23 && size(&b[..23])? == b.len(),
        "descriptor length mismatch"
    );
    let p = &b[23..];
    let mut v = json!({"guid":hex(&b[..16]),"kind":b[16],"ttl":b[17],"hops":b[18]});
    match b[16] {
        0 => {
            ensure!(
                p.is_empty() || p.first() == Some(&0xc3),
                "invalid ping extension"
            );
            v["operation"] = json!("ping");
        }
        1 => {
            ensure!(
                p.len() >= 14 && (p.len() == 14 || p.get(14) == Some(&0xc3)),
                "invalid pong size/extension"
            );
            v["operation"] = json!("pong");
            v["port"] = json!(u16::from_le_bytes(p[..2].try_into()?));
            v["ip"] = json!(std::net::Ipv4Addr::new(p[2], p[3], p[4], p[5]).to_string());
            v["files"] = json!(u32::from_le_bytes(p[6..10].try_into()?));
            v["kilobytes"] = json!(u32::from_le_bytes(p[10..14].try_into()?));
        }
        0x80 => {
            ensure!(p.len() >= 3, "query size");
            let mut pos = 2;
            v["operation"] = json!("query");
            v["query"] = json!(cstring(p, &mut pos)?);
            if pos < p.len() {
                v["opaque_extension_bytes"] = json!(p.len() - pos);
            }
        }
        0x81 => {
            ensure!(p.len() >= 27 && p[0] <= 32, "query-hit size/count");
            v["operation"] = json!("query_hit");
            v["port"] = json!(u16::from_le_bytes(p[1..3].try_into()?));
            v["ip"] = json!(std::net::Ipv4Addr::new(p[3], p[4], p[5], p[6]).to_string());
            v["speed"] = json!(u32::from_le_bytes(p[7..11].try_into()?));
            let mut pos = 11;
            let entries = &p[..p.len() - 16];
            let mut rows = vec![];
            for _ in 0..p[0] {
                let fixed = entries.get(pos..pos + 8).context("truncated hit")?;
                let index = u32::from_le_bytes(fixed[..4].try_into()?);
                let size = u32::from_le_bytes(fixed[4..].try_into()?);
                pos += 8;
                let name = cstring(entries, &mut pos)?;
                let urn = cstring(entries, &mut pos)?;
                rows.push(json!({"index":index,"size":size,"name":name,"urn":urn}));
            }
            if pos < entries.len() {
                v["opaque_extension_bytes"] = json!(entries.len() - pos);
            }
            v["results"] = json!(rows);
            v["servent_id"] = json!(hex(&p[p.len() - 16..]));
        }
        0x40 => {
            ensure!(p.len() == 26, "push size");
            v["operation"] = json!("push");
            v["servent_id"] = json!(hex(&p[..16]));
            v["index"] = json!(u32::from_le_bytes(p[16..20].try_into()?));
            v["ip"] = json!(std::net::Ipv4Addr::new(p[20], p[21], p[22], p[23]).to_string());
            v["port"] = json!(u16::from_le_bytes(p[24..26].try_into()?));
        }
        _ => v["operation"] = json!("extension"),
    };
    Ok(v)
}
async fn headers<R: AsyncRead + Unpin>(f: &mut Framer, s: &mut R, expected: &str) -> Result<()> {
    let first = f.delimited(s, b'\n', 8192).await?;
    ensure!(
        first == format!("{expected}\r\n").as_bytes(),
        "Gnutella handshake refused: {}",
        String::from_utf8_lossy(&first).trim()
    );
    let mut total = first.len();
    for _ in 0..32 {
        let line = f.delimited(s, b'\n', 8192).await?;
        total += line.len();
        ensure!(total <= 8192, "header cap");
        if line == b"\r\n" {
            return Ok(());
        }
        let text = std::str::from_utf8(&line)?;
        ensure!(
            text.ends_with("\r\n") && text.contains(':') && !text.contains('\0'),
            "invalid handshake header"
        );
        ensure!(
            !text.to_ascii_lowercase().starts_with("content-encoding:"),
            "compressed descriptors unsupported"
        );
    }
    bail!("header count cap")
}
#[derive(Default)]
pub struct Device {
    frame: Framer,
    phase: u8,
}
#[async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut ReadStream) -> Result<Vec<u8>> {
        if self.phase < 2 {
            headers(
                &mut self.frame,
                s,
                if self.phase == 0 {
                    "GNUTELLA CONNECT/0.6"
                } else {
                    "GNUTELLA/0.6 200 OK"
                },
            )
            .await?;
            Ok(vec![])
        } else {
            self.frame.sized(s, 23, MAX_COMMAND, size).await
        }
    }
    fn receive(&mut self, b: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        if self.phase < 2 {
            self.phase += 1;
            return Ok((
                if self.phase == 1 {
                    format!("GNUTELLA/0.6 200 OK\r\n{HEADERS}\r\n").into_bytes()
                } else {
                    vec![]
                },
                None,
            ));
        }
        let v = decode(b)?;
        if v["operation"] == "extension" {
            return Ok((vec![], None));
        }
        Ok((vec![], Some(v)))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let Some(a) = a else { return Ok(vec![]) };
        let id = guid(r["guid"].as_str().context("guid")?)?;
        let (kind, data) = match r["operation"].as_str() {
            Some("ping") => {
                let mut b = endpoint(a)?;
                b.extend(number(a, "files")?.to_le_bytes());
                b.extend(number(a, "kilobytes")?.to_le_bytes());
                (1, b)
            }
            Some("query") => {
                let rows = a["results"].as_array().cloned().unwrap_or_default();
                ensure!(rows.len() <= 32, "query hits cap");
                let mut b = vec![rows.len() as u8];
                b.extend(endpoint(a)?);
                b.extend(number(a, "speed")?.to_le_bytes());
                for row in rows {
                    b.extend(number(&row, "index")?.to_le_bytes());
                    b.extend(number(&row, "size")?.to_le_bytes());
                    for k in ["name", "urn"] {
                        let s = row[k].as_str().unwrap_or("");
                        text(s)?;
                        b.extend(s.as_bytes());
                        b.push(0);
                    }
                }
                b.extend([0; 16]);
                (0x81, b)
            }
            _ => return Ok(vec![]),
        };
        packet(id, kind, 1, 0, data)
    }
}
#[derive(Default)]
pub struct Scanner {
    frame: Framer,
}
#[async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut Stream) -> Result<()> {
        s.write_all(format!("GNUTELLA CONNECT/0.6\r\n{HEADERS}\r\n").as_bytes())
            .await?;
        headers(&mut self.frame, s, "GNUTELLA/0.6 200 OK").await?;
        s.write_all(format!("GNUTELLA/0.6 200 OK\r\n{HEADERS}\r\n").as_bytes())
            .await?;
        Ok(())
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        use rand::RngCore;
        let mut id = [0; 16];
        rand::thread_rng().fill_bytes(&mut id);
        let (kind, payload) = command(a)?;
        s.write_all(&packet(id, kind, 1, 0, payload)?).await?;
        Ok(json!({"sent":true,"guid":hex(&id)}))
    }
    async fn idle(&mut self, s: &mut Stream) -> Result<Option<Value>> {
        Ok(Some(decode(
            &self.frame.sized(s, 23, MAX_COMMAND, size).await?,
        )?))
    }
}
