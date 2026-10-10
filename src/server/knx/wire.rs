//! KNXnet/IP (tunnelling) and cEMI framing, KNX addresses, and the datapoint-type (DPT)
//! codec, shared by the server and the client. A group value is never handed to the handler
//! as bytes: it is decoded by its DPT when one is known, and offered under every plausible
//! reading when not.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

/// Largest KNXnet/IP frame accepted (the total-length field is 16 bits; real frames are
/// well under 300 bytes).
pub const MAX_FRAME: usize = 512;
/// Longest group value carried (DPT 16 is 14 bytes; extended frames allow 254).
pub const MAX_APDU_DATA: usize = 254;

pub const SEARCH_REQUEST: u16 = 0x0201;
pub const SEARCH_RESPONSE: u16 = 0x0202;
pub const DESCRIPTION_REQUEST: u16 = 0x0203;
pub const DESCRIPTION_RESPONSE: u16 = 0x0204;
pub const CONNECT_REQUEST: u16 = 0x0205;
pub const CONNECT_RESPONSE: u16 = 0x0206;
pub const CONNECTIONSTATE_REQUEST: u16 = 0x0207;
pub const CONNECTIONSTATE_RESPONSE: u16 = 0x0208;
pub const DISCONNECT_REQUEST: u16 = 0x0209;
pub const DISCONNECT_RESPONSE: u16 = 0x020A;
pub const TUNNELLING_REQUEST: u16 = 0x0420;
pub const TUNNELLING_ACK: u16 = 0x0421;

pub const E_NO_ERROR: u8 = 0x00;
pub const E_CONNECTION_TYPE: u8 = 0x22;
pub const E_CONNECTION_OPTION: u8 = 0x23;
pub const E_NO_MORE_CONNECTIONS: u8 = 0x24;
pub const E_CONNECTION_ID: u8 = 0x21;

pub const L_DATA_REQ: u8 = 0x11;
pub const L_DATA_CON: u8 = 0x2E;
pub const L_DATA_IND: u8 = 0x29;

/// A KNXnet/IP frame: its service type and body.
pub fn parse_frame(buf: &[u8]) -> Result<(u16, &[u8])> {
    ensure!(
        buf.len() >= 6 && buf.len() <= MAX_FRAME,
        "frame size out of range"
    );
    ensure!(buf[0] == 0x06 && buf[1] == 0x10, "not KNXnet/IP 1.0");
    let service = u16::from_be_bytes([buf[2], buf[3]]);
    let total = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    ensure!(
        total == buf.len(),
        "total length {total} does not match {}",
        buf.len()
    );
    Ok((service, &buf[6..]))
}

pub fn frame(service: u16, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0x06, 0x10];
    out.extend(service.to_be_bytes());
    out.extend(((body.len() + 6) as u16).to_be_bytes());
    out.extend(body);
    out
}

/// A host protocol address information block (UDP).
pub fn parse_hpai(b: &[u8]) -> Result<(SocketAddrV4, usize)> {
    ensure!(b.len() >= 8 && b[0] == 8, "malformed HPAI");
    ensure!(b[1] == 0x01, "only UDP HPAI is supported");
    let ip = Ipv4Addr::new(b[2], b[3], b[4], b[5]);
    let port = u16::from_be_bytes([b[6], b[7]]);
    Ok((SocketAddrV4::new(ip, port), 8))
}

pub fn hpai(addr: SocketAddr) -> Vec<u8> {
    let (ip, port) = match addr {
        SocketAddr::V4(a) => (*a.ip(), a.port()),
        SocketAddr::V6(_) => (Ipv4Addr::UNSPECIFIED, 0),
    };
    let mut out = vec![8, 0x01];
    out.extend(ip.octets());
    out.extend(port.to_be_bytes());
    out
}

/// Where to answer: the HPAI, or — when it is 0.0.0.0:0 (NAT mode) — the sender.
pub fn endpoint(hpai: SocketAddrV4, sender: SocketAddr) -> SocketAddr {
    if hpai.ip().is_unspecified() || hpai.port() == 0 {
        sender
    } else {
        SocketAddr::V4(hpai)
    }
}

