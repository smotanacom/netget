//! Selected ISO-on-TCP S7comm jobs. Values are byte-sized data-area elements.
use crate::server::ics_support::{self as io, number, DeviceSession, ScannerSession};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use tokio::{io::AsyncWriteExt, net::TcpStream};

pub const MAX_FRAME: usize = 487;
pub const MAX_ITEMS: usize = 16;
pub fn tpkt(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len() + 4).expect("bounded TPKT");
    let mut out = vec![3, 0];
    out.extend(len.to_be_bytes());
    out.extend(data);
    out
}
pub async fn read(stream: &mut TcpStream) -> Result<Vec<u8>> {
    io::frame(stream, 4, MAX_FRAME, |h| {
        ensure!(h[0..2] == [3, 0], "invalid TPKT version/reserved");
        Ok(u16::from_be_bytes([h[2], h[3]]) as usize)
    })
    .await
}
fn s7(reference: u16, parameters: &[u8], data: &[u8], response: bool) -> Vec<u8> {
    let mut p = vec![2, 0xf0, 0x80, 0x32, if response { 3 } else { 1 }, 0, 0];
    p.extend(reference.to_be_bytes());
    p.extend(
        u16::try_from(parameters.len())
            .expect("bounded params")
            .to_be_bytes(),
    );
    p.extend(
        u16::try_from(data.len())
            .expect("bounded data")
            .to_be_bytes(),
    );
    if response {
        p.extend([0, 0]);
    }
    p.extend(parameters);
    p.extend(data);
    tpkt(&p)
}
fn parts(frame: &[u8], response: bool) -> Result<(u16, &[u8], &[u8])> {
    ensure!(
        frame.len() >= if response { 19 } else { 17 },
        "short S7 header"
    );
    ensure!(
        frame[4..7] == [2, 0xf0, 0x80]
            && frame[7] == 0x32
            && frame[8] == if response { 3 } else { 1 },
        "unsupported COTP/S7 PDU"
    );
    ensure!(frame[9..11] == [0, 0], "S7 reserved field");
    let pl = u16::from_be_bytes([frame[13], frame[14]]) as usize;
    let dl = u16::from_be_bytes([frame[15], frame[16]]) as usize;
    let start = if response {
        ensure!(frame[17..19] == [0, 0], "S7 header error");
        19
    } else {
        17
    };
    ensure!(start + pl + dl == frame.len(), "S7 lengths disagree");
    Ok((
        u16::from_be_bytes([frame[11], frame[12]]),
        &frame[start..start + pl],
        &frame[start + pl..],
    ))
}
fn area_name(n: u8) -> Result<&'static str> {
    match n {
        0x81 => Ok("inputs"),
        0x82 => Ok("outputs"),
        0x83 => Ok("markers"),
        0x84 => Ok("db"),
        _ => bail!("unsupported data area"),
    }
}
fn area_id(n: &str) -> Result<u8> {
    match n {
        "inputs" => Ok(0x81),
        "outputs" => Ok(0x82),
        "markers" => Ok(0x83),
        "db" => Ok(0x84),
        _ => bail!("unknown data area"),
    }
}

