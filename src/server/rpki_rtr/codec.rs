//! Selected RFC8210/RFC6810 prefix records, with no RPKI repository or VRP store.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{net::IpAddr, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_PDU_BYTES: usize = 4096;
pub const MAX_RECORDS: usize = 256;
pub const MAX_JSON_BYTES: usize = 64 * 1024;
pub const DEFAULT_IO_SECONDS: u64 = 90;
pub const DEFAULT_HANDLER_SECONDS: u64 = 30;
pub const DEFAULT_REFRESH_SECONDS: u32 = 3600;
pub const DEFAULT_RETRY_SECONDS: u32 = 600;
pub const DEFAULT_EXPIRE_SECONDS: u32 = 7200;
pub const DEFAULT_PROTOCOL_VERSION: u8 = 1;
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

fn announced() -> bool { true }
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub prefix: String,
    pub max_length: u8,
    pub asn: u32,
    #[serde(default = "announced")]
    pub announcement: bool,
}
impl Record {
    pub fn parsed(&self) -> Result<(IpAddr, u8)> {
        ensure!(self.prefix.len() <= 49, "RPKI-RTR prefix text bound");
        let (address, bits) = self.prefix.split_once('/').context("RPKI-RTR prefix requires CIDR length")?;
        let ip: IpAddr = address.parse().context("RPKI-RTR prefix address")?;
        let len: u8 = bits.parse().context("RPKI-RTR prefix length")?;
        let canonical = match ip {
            IpAddr::V4(v) => {
                ensure!(len <= self.max_length && self.max_length <= 32, "RPKI-RTR IPv4 prefix lengths");
                u32::from(v) & if len == 0 { u32::MAX } else { u32::MAX >> len } == 0
            }
            IpAddr::V6(v) => {
                ensure!(len <= self.max_length && self.max_length <= 128, "RPKI-RTR IPv6 prefix lengths");
                u128::from(v) & if len == 0 { u128::MAX } else { u128::MAX >> len } == 0
            }
        };
        ensure!(canonical, "RPKI-RTR prefix has nonzero host bits");
        Ok((ip, len))
    }
    pub fn canonical(&self) -> Result<Self> {
        let (ip, len) = self.parsed()?;
        Ok(Self { prefix: format!("{ip}/{len}"), ..self.clone() })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub serial: u32,
    pub records: Vec<Record>,
}
impl Batch {
    pub fn validate(&self, reset: bool) -> Result<()> {
        ensure!(self.records.len() <= MAX_RECORDS, "RPKI-RTR transaction record bound");
        let mut announcements = std::collections::HashSet::new();
        for record in &self.records {
            let (ip, len) = record.parsed()?;
            ensure!(!reset || record.announcement, "RPKI-RTR reset cannot withdraw records");
            if record.announcement {
                ensure!(announcements.insert((ip, len, record.max_length, record.asn)), "RPKI-RTR duplicate announcement in transaction");
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Intervals {
    pub refresh: u32,
    pub retry: u32,
    pub expire: u32,
}
impl Default for Intervals {
    fn default() -> Self {
        Self { refresh: DEFAULT_REFRESH_SECONDS, retry: DEFAULT_RETRY_SECONDS, expire: DEFAULT_EXPIRE_SECONDS }
    }
}
impl Intervals {
    pub fn validate(&self) -> Result<()> {
        ensure!((1..=86400).contains(&self.refresh), "RPKI-RTR refresh interval1..86400");
        ensure!((1..=7200).contains(&self.retry), "RPKI-RTR retry interval1..7200");
        ensure!((600..=172800).contains(&self.expire) && self.expire > self.refresh && self.expire > self.retry, "RPKI-RTR expire interval600..172800 and larger than refresh/retry");
        Ok(())
    }
}
/// RFC1982 strict order, including wrap-around. The half-space is ambiguous.
pub fn serial_newer(next: u32, old: u32) -> bool {
    let difference = next.wrapping_sub(old);
    difference != 0 && difference < 0x8000_0000
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pdu {
    SerialNotify { session: u16, serial: u32 },
    SerialQuery { session: u16, serial: u32 },
    ResetQuery,
    CacheResponse { session: u16 },
    Prefix(Record),
    EndOfData { session: u16, serial: u32, intervals: Intervals },
    CacheReset,
    ErrorReport { code: u16, erroneous: Vec<u8>, diagnostic: String },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub version: u8,
    pub pdu: Pdu,
}
fn number(bytes: &[u8], start: usize) -> u32 {
    u32::from_be_bytes(bytes[start..start + 4].try_into().expect("checked fixed PDU length"))
}
impl Packet {
    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(self.version <= 1, "RPKI-RTR supported versions0/1");
        let mut body = Vec::new();
        let (kind, field) = match &self.pdu {
            Pdu::SerialNotify { session, serial } => { body.extend(serial.to_be_bytes()); (0, *session) }
            Pdu::SerialQuery { session, serial } => { body.extend(serial.to_be_bytes()); (1, *session) }
            Pdu::ResetQuery => (2, 0),
            Pdu::CacheResponse { session } => (3, *session),
            Pdu::Prefix(record) => {
                let (ip, len) = record.parsed()?;
                body.extend([u8::from(record.announcement), len, record.max_length, 0]);
                let kind = match ip {
                    IpAddr::V4(v) => { body.extend(v.octets()); 4 }
                    IpAddr::V6(v) => { body.extend(v.octets()); 6 }
                };
                body.extend(record.asn.to_be_bytes());
                (kind, 0)
            }
            Pdu::EndOfData { session, serial, intervals } => {
                intervals.validate()?;
                body.extend(serial.to_be_bytes());
                if self.version == 1 {
                    body.extend(intervals.refresh.to_be_bytes());
                    body.extend(intervals.retry.to_be_bytes());
                    body.extend(intervals.expire.to_be_bytes());
                }
                (7, *session)
            }
            Pdu::CacheReset => (8, 0),
            Pdu::ErrorReport { code, erroneous, diagnostic } => {
                ensure!(*code <= 8 && erroneous.len() <= 64 && diagnostic.len() <= 1024, "RPKI-RTR error report bound");
                body.extend((erroneous.len() as u32).to_be_bytes());
                body.extend(erroneous);
                body.extend((diagnostic.len() as u32).to_be_bytes());
                body.extend(diagnostic.as_bytes());
                (10, *code)
            }
        };
        let len = body.len() + 8;
        ensure!(len <= MAX_PDU_BYTES, "RPKI-RTR encoded PDU bound");
        let mut frame = Vec::with_capacity(len);
        frame.extend([self.version, kind]);
        frame.extend(field.to_be_bytes());
        frame.extend((len as u32).to_be_bytes());
        frame.extend(body);
        Ok(frame)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!((8..=MAX_PDU_BYTES).contains(&bytes.len()), "RPKI-RTR PDU length bound");
        let version = bytes[0];
        ensure!(version <= 1, "RPKI-RTR unsupported protocol version");
        ensure!(number(bytes, 4) as usize == bytes.len(), "RPKI-RTR exact PDU length");
        let field = u16::from_be_bytes([bytes[2], bytes[3]]);
        let pdu = match bytes[1] {
            0 | 1 => {
                ensure!(bytes.len() == 12, "RPKI-RTR serial PDU length");
                let serial = number(bytes, 8);
                if bytes[1] == 0 { Pdu::SerialNotify { session: field, serial } }
                else { Pdu::SerialQuery { session: field, serial } }
            }
            2 => { ensure!(bytes.len() == 8, "RPKI-RTR reset query length"); Pdu::ResetQuery }
            3 => { ensure!(bytes.len() == 8, "RPKI-RTR cache response length"); Pdu::CacheResponse { session: field } }
            4 | 6 => {
                let v4 = bytes[1] == 4;
                ensure!(bytes.len() == if v4 { 20 } else { 32 }, "RPKI-RTR prefix PDU length");
                let ip = if v4 {
                    IpAddr::V4(std::net::Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[12..16])?))
                } else {
                    IpAddr::V6(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[12..28])?))
                };
                let record = Record { prefix: format!("{ip}/{}", bytes[9]), max_length: bytes[10], asn: number(bytes, if v4 { 16 } else { 28 }), announcement: bytes[8] & 1 == 1 };
                record.parsed()?;
                Pdu::Prefix(record)
            }
            7 => {
                ensure!(bytes.len() == if version == 1 { 24 } else { 12 }, "RPKI-RTR version-specific EndOfData length");
                let intervals = if version == 1 { Intervals { refresh: number(bytes, 12), retry: number(bytes, 16), expire: number(bytes, 20) } } else { Intervals::default() };
                intervals.validate()?;
                Pdu::EndOfData { session: field, serial: number(bytes, 8), intervals }
            }
            8 => { ensure!(bytes.len() == 8, "RPKI-RTR cache reset length"); Pdu::CacheReset }
            10 => {
                ensure!(bytes.len() >= 16 && field <= 8, "RPKI-RTR error header");
                let encapsulated = number(bytes, 8) as usize;
                ensure!(encapsulated <= 64 && encapsulated + 16 <= bytes.len(), "RPKI-RTR encapsulated error bound");
                let text_len = number(bytes, 12 + encapsulated) as usize;
                ensure!(text_len <= 1024 && 16 + encapsulated + text_len == bytes.len(), "RPKI-RTR error diagnostic bound");
                Pdu::ErrorReport { code: field, erroneous: bytes[12..12 + encapsulated].to_vec(), diagnostic: std::str::from_utf8(&bytes[16 + encapsulated..])?.to_owned() }
            }
            _ => bail!("RPKI-RTR unsupported selected PDU type"),
        };
        Ok(Self { version, pdu })
    }
}
/// Read exactly one header/body before any allocation; TCP chunking is irrelevant.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R, deadline: Duration) -> Result<Vec<u8>> {
    tokio::time::timeout(deadline, async {
        let mut header = [0u8; 8];
        reader.read_exact(&mut header).await?;
        let len = number(&header, 4) as usize;
        ensure!((8..=MAX_PDU_BYTES).contains(&len), "RPKI-RTR inbound PDU bound");
        let mut bytes = vec![0u8; len];
        bytes[..8].copy_from_slice(&header);
        reader.read_exact(&mut bytes[8..]).await?;
        Ok(bytes)
    }).await.context("RPKI-RTR whole-frame deadline")?
}
pub async fn write_packet<W: AsyncWrite + Unpin>(writer: &mut W, packet: &Packet) -> Result<usize> {
    let bytes = packet.encode()?;
    tokio::time::timeout(WRITE_TIMEOUT, async {
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok::<_, anyhow::Error>(())
    }).await.context("RPKI-RTR write deadline")??;
    Ok(bytes.len())
}
pub fn within_json_budget(value: &Value) -> bool {
    crate::utils::json_budget::within_budget(value, crate::utils::json_budget::JsonBudget { max_bytes: MAX_JSON_BYTES, max_nodes: 4096, max_depth: 8 })
}
pub fn owned_json(value: Value) -> Result<Value> {
    if !within_json_budget(&value) {
        crate::utils::json_budget::drop_iteratively(value);
        bail!("RPKI-RTR action JSON bound");
    }
    Ok(value)
}
pub fn timeout(value: Option<u64>, default: u64) -> Result<Duration> {
    let seconds = value.unwrap_or(default);
    ensure!((1..=300).contains(&seconds), "RPKI-RTR timeout1..300seconds");
    Ok(Duration::from_secs(seconds))
}