pub fn format_group(a: u16) -> String {
    format!("{}/{}/{}", a >> 11, (a >> 8) & 0x07, a & 0xff)
}

pub fn parse_group(s: &str) -> Result<u16> {
    let parts: Vec<&str> = s.split('/').collect();
    let n = |p: &str| {
        p.trim()
            .parse::<u16>()
            .context("group address parts must be numbers")
    };
    Ok(match parts.as_slice() {
        [main, middle, sub] => {
            let (m, i, s) = (n(main)?, n(middle)?, n(sub)?);
            ensure!(
                m < 32 && i < 8 && s < 256,
                "group address out of range (31/7/255)"
            );
            (m << 11) | (i << 8) | s
        }
        [main, sub] => {
            let (m, s) = (n(main)?, n(sub)?);
            ensure!(m < 32 && s < 2048, "group address out of range (31/2047)");
            (m << 11) | s
        }
        _ => bail!("a group address is main/middle/sub, e.g. 1/2/3"),
    })
}

pub fn format_individual(a: u16) -> String {
    format!("{}.{}.{}", a >> 12, (a >> 8) & 0x0f, a & 0xff)
}

pub fn parse_individual(s: &str) -> Result<u16> {
    let parts: Vec<u16> = s
        .split('.')
        .map(|p| p.trim().parse::<u16>())
        .collect::<std::result::Result<_, _>>()
        .context("an individual address is area.line.device, e.g. 1.1.250")?;
    match parts.as_slice() {
        [a, l, d] if *a < 16 && *l < 16 && *d < 256 => Ok((a << 12) | (l << 8) | d),
        _ => bail!("an individual address is area.line.device (15.15.255)"),
    }
}

/// What a group telegram does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apci {
    Read,
    Response,
    Write,
}

impl Apci {
    pub fn name(self) -> &'static str {
        match self {
            Apci::Read => "read",
            Apci::Response => "response",
            Apci::Write => "write",
        }
    }
}

/// A group value on the wire: either the 6 bits inside the APCI, or bytes after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    Small(u8),
    Bytes(Vec<u8>),
}

/// A cEMI L_Data frame carrying a group telegram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Telegram {
    pub message_code: u8,
    pub source: u16,
    pub destination: u16,
    pub apci: Apci,
    pub data: Data,
}

/// Parse a cEMI L_Data frame. `Ok(None)` is a frame that is valid but not a group value
/// telegram (individual addressing, management services), which is acknowledged and ignored.
pub fn parse_cemi(b: &[u8]) -> Result<Option<Telegram>> {
    ensure!(b.len() >= 2, "short cEMI");
    let code = b[0];
    let add = b[1] as usize;
    let b = b
        .get(2 + add..)
        .context("cEMI additional info overruns the frame")?;
    ensure!(b.len() >= 7, "short L_Data");
    let ctrl2 = b[1];
    let source = u16::from_be_bytes([b[2], b[3]]);
    let destination = u16::from_be_bytes([b[4], b[5]]);
    let len = b[6] as usize;
    let tpdu = b.get(7..).context("short TPDU")?;
    ensure!(
        tpdu.len() == len + 1,
        "NPDU length {len} does not match the frame"
    );
    if ctrl2 & 0x80 == 0 || tpdu.len() < 2 {
        return Ok(None);
    }
    let apci = (u16::from(tpdu[0] & 0x03) << 8) | u16::from(tpdu[1]);
    let kind = match apci & 0x3c0 {
        0x000 => Apci::Read,
        0x040 => Apci::Response,
        0x080 => Apci::Write,
        _ => return Ok(None),
    };
    ensure!(tpdu.len() - 2 <= MAX_APDU_DATA, "group value too long");
    let data = if tpdu.len() == 2 {
        Data::Small((apci & 0x3f) as u8)
    } else {
        Data::Bytes(tpdu[2..].to_vec())
    };
    Ok(Some(Telegram {
        message_code: code,
        source,
        destination,
        apci: kind,
        data,
    }))
}