#[derive(Default)]
pub struct Device {
    cotp: bool,
    setup: bool,
    pdu: usize,
}
#[async_trait::async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut TcpStream) -> Result<Vec<u8>> {
        read(s).await
    }
    fn receive(&mut self, f: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        if !self.cotp {
            ensure!(
                f.len() >= 11 && f[5] == 0xe0 && f[4] as usize + 5 == f.len(),
                "expected COTP connect"
            );
            ensure!(f[10] == 0, "unsupported COTP class");
            let mut pos = 11;
            while pos < f.len() {
                ensure!(pos + 2 <= f.len(), "short COTP option");
                let n = f[pos + 1] as usize;
                ensure!(pos + 2 + n <= f.len(), "short COTP option value");
                pos += 2 + n;
            }
            let mut cc = f[4..].to_vec();
            cc[1] = 0xd0;
            cc[2..4].copy_from_slice(&f[8..10]);
            cc[4..6].copy_from_slice(&[0, 1]);
            self.cotp = true;
            return Ok((tpkt(&cc), None));
        }
        let (reference, p, d) = parts(f, false)?;
        ensure!(!p.is_empty(), "missing S7 function");
        if p[0] == 0xf0 {
            ensure!(
                p.len() == 8 && d.is_empty() && p[1] == 0,
                "invalid setup communication"
            );
            let offered = u16::from_be_bytes([p[6], p[7]]) as usize;
            ensure!(offered >= 240, "PDU too small");
            self.pdu = offered.min(480);
            self.setup = true;
            let mut answer = vec![0xf0, 0, 0, 1, 0, 1];
            answer.extend((self.pdu as u16).to_be_bytes());
            return Ok((s7(reference, &answer, &[], true), None));
        }
        ensure!(
            self.setup && f.len() <= self.pdu + 7,
            "S7 setup required or negotiated PDU exceeded"
        );
        if !matches!(p[0], 4 | 5) {
            return Ok((
                s7(reference, &[p[0]], &[], true)
                    .into_iter()
                    .enumerate()
                    .map(|(i, b)| {
                        if i == 17 {
                            0x81
                        } else if i == 18 {
                            0x04
                        } else {
                            b
                        }
                    })
                    .collect(),
                None,
            ));
        }
        ensure!(p.len() >= 2, "short S7 item count");
        let count = p[1] as usize;
        ensure!(
            (1..=MAX_ITEMS).contains(&count) && p.len() == 2 + 12 * count,
            "invalid S7 item list"
        );
        let mut items = Vec::new();
        let mut offset = 0;
        for item in p[2..].chunks_exact(12) {
            ensure!(
                item[..4] == [0x12, 10, 0x10, 2],
                "only S7ANY byte reads/writes are supported"
            );
            let amount = u16::from_be_bytes([item[4], item[5]]) as usize;
            ensure!((1..=200).contains(&amount), "item length outside 1..200");
            let db = u16::from_be_bytes([item[6], item[7]]);
            let area = area_name(item[8])?;
            let bit_address = ((item[9] as u32) << 16) | ((item[10] as u32) << 8) | item[11] as u32;
            ensure!(bit_address % 8 == 0, "byte address must be aligned");
            ensure!(area == "db" || db == 0, "non-DB area has DB number");
            let mut v = json!({"area":area,"db":db,"start":bit_address/8,"count":amount});
            if p[0] == 5 {
                ensure!(
                    offset + 4 <= d.len() && d[offset] == 0 && d[offset + 1] == 4,
                    "invalid write data header"
                );
                let bits = u16::from_be_bytes([d[offset + 2], d[offset + 3]]) as usize;
                ensure!(
                    bits == amount * 8 && offset + 4 + amount <= d.len(),
                    "write data length disagrees"
                );
                v["values"] = json!(d[offset + 4..offset + 4 + amount]);
                offset += 4 + amount;
                if amount % 2 == 1 && items.len() + 1 < count {
                    ensure!(d.get(offset) == Some(&0), "missing item padding");
                    offset += 1;
                }
            }
            items.push(v);
        }
        ensure!(offset == d.len(), "unexpected S7 job data");
        Ok((
            vec![],
            Some(
                json!({"operation":if p[0]==4{"read"}else{"write"},"reference":reference,"items":items}),
            ),
        ))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let items = r["items"].as_array().context("items")?;
        let read = r["operation"] == "read";
        let answers = a
            .filter(|a| a["type"] == "s7comm_reply")
            .and_then(|a| a["items"].as_array())
            .filter(|a| a.len() == items.len());
        let mut data = vec![];
        for (i, item) in items.iter().enumerate() {
            let supplied = answers.and_then(|a| a.get(i));
            let error = supplied.and_then(|v| v["error"].as_str());
            let code = match error {
                Some("address") => 5,
                Some("unsupported") => 6,
                Some("denied") => 3,
                Some(_) => 10,
                None => 0xff,
            };
            if read {
                let vals = supplied
                    .and_then(|v| v["values"].as_array())
                    .filter(|v| v.len() == item["count"].as_u64().unwrap_or(0) as usize);
                let valid = code == 0xff
                    && vals.is_some_and(|v| v.iter().all(|n| n.as_u64().is_some_and(|n| n <= 255)));
                if valid {
                    let vals = vals.expect("checked");
                    data.extend([0xff, 4]);
                    data.extend((u16::try_from(vals.len() * 8)?).to_be_bytes());
                    data.extend(vals.iter().map(|n| n.as_u64().expect("checked") as u8));
                    if vals.len() % 2 == 1 && i + 1 < items.len() {
                        data.push(0);
                    }
                } else {
                    data.extend([if code == 0xff { 3 } else { code }, 0, 0, 0]);
                }
            } else {
                data.push(
                    if supplied.is_some_and(|v| v["accepted"] == true) && error.is_none() {
                        0xff
                    } else if code == 0xff {
                        3
                    } else {
                        code
                    },
                );
            }
        }
        ensure!(
            data.len() + 14 <= self.pdu,
            "response exceeds negotiated PDU"
        );
        Ok(s7(
            u16::try_from(number(r, "reference", 65535)?)?,
            &[if read { 4 } else { 5 }, items.len() as u8],
            &data,
            true,
        ))
    }
}

