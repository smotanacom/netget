//! Bounded DNP3/TCP selected subset: link/transport/app, class polls and typed objects.
use crate::server::ics_support::{self as io, number, DeviceSession, ScannerSession};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use tokio::{io::AsyncWriteExt, net::TcpStream};
pub const MAX_FRAME: usize = 292;
pub const MAX_APPLICATION: usize = 2048;
pub fn crc(b: &[u8]) -> u16 {
    let mut c = 0u16;
    for byte in b {
        c ^= *byte as u16;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xa6bc
            } else {
                c >> 1
            };
        }
    }
    !c
}
pub fn link(control: u8, destination: u16, source: u16, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= 250);
    let mut b = vec![5, 0x64, (5 + payload.len()) as u8, control];
    b.extend(destination.to_le_bytes());
    b.extend(source.to_le_bytes());
    b.extend(crc(&b).to_le_bytes());
    for block in payload.chunks(16) {
        b.extend(block);
        b.extend(crc(block).to_le_bytes());
    }
    b
}
pub async fn read(s: &mut TcpStream) -> Result<Vec<u8>> {
    io::frame(s, 10, MAX_FRAME, |h| {
        ensure!(h[..2] == [5, 0x64] && h[2] >= 5, "invalid DNP3 link header");
        ensure!(
            crc(&h[..8]) == u16::from_le_bytes([h[8], h[9]]),
            "DNP3 header CRC"
        );
        let n = h[2] as usize - 5;
        Ok(10 + n + 2 * n.div_ceil(16))
    })
    .await
}
fn payload(f: &[u8]) -> Result<Vec<u8>> {
    let n = f[2] as usize - 5;
    let mut offset = 10;
    let mut remaining = n;
    let mut b = vec![];
    while remaining > 0 {
        let amount = remaining.min(16);
        ensure!(offset + amount + 2 <= f.len(), "truncated DNP3 block");
        let chunk = &f[offset..offset + amount];
        ensure!(
            crc(chunk) == u16::from_le_bytes([f[offset + amount], f[offset + amount + 1]]),
            "DNP3 data CRC"
        );
        b.extend(chunk);
        offset += amount + 2;
        remaining -= amount;
    }
    ensure!(offset == f.len(), "trailing DNP3 data");
    Ok(b)
}
#[derive(Default)]
struct Assembly {
    bytes: Vec<u8>,
    next: u8,
    active: bool,
}
impl Assembly {
    fn push(&mut self, b: &[u8]) -> Result<Option<Vec<u8>>> {
        ensure!(!b.is_empty(), "missing transport control");
        let seq = b[0] & 63;
        if b[0] & 0x40 != 0 {
            ensure!(!self.active, "interleaved transport fragment");
            self.bytes.clear();
            self.active = true;
            self.next = seq;
        }
        ensure!(
            self.active && self.next == seq,
            "transport sequence mismatch"
        );
        ensure!(
            self.bytes.len() + b.len() - 1 <= MAX_APPLICATION,
            "application exceeds 2048 bytes"
        );
        self.bytes.extend(&b[1..]);
        self.next = (seq + 1) & 63;
        if b[0] & 0x80 != 0 {
            self.active = false;
            Ok(Some(std::mem::take(&mut self.bytes)))
        } else {
            Ok(None)
        }
    }
}
fn frames(app: &[u8], master: bool, local: u16, remote: u16, seq: &mut u8) -> Vec<u8> {
    let chunks = app.chunks(249);
    let count = chunks.len();
    let mut out = vec![];
    for (i, chunk) in chunks.enumerate() {
        let mut b =
            vec![*seq | if i == 0 { 0x40 } else { 0 } | if i + 1 == count { 0x80 } else { 0 }];
        b.extend(chunk);
        *seq = (*seq + 1) & 63;
        out.extend(link(if master { 0xc4 } else { 0x44 }, remote, local, &b));
    }
    out
}
fn read_headers(b: &[u8]) -> Result<Vec<Value>> {
    let mut groups = vec![];
    let mut offset = 0;
    while offset < b.len() {
        ensure!(offset + 3 <= b.len(), "short object header");
        let (g, v, q) = (b[offset], b[offset + 1], b[offset + 2]);
        offset += 3;
        ensure!(
            g == 60 && (1..=4).contains(&v) && q == 6,
            "only all-object class 0/1/2/3 polls are supported"
        );
        groups.push(json!(v - 1));
    }
    ensure!(
        !groups.is_empty() && groups.len() <= 4,
        "invalid class poll"
    );
    Ok(groups)
}
/// Selected polled event/static variations have explicit typed flags and optional 48-bit time.
fn encode_points(points: &[Value], event: bool) -> Result<Vec<u8>> {
    ensure!(points.len() <= 64, "at most 64 points per response");
    let mut b = vec![];
    for point in points {
        let kind = point["kind"].as_str().context("point kind")?;
        let timestamp = point.get("timestamp_ms").and_then(Value::as_u64);
        let timed = event && timestamp.is_some();
        let (g, v) = match (kind, event, timed) {
            ("binary", false, _) => (1, 2),
            ("binary", true, false) => (2, 1),
            ("binary", true, true) => (2, 2),
            ("analog", false, _) => (30, 5),
            ("analog", true, false) => (32, 5),
            ("analog", true, true) => (32, 7),
            ("counter", false, _) => (20, 1),
            ("counter", true, false) => (22, 1),
            ("counter", true, true) => (22, 5),
            _ => bail!("unknown point kind"),
        };
        let index = number(point, "index", 65535)? as u16;
        if event {
            b.extend([g, v, 0x28, 1, 0]);
            b.extend(index.to_le_bytes());
        } else {
            b.extend([g, v, 1]);
            b.extend(index.to_le_bytes());
            b.extend(index.to_le_bytes());
        }
        let flags = point
            .get("flags")
            .map(|n| n.as_u64().context("flags integer"))
            .transpose()?
            .unwrap_or(1);
        ensure!(flags <= 255, "flags outside byte range");
        match kind {
            "binary" => {
                let state = point["value"]
                    .as_bool()
                    .context("binary boolean required")?;
                b.push((flags as u8 & 0x7f) | if state { 0x80 } else { 0 });
            }
            "analog" => {
                let value = point["value"].as_f64().context("analog number required")?;
                ensure!(
                    value.is_finite() && (value as f32).is_finite(),
                    "nonfinite analog"
                );
                b.push(flags as u8);
                b.extend((value as f32).to_le_bytes());
            }
            "counter" => {
                b.push(flags as u8);
                b.extend((number(point, "value", u32::MAX as u64)? as u32).to_le_bytes());
            }
            _ => unreachable!(),
        }
        if timed {
            let ms = timestamp.expect("checked");
            ensure!(ms < 1u64 << 48, "timestamp exceeds 48 bits");
            b.extend(&ms.to_le_bytes()[..6]);
        }
    }
    ensure!(b.len() + 4 <= MAX_APPLICATION, "point response too large");
    Ok(b)
}
fn decode_points(b: &[u8]) -> Result<Vec<Value>> {
    let mut out = vec![];
    let mut offset = 0;
    while offset < b.len() {
        ensure!(offset + 3 <= b.len(), "short measurement header");
        let (g, v, q) = (b[offset], b[offset + 1], b[offset + 2]);
        offset += 3;
        let (count, start, prefix) = match q {
            0x28 => {
                ensure!(offset + 2 <= b.len(), "short count");
                let n = u16::from_le_bytes([b[offset], b[offset + 1]]) as usize;
                offset += 2;
                (n, 0, true)
            }
            0x17 => {
                ensure!(offset < b.len(), "short count");
                let n = b[offset] as usize;
                offset += 1;
                (n, 0, false)
            }
            0 => {
                ensure!(offset + 2 <= b.len(), "short range");
                let start = b[offset] as usize;
                let end = b[offset + 1] as usize;
                ensure!(start <= end, "bad object range");
                offset += 2;
                (end - start + 1, start, false)
            }
            1 => {
                ensure!(offset + 4 <= b.len(), "short range");
                let start = u16::from_le_bytes([b[offset], b[offset + 1]]) as usize;
                let end = u16::from_le_bytes([b[offset + 2], b[offset + 3]]) as usize;
                ensure!(start <= end, "bad object range");
                offset += 4;
                (end - start + 1, start, false)
            }
            _ => bail!("unsupported object qualifier"),
        };
        ensure!(
            count <= 128 && out.len() + count <= 128,
            "measurement count exceeds bound"
        );
        let (kind, width, timed) = match (g, v) {
            (1, 2) | (2, 1) => ("binary", 1, false),
            (2, 2) => ("binary", 1, true),
            (30, 5) | (32, 5) => ("analog", 5, false),
            (32, 7) => ("analog", 5, true),
            (20, 1) | (22, 1) => ("counter", 5, false),
            (22, 5) => ("counter", 5, true),
            (30, 1) | (32, 1) => ("analog_int", 5, false),
            _ => bail!("unsupported measurement group/variation {g}/{v}"),
        };
        for index in 0..count {
            let idx = if prefix {
                ensure!(offset + 2 <= b.len(), "missing point index");
                let i = u16::from_le_bytes([b[offset], b[offset + 1]]) as usize;
                offset += 2;
                i
            } else if q == 0x17 {
                ensure!(offset < b.len(), "missing point index");
                let i = b[offset] as usize;
                offset += 1;
                i
            } else {
                start + index
            };
            ensure!(
                offset + width + if timed { 6 } else { 0 } <= b.len(),
                "truncated point"
            );
            let flags = b[offset];
            let value = match kind {
                "binary" => json!(flags & 0x80 != 0),
                "analog" => {
                    let n = f32::from_le_bytes(b[offset + 1..offset + 5].try_into()?);
                    ensure!(n.is_finite(), "nonfinite wire value");
                    json!(n)
                }
                "analog_int" => json!(i32::from_le_bytes(b[offset + 1..offset + 5].try_into()?)),
                _ => json!(u32::from_le_bytes(b[offset + 1..offset + 5].try_into()?)),
            };
            offset += width;
            let mut point = json!({"kind":if kind=="analog_int"{"analog"}else{kind},"index":idx,"flags":flags,"value":value,"event":matches!(g,2|22|32)});
            if timed {
                let mut time = [0; 8];
                time[..6].copy_from_slice(&b[offset..offset + 6]);
                offset += 6;
                point["timestamp_ms"] = json!(u64::from_le_bytes(time));
            }
            out.push(point);
        }
    }
    Ok(out)
}
pub struct Device {
    assembly: Assembly,
    transport: u8,
    local: u16,
    remote: u16,
    last: Option<(Vec<u8>, Vec<u8>)>,
    confirm: Option<u8>,
    request: Vec<u8>,
}
impl Default for Device {
    fn default() -> Self {
        Self {
            assembly: Assembly::default(),
            transport: 0,
            local: 10,
            remote: 1,
            last: None,
            confirm: None,
            request: vec![],
        }
    }
}
#[async_trait::async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut TcpStream) -> Result<Vec<u8>> {
        read(s).await
    }
    fn receive(&mut self, f: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        ensure!(
            f.len() >= 10
                && u16::from_le_bytes([f[4], f[5]]) == self.local
                && u16::from_le_bytes([f[6], f[7]]) == self.remote
                && f[3] & 0xc0 == 0xc0,
            "DNP3 link addressing/direction"
        );
        let b = payload(f)?;
        match f[3] & 15 {
            0 => {
                ensure!(b.is_empty(), "reset link payload");
                self.assembly = Assembly::default();
                return Ok((link(0, self.remote, self.local, &[]), None));
            }
            9 => {
                ensure!(b.is_empty(), "link status payload");
                return Ok((link(0x0b, self.remote, self.local, &[]), None));
            }
            4 => {}
            _ => bail!("unsupported confirmed link service"),
        }
        let Some(app) = self.assembly.push(&b)? else {
            return Ok((vec![], None));
        };
        ensure!(
            app.len() >= 2 && app[0] & 0xc0 == 0xc0 && app[0] & 0x10 == 0,
            "fragmented/unsolicited app requests unsupported"
        );
        let seq = app[0] & 15;
        if app[1] == 0 {
            ensure!(
                app.len() == 2 && self.confirm == Some(seq),
                "unexpected application confirmation"
            );
            self.confirm = None;
            return Ok((vec![], None));
        }
        if let Some((last, reply)) = &self.last {
            if last == &app {
                return Ok((reply.clone(), None));
            }
        }
        if matches!(app[1], 20 | 21) {
            let reply = frames(
                &[0xc0 | seq, 0x81, 0, if app[1] == 20 { 1 } else { 0 }],
                false,
                self.local,
                self.remote,
                &mut self.transport,
            );
            self.last = Some((app, reply.clone()));
            return Ok((reply, None));
        }
        self.request = app.clone();
        let mut request = json!({"sequence":seq,"function":app[1]});
        match app[1] {
            1 => match read_headers(&app[2..]) {
                Ok(classes) => {
                    request["operation"] = json!("poll");
                    request["classes"] = json!(classes)
                }
                Err(_) => {
                    let reply = frames(
                        &[0xc0 | seq, 0x81, 0, 2],
                        false,
                        self.local,
                        self.remote,
                        &mut self.transport,
                    );
                    return Ok((reply, None));
                }
            },
            5 => {
                let p = &app[2..];
                ensure!(
                    p.len() >= 4 && p[..2] == [12, 1],
                    "only CROB direct-operate supported"
                );
                let (base, index) = match p[2] {
                    0x17 => {
                        ensure!(p.len() == 16 && p[3] == 1, "one 8-bit CROB index required");
                        (5, p[4] as u16)
                    }
                    0x28 => {
                        ensure!(
                            p.len() == 18 && p[3..5] == [1, 0],
                            "one 16-bit CROB index required"
                        );
                        (7, u16::from_le_bytes([p[5], p[6]]))
                    }
                    _ => bail!("unsupported CROB qualifier"),
                };
                ensure!(
                    (1..=4).contains(&p[base]) && p[base + 1] > 0 && p[base + 10] == 0,
                    "invalid CROB fields"
                );
                request["operation"] = json!("control");
                request["index"] = json!(index);
                request["code"] = json!(p[base]);
                request["count"] = json!(p[base + 1]);
                request["on_ms"] = json!(u32::from_le_bytes(p[base + 2..base + 6].try_into()?));
                request["off_ms"] = json!(u32::from_le_bytes(p[base + 6..base + 10].try_into()?));
            }
            _ => {
                let reply = frames(
                    &[0xc0 | seq, 0x81, 0, 1],
                    false,
                    self.local,
                    self.remote,
                    &mut self.transport,
                );
                return Ok((reply, None));
            }
        }
        Ok((vec![], Some(request)))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let seq = number(r, "sequence", 15)? as u8;
        let mut app = vec![0xc0 | seq, 0x81, 0, 0];
        let mut events = false;
        if r["operation"] == "poll" {
            if let Some(a) = a.filter(|a| a["type"] == "dnp3_measurements") {
                let points = a["points"].as_array().context("points array required")?;
                let selected = r["classes"].as_array().context("classes")?;
                let mut static_points = vec![];
                let mut event_points = vec![];
                for point in points {
                    if point.get("class").and_then(Value::as_u64).unwrap_or(0) == 0 {
                        if selected.contains(&json!(0)) {
                            static_points.push(point.clone());
                        }
                    } else if selected.contains(&point["class"]) {
                        event_points.push(point.clone());
                    }
                }
                let built = (|| -> Result<Vec<u8>> {
                    let mut b = encode_points(&static_points, false)?;
                    b.extend(encode_points(&event_points, true)?);
                    ensure!(b.len() + 4 <= MAX_APPLICATION, "response budget");
                    Ok(b)
                })();
                match built {
                    Ok(b) => {
                        events = !event_points.is_empty();
                        app.extend(b);
                    }
                    Err(_) => app[3] = 4,
                }
            } else {
                app[3] = 4;
            }
        } else {
            let original = &self.request;
            app.extend(&original[2..]);
            let status = a
                .filter(|a| a["type"] == "dnp3_control_result")
                .and_then(|a| a["status"].as_str());
            let last = app.len() - 1;
            app[last] = match status {
                Some("success") => 0,
                Some("not_supported") => 4,
                Some("out_of_range") => 12,
                _ => 6,
            };
        }
        if events {
            app[0] |= 0x20;
            self.confirm = Some(seq);
        }
        let reply = frames(&app, false, self.local, self.remote, &mut self.transport);
        let request = self.request.clone();
        self.last = Some((request, reply.clone()));
        Ok(reply)
    }
}
pub fn validate(a: &Value) -> Result<()> {
    match a["type"].as_str() {
        Some("dnp3_poll") => {
            let classes = a["classes"].as_array().context("classes required")?;
            ensure!(
                !classes.is_empty()
                    && classes.len() <= 4
                    && classes.iter().all(|v| v.as_u64().is_some_and(|v| v <= 3)),
                "classes must be 0..3"
            );
        }
        Some("dnp3_control") => {
            number(a, "index", 65535)?;
            let code = number(a, "code", 255)?;
            ensure!((1..=4).contains(&code), "unsupported CROB code");
            let count = number(a, "count", 255)?;
            ensure!(count > 0, "positive control count required");
            number(a, "on_ms", u32::MAX as u64)?;
            number(a, "off_ms", u32::MAX as u64)?;
        }
        _ => bail!("unknown DNP3 action"),
    };
    Ok(())
}
#[derive(Default)]
pub struct Scanner {
    assembly: Assembly,
    transport: u8,
    sequence: u8,
}
#[async_trait::async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut TcpStream) -> Result<()> {
        s.write_all(&link(0xc9, 10, 1, &[])).await?;
        let f = read(s).await?;
        ensure!(
            f[3] == 0x0b && f[4..8] == [1, 0, 10, 0] && payload(&f)?.is_empty(),
            "DNP3 link status refused"
        );
        Ok(())
    }
    async fn exchange(&mut self, s: &mut TcpStream, a: &Value) -> Result<Value> {
        validate(a)?;
        self.sequence = (self.sequence + 1) & 15;
        let control = a["type"] == "dnp3_control";
        let mut app = vec![0xc0 | self.sequence, if control { 5 } else { 1 }];
        if control {
            app.extend([12, 1, 0x28, 1, 0]);
            app.extend((number(a, "index", 65535)? as u16).to_le_bytes());
            app.extend([
                number(a, "code", 255)? as u8,
                number(a, "count", 255)? as u8,
            ]);
            app.extend((number(a, "on_ms", u32::MAX as u64)? as u32).to_le_bytes());
            app.extend((number(a, "off_ms", u32::MAX as u64)? as u32).to_le_bytes());
            app.push(0);
        } else {
            for class in a["classes"].as_array().context("classes")? {
                app.extend([60, class.as_u64().expect("validated") as u8 + 1, 6]);
            }
        }
        s.write_all(&frames(&app, true, 1, 10, &mut self.transport))
            .await?;
        loop {
            let f = read(s).await?;
            ensure!(
                f[3] == 0x44 && f[4..8] == [1, 0, 10, 0],
                "response link direction/addresses"
            );
            let Some(response) = self.assembly.push(&payload(&f)?)? else {
                continue;
            };
            ensure!(
                response.len() >= 4
                    && response[1] == 0x81
                    && response[0] & 15 == self.sequence
                    && response[0] & 0xc0 == 0xc0,
                "application response correlation"
            );
            if response[0] & 0x20 != 0 {
                s.write_all(&frames(
                    &[0xc0 | self.sequence, 0],
                    true,
                    1,
                    10,
                    &mut self.transport,
                ))
                .await?;
            }
            let iin = u16::from_le_bytes([response[2], response[3]]);
            if iin & 0x0700 != 0 {
                return Ok(json!({"success":false,"iin":iin}));
            }
            if control {
                ensure!(
                    response.len() == app.len() + 2
                        && response[4..response.len() - 1] == app[2..app.len() - 1],
                    "control echo mismatch"
                );
                let status = *response.last().context("status")?;
                return Ok(json!({"success":status==0,"status":status}));
            }
            return Ok(json!({"success":true,"iin":iin,"points":decode_points(&response[4..])?}));
        }
    }
}