pub fn cemi(t: &Telegram) -> Vec<u8> {
    let apci: u16 = match t.apci {
        Apci::Read => 0x000,
        Apci::Response => 0x040,
        Apci::Write => 0x080,
    };
    let mut tpdu = vec![(apci >> 8) as u8 & 0x03, (apci & 0xff) as u8];
    match &t.data {
        Data::Small(v) => tpdu[1] |= v & 0x3f,
        Data::Bytes(b) => tpdu.extend(b),
    }
    let mut out = vec![t.message_code, 0x00];
    // Standard frame, no repeat, broadcast, low priority, ack request off; group address,
    // hop count 6.
    out.push(0xBC);
    out.push(0xE0);
    out.extend(t.source.to_be_bytes());
    out.extend(t.destination.to_be_bytes());
    out.push((tpdu.len() - 1) as u8);
    out.extend(tpdu);
    out
}

/// A tunnelling request body: connection header and cEMI.
pub fn tunnelling(channel: u8, seq: u8, cemi: &[u8]) -> Vec<u8> {
    let mut body = vec![4, channel, seq, 0];
    body.extend(cemi);
    frame(TUNNELLING_REQUEST, &body)
}

pub fn tunnelling_ack(channel: u8, seq: u8, status: u8) -> Vec<u8> {
    frame(TUNNELLING_ACK, &[4, channel, seq, status])
}

/// The main number of a DPT name: "9.001" → 9, "dpt1" → 1.
pub fn dpt_main(dpt: &str) -> Result<u16> {
    let d = dpt
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_alphabetic());
    d.split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .with_context(|| format!("unknown DPT {dpt}"))
}

/// The DPTs the codec knows.
pub const DPT_NOTE: &str = "1 (bool), 5 (0-255; 5.001 is 0-100 %), 6 (i8), 7 (u16), 8 (i16), 9 (2-byte float, e.g. °C), 12 (u32), 13 (i32), 14 (4-byte float), 16 (text, 14 chars), 17 (scene 0-63), 20 (u8 enum)";

fn dpt9_encode(v: f64) -> Result<[u8; 2]> {
    ensure!(
        v.is_finite() && (-671088.64..=670760.96).contains(&v),
        "DPT 9 range is -671088.64..670760.96"
    );
    let mut m = (v * 100.0).round() as i64;
    let mut e = 0u8;
    while !(-2048..=2047).contains(&m) {
        m = (m as f64 / 2.0).round() as i64;
        e += 1;
    }
    ensure!(e <= 15, "DPT 9 exponent overflow");
    let raw = (m & 0x0fff) as u16;
    let sign = if m < 0 { 0x80 } else { 0 };
    Ok([
        sign | (e << 3) | ((raw >> 8) as u8 & 0x07),
        (raw & 0xff) as u8,
    ])
}

fn dpt9_decode(b: [u8; 2]) -> f64 {
    let e = (b[0] >> 3) & 0x0f;
    let mut m = (i32::from(b[0] & 0x07) << 8) | i32::from(b[1]);
    if b[0] & 0x80 != 0 {
        m -= 2048;
    }
    let v = 0.01 * f64::from(m) * f64::from(1u32 << e);
    (v * 100.0).round() / 100.0
}