pub fn validate(a: &Value) -> Result<()> {
    ensure!(
        matches!(a["type"].as_str(), Some("s7comm_read" | "s7comm_write")),
        "unknown S7comm action"
    );
    area_id(a["area"].as_str().context("area required")?)?;
    number(a, "db", 65535)?;
    number(a, "start", 0x1fffff)?;
    if a["type"] == "s7comm_read" {
        let c = number(a, "count", 200)?;
        ensure!(c > 0, "count must be positive");
    } else {
        let values = a["values"].as_array().context("values required")?;
        ensure!(
            !values.is_empty() && values.len() <= 200,
            "write count outside 1..200"
        );
        for n in values {
            ensure!(n.as_u64().is_some_and(|n| n <= 255), "values must be bytes");
        }
    }
    ensure!(
        a["area"] == "db" || a["db"] == 0,
        "non-DB area must have db=0"
    );
    Ok(())
}
#[derive(Default)]
pub struct Scanner {
    reference: u16,
}
#[async_trait::async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut TcpStream) -> Result<()> {
        s.write_all(&tpkt(&[
            17, 0xe0, 0, 0, 0, 1, 0, 0xc0, 1, 10, 0xc1, 2, 1, 0, 0xc2, 2, 1, 2,
        ]))
        .await?;
        let cc = read(s).await?;
        ensure!(
            cc.len() >= 11 && cc[5] == 0xd0 && cc[6..8] == [0, 1],
            "COTP connection refused"
        );
        s.write_all(&s7(1, &[0xf0, 0, 0, 1, 0, 1, 1, 0xe0], &[], false))
            .await?;
        let f = read(s).await?;
        let (reference, p, d) = parts(&f, true)?;
        ensure!(
            reference == 1 && p.len() == 8 && p[0] == 0xf0 && d.is_empty(),
            "invalid setup response"
        );
        self.reference = 1;
        Ok(())
    }
    async fn exchange(&mut self, s: &mut TcpStream, a: &Value) -> Result<Value> {
        validate(a)?;
        self.reference = self.reference.wrapping_add(1);
        let write = a["type"] == "s7comm_write";
        let count = if write {
            a["values"].as_array().context("values")?.len()
        } else {
            number(a, "count", 200)? as usize
        };
        let mut p = vec![if write { 5 } else { 4 }, 1, 0x12, 10, 0x10, 2];
        p.extend((count as u16).to_be_bytes());
        p.extend((number(a, "db", 65535)? as u16).to_be_bytes());
        p.push(area_id(a["area"].as_str().context("area")?)?);
        let bits = (number(a, "start", 0x1fffff)? as u32) * 8;
        p.extend(&bits.to_be_bytes()[1..]);
        let mut data = vec![];
        if write {
            data.extend([0, 4]);
            data.extend(((count * 8) as u16).to_be_bytes());
            data.extend(
                a["values"]
                    .as_array()
                    .context("values")?
                    .iter()
                    .map(|v| v.as_u64().expect("validated") as u8),
            );
        }
        s.write_all(&s7(self.reference, &p, &data, false)).await?;
        let f = read(s).await?;
        let (reference, p, d) = parts(&f, true)?;
        ensure!(
            reference == self.reference && p == [if write { 5 } else { 4 }, 1],
            "S7 response correlation failed"
        );
        ensure!(!d.is_empty(), "missing item status");
        if d[0] != 0xff {
            ensure!(d.len() == if write { 1 } else { 4 }, "bad error item");
            return Ok(
                json!({"operation":if write{"write"}else{"read"},"status":d[0],"success":false}),
            );
        }
        if write {
            ensure!(d.len() == 1, "write response length");
            Ok(json!({"operation":"write","success":true}))
        } else {
            ensure!(
                d.len() == count + 4
                    && d[1] == 4
                    && u16::from_be_bytes([d[2], d[3]]) as usize == count * 8,
                "read response shape"
            );
            Ok(json!({"operation":"read","success":true,"values":&d[4..]}))
        }
    }
}
