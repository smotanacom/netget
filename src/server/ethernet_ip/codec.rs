//! EtherNet/IP encapsulation, CPF and selected explicit CIP object services.
use crate::server::ics_support::{self as io, number, DeviceSession, ScannerSession};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use tokio::{io::AsyncWriteExt, net::TcpStream};
pub const MAX_FRAME: usize = 4096;
pub const PORT: u16 = 44818;
fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
pub fn encapsulate(
    command: u16,
    session: u32,
    context: [u8; 8],
    status: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut p = vec![];
    p.extend(command.to_le_bytes());
    p.extend(
        u16::try_from(data.len())
            .expect("bounded payload")
            .to_le_bytes(),
    );
    p.extend(session.to_le_bytes());
    p.extend(status.to_le_bytes());
    p.extend(context);
    p.extend([0; 4]);
    p.extend(data);
    p
}
pub async fn read(s: &mut TcpStream) -> Result<Vec<u8>> {
    io::frame(s, 24, MAX_FRAME, |h| {
        ensure!(h[20..24] == [0; 4], "unsupported encapsulation options");
        Ok(24 + u16le(&h[2..]) as usize)
    })
    .await
}
fn context(f: &[u8]) -> [u8; 8] {
    f[12..20].try_into().expect("checked header")
}
/// Identity data uses an explicit NetGet simulator identity, rather than a vendor impersonation.
pub fn identity(addr: std::net::SocketAddr) -> Vec<u8> {
    let mut data = vec![1, 0];
    data.extend(2u16.to_be_bytes());
    data.extend(addr.port().to_be_bytes());
    data.extend(match addr.ip() {
        std::net::IpAddr::V4(v) => v.octets(),
        _ => [127, 0, 0, 1],
    });
    data.extend([0; 8]);
    data.extend(0u16.to_le_bytes());
    data.extend(0u16.to_le_bytes());
    data.extend(1u16.to_le_bytes());
    data.extend([1, 0]);
    data.extend(0u16.to_le_bytes());
    data.extend(1u32.to_le_bytes());
    let name = b"NetGet simulator";
    data.push(name.len() as u8);
    data.extend(name);
    data.push(3);
    let mut out = vec![1, 0, 0x0c, 0];
    out.extend((data.len() as u16).to_le_bytes());
    out.extend(data);
    out
}
fn cpf(cip: &[u8]) -> Vec<u8> {
    let mut p = vec![0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0xb2, 0];
    p.extend((cip.len() as u16).to_le_bytes());
    p.extend(cip);
    p
}
fn cip_payload(p: &[u8]) -> Result<&[u8]> {
    ensure!(
        p.len() >= 16
            && p[..4] == [0; 4]
            && u16le(&p[6..]) == 2
            && p[8..12] == [0; 4]
            && p[12..14] == [0xb2, 0],
        "unsupported CPF/interface"
    );
    let n = u16le(&p[14..]) as usize;
    ensure!(p.len() == 16 + n, "CPF length mismatch");
    Ok(&p[16..])
}
fn path(cip: &[u8]) -> Result<(u16, u16, Option<u16>, &[u8])> {
    ensure!(cip.len() >= 2, "short CIP request");
    let end = 2 + cip[1] as usize * 2;
    ensure!(end <= cip.len(), "short CIP path");
    let mut pos = 2;
    let mut c = None;
    let mut i = None;
    let mut a = None;
    while pos < end {
        let code = cip[pos];
        let (n, used) = match code {
            0x20 | 0x24 | 0x30 => {
                ensure!(pos + 2 <= end, "short logical segment");
                (cip[pos + 1] as u16, 2)
            }
            0x21 | 0x25 | 0x31 => {
                ensure!(
                    pos + 4 <= end && cip[pos + 1] == 0,
                    "short/padded logical segment"
                );
                (u16le(&cip[pos + 2..]), 4)
            }
            _ => bail!("unsupported CIP path segment"),
        };
        let slot = match code & 0xfc {
            0x20 => &mut c,
            0x24 => &mut i,
            0x30 => &mut a,
            _ => unreachable!(),
        };
        ensure!(slot.is_none(), "duplicate CIP path segment");
        *slot = Some(n);
        pos += used;
    }
    Ok((
        c.context("class missing")?,
        i.context("instance missing")?,
        a,
        &cip[end..],
    ))
}
fn cip_error(service: u8, status: u8) -> Vec<u8> {
    vec![service | 0x80, 0, status, 0]
}
fn encode_value(v: &Value) -> Result<Vec<u8>> {
    let kind = v["value_type"].as_str().context("value_type required")?;
    Ok(match kind {
        "uint8" => vec![number(v, "value", 255)? as u8],
        "uint16" => (number(v, "value", 65535)? as u16).to_le_bytes().to_vec(),
        "uint32" => (number(v, "value", u32::MAX as u64)? as u32)
            .to_le_bytes()
            .to_vec(),
        "int32" => {
            let n = v["value"].as_i64().context("signed value")?;
            i32::try_from(n)?.to_le_bytes().to_vec()
        }
        "real" => {
            let n = v["value"].as_f64().context("real value")?;
            ensure!(n.is_finite() && (n as f32).is_finite(), "nonfinite value");
            (n as f32).to_le_bytes().to_vec()
        }
        "string" => {
            let t = v["value"].as_str().context("string value")?;
            ensure!(
                t.is_ascii() && t.len() <= 255,
                "short ASCII string required"
            );
            let mut b = vec![t.len() as u8];
            b.extend(t.as_bytes());
            b
        }
        _ => bail!("unsupported attribute value_type"),
    })
}
fn decode_value(kind: &str, b: &[u8]) -> Result<Value> {
    Ok(match kind {
        "uint8" => {
            ensure!(b.len() == 1, "uint8 length");
            json!(b[0])
        }
        "uint16" => {
            ensure!(b.len() == 2, "uint16 length");
            json!(u16le(b))
        }
        "uint32" => {
            ensure!(b.len() == 4, "uint32 length");
            json!(u32le(b))
        }
        "int32" => {
            ensure!(b.len() == 4, "int32 length");
            json!(i32::from_le_bytes(b.try_into()?))
        }
        "real" => {
            ensure!(b.len() == 4, "real length");
            let n = f32::from_le_bytes(b.try_into()?);
            ensure!(n.is_finite(), "nonfinite real");
            json!(n)
        }
        "string" => {
            ensure!(
                !b.is_empty() && b.len() == 1 + b[0] as usize,
                "short string length"
            );
            json!(std::str::from_utf8(&b[1..])?)
        }
        _ => bail!("unsupported attribute value_type"),
    })
}
pub struct Device {
    session: u32,
    addr: std::net::SocketAddr,
    pub types: Vec<Value>,
    context: [u8; 8],
}
impl Default for Device {
    fn default() -> Self {
        Self {
            session: 0,
            types: vec![],
            context: [0; 8],
            addr: "127.0.0.1:44818".parse().expect("constant"),
        }
    }
}
#[async_trait::async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut TcpStream) -> Result<Vec<u8>> {
        self.addr = s.local_addr()?;
        read(s).await
    }
    fn receive(&mut self, f: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        ensure!(
            f.len() >= 24 && u32le(&f[8..]) == 0,
            "invalid encapsulation header"
        );
        let command = u16le(f);
        let session = u32le(&f[4..]);
        let payload = &f[24..];
        let ctx = context(f);
        let auto = |status, data: &[u8]| encapsulate(command, session, ctx, status, data);
        if matches!(command, 0x63 | 0x64 | 0x04) {
            ensure!(
                payload.is_empty() && session == 0,
                "discovery fields must be zero"
            );
            let data = match command {
                0x63 => identity(self.addr),
                0x64 => vec![0, 0],
                _ => {
                    let mut p = vec![1, 0, 0, 1, 20, 0, 1, 0, 0x20, 0];
                    p.extend(b"Communications\0\0");
                    p
                }
            };
            return Ok((auto(0, &data), None));
        }
        if command == 0x65 {
            if payload != [1, 0, 0, 0] || session != 0 {
                return Ok((auto(0x69, &[]), None));
            }
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
            self.session = NEXT
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                .max(1);
            return Ok((encapsulate(command, self.session, ctx, 0, payload), None));
        }
        if self.session == 0 || session != self.session {
            return Ok((auto(0x64, &[]), None));
        }
        if command == 0x66 {
            bail!("session unregistered");
        }
        if command != 0x6f {
            return Ok((auto(1, &[]), None));
        }
        let cip = cip_payload(payload)?;
        ensure!(!cip.is_empty(), "empty CIP");
        let (c, i, a, body) = match path(cip) {
            Ok(p) => p,
            Err(_) => return Ok((auto(0, &cpf(&cip_error(cip[0], 4))), None)),
        };
        if !matches!(cip[0], 0x0e | 0x10) {
            return Ok((auto(0, &cpf(&cip_error(cip[0], 8))), None));
        }
        let Some(attribute) = a else {
            return Ok((auto(0, &cpf(&cip_error(cip[0], 4))), None));
        };
        if cip[0] == 0x0e {
            ensure!(body.is_empty(), "unexpected GetAttributeSingle data");
        }
        let mut request = json!({"operation":if cip[0]==0x0e{"get"}else{"set"},"class":c,"instance":i,"attribute":attribute,"service":cip[0]});
        // CIP carries no type tag: a declared schema supplies the attribute type.
        self.context = ctx;
        if cip[0] == 0x10 {
            ensure!(body.len() <= 1024, "attribute too large");
            let configured = self.types.iter().find(|a| {
                a["class"] == c
                    && a["instance"] == i
                    && a["attribute"].as_u64() == Some(attribute as u64)
            });
            let kind = configured
                .and_then(|t| t["value_type"].as_str())
                .or(if c == 1 && i == 1 {
                    match attribute {
                        1..=5 => Some("uint16"),
                        6 => Some("uint32"),
                        7 => Some("string"),
                        8 => Some("uint8"),
                        _ => None,
                    }
                } else {
                    None
                });
            let Some(kind) = kind else {
                return Ok((auto(0, &cpf(&cip_error(cip[0], 0x14))), None));
            };
            let value = match decode_value(kind, body) {
                Ok(v) => v,
                Err(_) => return Ok((auto(0, &cpf(&cip_error(cip[0], 9))), None)),
            };
            request["value_type"] = json!(kind);
            request["value"] = value;
        }
        Ok((vec![], Some(request)))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let service = number(r, "service", 255)? as u8;
        let a = a.filter(|a| a["type"] == "ethernet_ip_reply");
        let mut cip = cip_error(service, 0x0f);
        if let Some(a) = a {
            if let Some(status) = a.get("status").filter(|v| !v.is_null()) {
                let n = status.as_u64().context("status integer")?;
                ensure!((1..=255).contains(&n), "error status 1..255");
                cip = cip_error(service, n as u8);
            } else if service == 0x0e {
                match encode_value(a) {
                    Ok(value) => {
                        cip = cip_error(service, 0);
                        cip.extend(value)
                    }
                    Err(_) => {}
                }
            } else if a["accepted"] == true {
                cip = cip_error(service, 0);
            }
        }
        Ok(encapsulate(0x6f, self.session, self.context, 0, &cpf(&cip)))
    }
}
pub fn validate(a: &Value) -> Result<()> {
    if a["type"] == "ethernet_ip_discover" {
        return Ok(());
    }
    ensure!(
        matches!(
            a["type"].as_str(),
            Some("ethernet_ip_get" | "ethernet_ip_set")
        ),
        "unknown explicit messaging action"
    );
    for field in ["class", "instance", "attribute"] {
        number(a, field, 65535)?;
    }
    let kind = a["value_type"].as_str().context("value_type required")?;
    ensure!(
        matches!(
            kind,
            "uint8" | "uint16" | "uint32" | "int32" | "real" | "string"
        ),
        "unsupported value_type"
    );
    if a["type"] == "ethernet_ip_set" {
        encode_value(a)?;
    }
    Ok(())
}
#[derive(Default)]
pub struct Scanner {
    session: u32,
    context: u64,
}
impl Scanner {
    async fn send(&mut self, s: &mut TcpStream, command: u16, payload: &[u8]) -> Result<Vec<u8>> {
        self.context = self.context.wrapping_add(1);
        let ctx = self.context.to_le_bytes();
        let session = if command == 0x63 { 0 } else { self.session };
        s.write_all(&encapsulate(command, session, ctx, 0, payload))
            .await?;
        let f = read(s).await?;
        ensure!(
            u16le(&f) == command && context(&f) == ctx,
            "encapsulation response correlation failed"
        );
        ensure!(
            u32le(&f[8..]) == 0,
            "encapsulation error {}",
            u32le(&f[8..])
        );
        if command != 0x65 {
            ensure!(u32le(&f[4..]) == session, "session mismatch");
        }
        Ok(f)
    }
}
#[async_trait::async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut TcpStream) -> Result<()> {
        let f = self.send(s, 0x65, &[1, 0, 0, 0]).await?;
        ensure!(f[24..] == [1, 0, 0, 0], "registration payload invalid");
        self.session = u32le(&f[4..]);
        ensure!(self.session != 0, "zero session handle");
        Ok(())
    }
    async fn exchange(&mut self, s: &mut TcpStream, a: &Value) -> Result<Value> {
        validate(a)?;
        if a["type"] == "ethernet_ip_discover" {
            let f = self.send(s, 0x63, &[]).await?;
            let p = &f[24..];
            ensure!(
                p.len() >= 39
                    && u16le(p) == 1
                    && u16le(&p[2..]) == 0x0c
                    && u16le(&p[4..]) as usize + 6 == p.len(),
                "invalid identity list"
            );
            let v = &p[6..];
            let len = v[32] as usize;
            ensure!(v.len() == 34 + len, "identity name length");
            return Ok(
                json!({"vendor":u16le(&v[18..]),"device_type":u16le(&v[20..]),"product":u16le(&v[22..]),"serial":u32le(&v[28..]),"name":std::str::from_utf8(&v[33..33+len])?}),
            );
        }
        let service = if a["type"] == "ethernet_ip_get" {
            0x0e
        } else {
            0x10
        };
        let mut path = vec![];
        for (key, segment) in [("class", 0x20), ("instance", 0x24), ("attribute", 0x30)] {
            let n = number(a, key, 65535)? as u16;
            if n <= 255 {
                path.extend([segment, n as u8]);
            } else {
                path.extend([segment + 1, 0]);
                path.extend(n.to_le_bytes());
            }
        }
        let mut cip = vec![service, (path.len() / 2) as u8];
        cip.extend(path);
        if service == 0x10 {
            cip.extend(encode_value(a)?);
        }
        let f = self.send(s, 0x6f, &cpf(&cip)).await?;
        let p = cip_payload(&f[24..])?;
        ensure!(
            p.len() >= 4 && p[0] == service | 0x80 && p[1] == 0 && p.len() >= 4 + 2 * p[3] as usize,
            "CIP response header"
        );
        if p[2] != 0 {
            return Ok(json!({"success":false,"status":p[2]}));
        }
        let body = &p[4 + 2 * p[3] as usize..];
        if service == 0x10 {
            ensure!(body.is_empty(), "unexpected write response data");
            Ok(json!({"success":true}))
        } else {
            Ok(
                json!({"success":true,"value":decode_value(a["value_type"].as_str().context("type")?,body)?}),
            )
        }
    }
}
