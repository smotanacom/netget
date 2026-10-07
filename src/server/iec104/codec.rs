//! IEC 60870-5-104 APCI state and selected ASDUs (single point, float, commands, GI).
use crate::server::ics_support::{number, DeviceSession, ScannerSession};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{collections::VecDeque, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
};
pub const MAX_FRAME: usize = 255;
pub const T1: Duration = Duration::from_secs(15);
pub const T3: Duration = Duration::from_secs(20);
pub const K: usize = 12;
pub const W: usize = 8;
pub fn u_frame(code: u8) -> Vec<u8> {
    vec![0x68, 4, code, 0, 0, 0]
}
fn address(b: &[u8]) -> u32 {
    b[0] as u32 | ((b[1] as u32) << 8) | ((b[2] as u32) << 16)
}
fn asdu(typ: u8, cause: u8, origin: u8, ca: u16, ioa: u32, data: &[u8]) -> Vec<u8> {
    let mut b = vec![typ, 1, cause, origin];
    b.extend(ca.to_le_bytes());
    b.extend(&ioa.to_le_bytes()[..3]);
    b.extend(data);
    b
}
#[derive(Default)]
struct Channel {
    send: u16,
    recv: u16,
    acked: u16,
    pending: VecDeque<(u16, Instant)>,
    active: bool,
    buffer: Vec<u8>,
    test: Option<Instant>,
}
impl Channel {
    fn i(&mut self, asdu: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            self.active && asdu.len() <= 249 && self.pending.len() < K,
            "inactive link or send window exceeded"
        );
        let mut b = vec![0x68, (asdu.len() + 4) as u8];
        b.extend((self.send << 1).to_le_bytes());
        b.extend((self.recv << 1).to_le_bytes());
        b.extend(asdu);
        self.pending.push_back((self.send, Instant::now()));
        self.send = (self.send + 1) & 32767;
        Ok(b)
    }
    fn s(&self) -> Vec<u8> {
        let mut b = vec![0x68, 4, 1, 0];
        b.extend((self.recv << 1).to_le_bytes());
        b
    }
    fn ack(&mut self, ack: u16) -> Result<()> {
        let distance = ack.wrapping_sub(self.acked) & 32767;
        ensure!(
            distance as usize <= self.pending.len(),
            "invalid acknowledgment sequence"
        );
        for _ in 0..distance {
            self.pending.pop_front();
        }
        self.acked = ack;
        Ok(())
    }
    fn receive(&mut self, f: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
        ensure!(
            f.len() >= 6 && f[0] == 0x68 && f[1] as usize + 2 == f.len(),
            "APCI length/version"
        );
        if f[2] & 1 == 0 {
            ensure!(
                self.active && f.len() >= 15 && f[4] & 1 == 0,
                "I frame before STARTDT or truncated ASDU"
            );
            let sent = u16::from_le_bytes([f[2], f[3]]) >> 1;
            let ack = u16::from_le_bytes([f[4], f[5]]) >> 1;
            self.ack(ack)?;
            ensure!(sent == self.recv, "receive sequence mismatch");
            self.recv = (self.recv + 1) & 32767;
            return Ok((vec![], Some(f[6..].to_vec())));
        }
        ensure!(f.len() == 6, "control frame has ASDU");
        if f[2] & 3 == 1 {
            ensure!(f[2..4] == [1, 0] && f[4] & 1 == 0, "bad S frame");
            self.ack(u16::from_le_bytes([f[4], f[5]]) >> 1)?;
            return Ok((vec![], None));
        }
        ensure!(f[3..6] == [0; 3], "bad U frame reserved fields");
        let reply = match f[2] {
            7 => {
                self.active = true;
                u_frame(0x0b)
            }
            0x0b => {
                self.active = true;
                vec![]
            }
            0x13 => {
                self.active = false;
                u_frame(0x23)
            }
            0x23 => {
                self.active = false;
                vec![]
            }
            0x43 => u_frame(0x83),
            0x83 => {
                ensure!(self.test.is_some(), "unexpected TESTFR confirmation");
                self.test = None;
                vec![]
            }
            _ => bail!("unknown U frame"),
        };
        Ok((reply, None))
    }
    /// The accumulator survives cancellation. Deadlines cover both full and partial frames.
    async fn read(&mut self, s: &mut TcpStream) -> Result<Vec<u8>> {
        let mut idle = Instant::now() + T3;
        loop {
            if self.buffer.len() >= 2 {
                ensure!(
                    self.buffer[0] == 0x68 && (4..=253).contains(&self.buffer[1]),
                    "invalid APCI framing"
                );
                let n = self.buffer[1] as usize + 2;
                if self.buffer.len() >= n {
                    return Ok(self.buffer.drain(..n).collect());
                }
            }
            let deadline = self
                .pending
                .front()
                .map(|(_, t)| *t + T1)
                .unwrap_or(idle)
                .min(self.test.map(|t| t + T1).unwrap_or(idle));
            let mut b = [0; 255];
            match tokio::time::timeout_at(deadline, s.read(&mut b)).await {
                Ok(Ok(0)) => bail!("IEC 104 peer closed"),
                Ok(Ok(n)) => {
                    ensure!(
                        self.buffer.len() + n <= MAX_FRAME * 2,
                        "APCI accumulator limit"
                    );
                    self.buffer.extend(&b[..n]);
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    ensure!(
                        self.pending.is_empty() && self.test.is_none() && self.buffer.is_empty(),
                        "t1 acknowledgment/TESTFR or partial-frame deadline"
                    );
                    s.write_all(&u_frame(0x43)).await?;
                    self.test = Some(Instant::now());
                    idle = Instant::now() + T3;
                }
            }
        }
    }
}
fn decode_telemetry(a: &[u8]) -> Result<Vec<Value>> {
    ensure!(a.len() >= 6, "ASDU header");
    let typ = a[0];
    ensure!(matches!(typ, 1 | 13), "unsupported telemetry ASDU");
    let count = (a[1] & 127) as usize;
    ensure!((1..=64).contains(&count), "telemetry count outside bound");
    let sequential = a[1] & 128 != 0;
    let ca = u16::from_le_bytes([a[4], a[5]]);
    let mut pos = 6;
    let mut start = 0;
    let mut out = vec![];
    for i in 0..count {
        let ioa = if sequential && i > 0 {
            start + i as u32
        } else {
            ensure!(pos + 3 <= a.len(), "IOA truncated");
            let n = address(&a[pos..]);
            pos += 3;
            if i == 0 {
                start = n;
            }
            n
        };
        ensure!(ioa <= 0xffffff, "IOA sequence overflow");
        let (kind, value, quality) = if typ == 1 {
            ensure!(pos < a.len(), "single point truncated");
            let p = a[pos];
            pos += 1;
            ("binary", json!(p & 1 != 0), p & 0xf0)
        } else {
            ensure!(pos + 5 <= a.len(), "float point truncated");
            let p = f32::from_le_bytes(a[pos..pos + 4].try_into()?);
            ensure!(p.is_finite(), "nonfinite telemetry");
            let q = a[pos + 4];
            pos += 5;
            ("analog", json!(p), q)
        };
        out.push(json!({"kind":kind,"value":value,"quality":quality,"common_address":ca,"ioa":ioa,"cause":a[2]&63}));
    }
    ensure!(pos == a.len(), "ASDU trailing data");
    Ok(out)
}
fn point(p: &Value, ca: u16, cause: u8) -> Result<Vec<u8>> {
    let ioa = number(p, "ioa", 0xffffff)? as u32;
    let quality = p
        .get("quality")
        .map(|v| v.as_u64().context("quality integer"))
        .transpose()?
        .unwrap_or(0);
    ensure!(quality <= 255, "quality byte");
    match p["kind"].as_str() {
        Some("binary") => {
            let v = p["value"].as_bool().context("binary boolean")?;
            Ok(asdu(
                1,
                cause,
                0,
                ca,
                ioa,
                &[(quality as u8 & 0xf0) | u8::from(v)],
            ))
        }
        Some("analog") => {
            let n = p["value"].as_f64().context("analog number")?;
            ensure!(n.is_finite() && (n as f32).is_finite(), "nonfinite analog");
            let mut b = (n as f32).to_le_bytes().to_vec();
            b.push(quality as u8);
            Ok(asdu(13, cause, 0, ca, ioa, &b))
        }
        _ => bail!("unsupported point kind"),
    }
}
#[derive(Default)]
pub struct Device {
    channel: Channel,
    request: Vec<u8>,
}
#[async_trait::async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut TcpStream) -> Result<Vec<u8>> {
        self.channel.read(s).await
    }
    fn receive(&mut self, f: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        let (auto, a) = self.channel.receive(f)?;
        let Some(a) = a else { return Ok((auto, None)) };
        ensure!(
            a.len() >= 9 && a[1] == 1,
            "ASDU expects one non-sequential request"
        );
        let ca = u16::from_le_bytes([a[4], a[5]]);
        let ioa = address(&a[6..]);
        let cause = a[2] & 63;
        self.request = a.clone();
        let mut r = json!({"common_address":ca,"ioa":ioa,"originator":a[3]});
        match a[0] {
            100 => {
                ensure!(
                    a.len() == 10 && ioa == 0 && cause == 6,
                    "invalid interrogation"
                );
                r["operation"] = json!("interrogate");
                r["qualifier"] = json!(a[9]);
            }
            102 => {
                ensure!(a.len() == 9 && cause == 5, "invalid read command");
                r["operation"] = json!("read");
            }
            45 => {
                ensure!(
                    a.len() == 10 && cause == 6 && a[9] & 2 == 0,
                    "invalid single command"
                );
                r["operation"] = json!("command");
                r["value"] = json!(a[9] & 1 != 0);
                r["select"] = json!(a[9] & 0x80 != 0);
            }
            _ => {
                let mut refused = a;
                refused[2] = 44;
                return Ok((self.channel.i(&refused)?, None));
            }
        }
        Ok((auto, Some(r)))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let ca = number(r, "common_address", 65535)? as u16;
        let mut out = vec![];
        if r["operation"] == "command" {
            let accepted = a
                .is_some_and(|a| a["type"] == "iec104_command_result" && a["accepted"] == true)
                && r["select"] != true;
            let mut response = self.request.clone();
            response[2] = 7 | if accepted { 0 } else { 0x40 };
            out.extend(self.channel.i(&response)?);
            if accepted {
                response[2] = 10;
                out.extend(self.channel.i(&response)?);
            }
            return Ok(out);
        }
        let points = a
            .filter(|a| a["type"] == "iec104_measurements")
            .and_then(|a| a["points"].as_array());
        let good =
            points.is_some_and(|p| p.len() <= 8 && p.iter().all(|p| point(p, ca, 20).is_ok()));
        if r["operation"] == "interrogate" {
            let mut confirmation = self.request.clone();
            let accepted = good && r["qualifier"] == 20;
            confirmation[2] = 7 | if accepted { 0 } else { 0x40 };
            out.extend(self.channel.i(&confirmation)?);
            if accepted {
                for p in points.expect("checked") {
                    out.extend(self.channel.i(&point(p, ca, 20)?)?);
                }
                confirmation[2] = 10;
                out.extend(self.channel.i(&confirmation)?);
            }
        } else if good {
            let selected = points
                .expect("checked")
                .iter()
                .find(|p| p["ioa"] == r["ioa"]);
            if let Some(p) = selected {
                out.extend(self.channel.i(&point(p, ca, 5)?)?);
            } else {
                let mut response = self.request.clone();
                response[2] = 47;
                out.extend(self.channel.i(&response)?);
            }
        } else {
            let mut response = self.request.clone();
            response[2] = 47;
            out.extend(self.channel.i(&response)?);
        }
        Ok(out)
    }
}
pub fn validate(a: &Value) -> Result<()> {
    number(a, "common_address", 65535)?;
    match a["type"].as_str() {
        Some("iec104_interrogate") => {}
        Some("iec104_read") => {
            number(a, "ioa", 0xffffff)?;
        }
        Some("iec104_command") => {
            number(a, "ioa", 0xffffff)?;
            ensure!(
                a["value"].is_boolean(),
                "single command value must be boolean"
            );
        }
        _ => bail!("unknown IEC 104 action"),
    };
    Ok(())
}
#[derive(Default)]
pub struct Scanner {
    channel: Channel,
}
#[async_trait::async_trait]
impl ScannerSession for Scanner {
    async fn idle(&mut self, s: &mut TcpStream) -> Result<Option<Value>> {
        let f = self.channel.read(s).await?;
        let (auto, a) = self.channel.receive(&f)?;
        if !auto.is_empty() {
            s.write_all(&auto).await?;
        }
        if let Some(a) = a {
            s.write_all(&self.channel.s()).await?;
            Ok(Some(json!({"success":true,"points":decode_telemetry(&a)?})))
        } else {
            Ok(None)
        }
    }
    async fn open(&mut self, s: &mut TcpStream) -> Result<()> {
        s.write_all(&u_frame(7)).await?;
        loop {
            let f = self.channel.read(s).await?;
            let (auto, a) = self.channel.receive(&f)?;
            ensure!(a.is_none(), "ASDU before STARTDT");
            if !auto.is_empty() {
                s.write_all(&auto).await?;
            }
            if self.channel.active {
                return Ok(());
            }
        }
    }
    async fn exchange(&mut self, s: &mut TcpStream, a: &Value) -> Result<Value> {
        validate(a)?;
        let ca = number(a, "common_address", 65535)? as u16;
        let mut points = vec![];
        let (typ, request) = match a["type"].as_str() {
            Some("iec104_interrogate") => (100, asdu(100, 6, 0, ca, 0, &[20])),
            Some("iec104_read") => (
                102,
                asdu(102, 5, 0, ca, number(a, "ioa", 0xffffff)? as u32, &[]),
            ),
            _ => (
                45,
                asdu(
                    45,
                    6,
                    0,
                    ca,
                    number(a, "ioa", 0xffffff)? as u32,
                    &[u8::from(a["value"].as_bool().context("command value")?)],
                ),
            ),
        };
        s.write_all(&self.channel.i(&request)?).await?;
        loop {
            let f = self.channel.read(s).await?;
            let (auto, response) = self.channel.receive(&f)?;
            if !auto.is_empty() {
                s.write_all(&auto).await?;
            }
            let Some(response) = response else { continue };
            s.write_all(&self.channel.s()).await?;
            ensure!(
                u16::from_le_bytes([response[4], response[5]]) == ca,
                "common address mismatch"
            );
            let cause = response[2] & 63;
            if matches!(cause, 44..=47) || response[2] & 0x40 != 0 {
                return Ok(json!({"success":false,"cause":cause}));
            }
            if matches!(response[0], 1 | 13) {
                points.extend(decode_telemetry(&response)?);
                ensure!(points.len() <= 64, "telemetry response bound");
                if typ == 102 {
                    return Ok(json!({"success":true,"points":points}));
                }
                continue;
            }
            ensure!(
                response[0] == typ && response[6..] == request[6..],
                "ASDU operation correlation"
            );
            if cause == 10 {
                return Ok(json!({"success":true,"points":points}));
            }
            ensure!(cause == 7, "unexpected confirmation cause");
        }
    }
}