/// Encode a value as DPT `dpt`.
pub fn encode(dpt: &str, v: &Value) -> Result<Data> {
    let main = dpt_main(dpt)?;
    let int = |lo: i64, hi: i64| -> Result<i64> {
        let n = v
            .as_i64()
            .with_context(|| format!("DPT {dpt} takes an integer"))?;
        ensure!((lo..=hi).contains(&n), "DPT {dpt} range is {lo}..={hi}");
        Ok(n)
    };
    Ok(match main {
        1 => Data::Small(u8::from(match v {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_u64() == Some(1),
            _ => bail!("DPT 1 takes true or false"),
        })),
        5 if dpt.trim().ends_with(".001") => {
            let pct = v.as_f64().context("DPT 5.001 takes a percentage")?;
            ensure!((0.0..=100.0).contains(&pct), "DPT 5.001 range is 0..=100");
            Data::Bytes(vec![(pct * 255.0 / 100.0).round() as u8])
        }
        5 | 20 => Data::Bytes(vec![int(0, 255)? as u8]),
        17 => Data::Bytes(vec![int(0, 63)? as u8]),
        6 => Data::Bytes(vec![int(-128, 127)? as i8 as u8]),
        7 => Data::Bytes((int(0, 65535)? as u16).to_be_bytes().to_vec()),
        8 => Data::Bytes((int(-32768, 32767)? as i16).to_be_bytes().to_vec()),
        9 => Data::Bytes(dpt9_encode(v.as_f64().context("DPT 9 takes a number")?)?.to_vec()),
        12 => Data::Bytes((int(0, u32::MAX as i64)? as u32).to_be_bytes().to_vec()),
        13 => Data::Bytes(
            (int(i32::MIN as i64, i32::MAX as i64)? as i32)
                .to_be_bytes()
                .to_vec(),
        ),
        14 => Data::Bytes(
            (v.as_f64().context("DPT 14 takes a number")? as f32)
                .to_be_bytes()
                .to_vec(),
        ),
        16 => {
            let s = v.as_str().context("DPT 16 takes text")?;
            ensure!(
                s.len() <= 14 && s.is_ascii(),
                "DPT 16 is at most 14 ASCII characters"
            );
            let mut b = s.as_bytes().to_vec();
            b.resize(14, 0);
            Data::Bytes(b)
        }
        _ => bail!("DPT {dpt} is not supported; supported: {DPT_NOTE}"),
    })
}

/// Decode as DPT `dpt`, when the data has the right shape for it.
pub fn decode(dpt: &str, d: &Data) -> Result<Value> {
    let main = dpt_main(dpt)?;
    let bytes = |n: usize| -> Result<&[u8]> {
        match d {
            Data::Bytes(b) if b.len() == n => Ok(b),
            _ => bail!("DPT {dpt} carries {n} byte(s)"),
        }
    };
    Ok(match main {
        1 => match d {
            Data::Small(v) => json!(*v & 1 == 1),
            _ => bail!("DPT 1 is a 1-bit value"),
        },
        5 if dpt.trim().ends_with(".001") => {
            json!((f64::from(bytes(1)?[0]) * 100.0 / 255.0 * 10.0).round() / 10.0)
        }
        5 | 17 | 20 => json!(bytes(1)?[0]),
        6 => json!(bytes(1)?[0] as i8),
        7 => json!(u16::from_be_bytes(bytes(2)?.try_into()?)),
        8 => json!(i16::from_be_bytes(bytes(2)?.try_into()?)),
        9 => json!(dpt9_decode(bytes(2)?.try_into()?)),
        12 => json!(u32::from_be_bytes(bytes(4)?.try_into()?)),
        13 => json!(i32::from_be_bytes(bytes(4)?.try_into()?)),
        14 => json!(f32::from_be_bytes(bytes(4)?.try_into()?)),
        16 => {
            let b = bytes(14)?;
            let end = b.iter().position(|c| *c == 0).unwrap_or(14);
            json!(String::from_utf8_lossy(&b[..end]))
        }
        _ => bail!("DPT {dpt} is not supported"),
    })
}

/// Every reading the data's size allows, keyed by DPT, for a group address whose type is
/// not configured.
pub fn interpretations(d: &Data) -> Value {
    let mut out = Map::new();
    let candidates: &[&str] = match d {
        Data::Small(_) => &["1"],
        Data::Bytes(b) => match b.len() {
            1 => &["5", "5.001", "6"],
            2 => &["9", "7", "8"],
            4 => &["14", "12", "13"],
            14 => &["16"],
            _ => &[],
        },
    };
    for c in candidates {
        if let Ok(v) = decode(c, d) {
            out.insert(format!("dpt{c}"), v);
        }
    }
    if let Data::Small(v) = d {
        out.insert("raw_6bit".into(), json!(v));
    }
    Value::Object(out)
}
