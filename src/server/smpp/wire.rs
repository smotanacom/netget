//! SMPP 3.4 PDUs (the SMS Peer-to-Peer protocol between an ESME and an SMSC): the 16-byte
//! header, C-Octet strings, bind, submit_sm/deliver_sm with their TLVs, enquire_link, unbind
//! and generic_nack. Shared by the server (SMSC) and the client (ESME); every length is
//! checked before anything is allocated for it.
use anyhow::{bail, ensure, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest PDU accepted: room for a 64 KiB message_payload and the fixed fields.
pub const MAX_PDU: usize = 65536 + 512;
pub const HEADER_LEN: usize = 16;
/// short_message holds at most 254 octets; longer text rides in message_payload.
pub const MAX_SHORT_MESSAGE: usize = 254;
pub const MAX_PAYLOAD: usize = 65536;
/// Deadline for the rest of a PDU once its first byte has arrived, and for each write.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub const GENERIC_NACK: u32 = 0x8000_0000;
pub const BIND_RECEIVER: u32 = 0x0000_0001;
pub const BIND_TRANSMITTER: u32 = 0x0000_0002;
pub const SUBMIT_SM: u32 = 0x0000_0004;
pub const DELIVER_SM: u32 = 0x0000_0005;
pub const UNBIND: u32 = 0x0000_0006;
pub const BIND_TRANSCEIVER: u32 = 0x0000_0009;
pub const ENQUIRE_LINK: u32 = 0x0000_0015;
pub const RESP: u32 = 0x8000_0000;

pub const ESME_ROK: u32 = 0x00;
pub const ESME_RINVMSGLEN: u32 = 0x01;
pub const ESME_RINVCMDLEN: u32 = 0x02;
pub const ESME_RINVCMDID: u32 = 0x03;
pub const ESME_RINVBNDSTS: u32 = 0x04;
pub const ESME_RALYBND: u32 = 0x05;
pub const ESME_RSYSERR: u32 = 0x08;
pub const ESME_RINVSRCADR: u32 = 0x0A;
pub const ESME_RINVDSTADR: u32 = 0x0B;
pub const ESME_RBINDFAIL: u32 = 0x0D;
pub const ESME_RINVPASWD: u32 = 0x0E;
pub const ESME_RINVSYSID: u32 = 0x0F;
pub const ESME_RSUBMITFAIL: u32 = 0x45;
pub const ESME_RTHROTTLED: u32 = 0x58;
pub const ESME_RX_T_APPN: u32 = 0x64;
pub const ESME_RX_P_APPN: u32 = 0x65;

pub const TLV_MESSAGE_PAYLOAD: u16 = 0x0424;
pub const TLV_RECEIPTED_MESSAGE_ID: u16 = 0x001E;
pub const TLV_MESSAGE_STATE: u16 = 0x0427;

/// Status names the handler may use, and their codes.
pub const STATUS_NAMES: &[(&str, u32)] = &[
    ("ESME_RINVMSGLEN", ESME_RINVMSGLEN),
    ("ESME_RSYSERR", ESME_RSYSERR),
    ("ESME_RINVSRCADR", ESME_RINVSRCADR),
    ("ESME_RINVDSTADR", ESME_RINVDSTADR),
    ("ESME_RBINDFAIL", ESME_RBINDFAIL),
    ("ESME_RINVPASWD", ESME_RINVPASWD),
    ("ESME_RINVSYSID", ESME_RINVSYSID),
    ("ESME_RSUBMITFAIL", ESME_RSUBMITFAIL),
    ("ESME_RTHROTTLED", ESME_RTHROTTLED),
    ("ESME_RX_T_APPN", ESME_RX_T_APPN),
    ("ESME_RX_P_APPN", ESME_RX_P_APPN),
];

pub fn status_code(name: &str) -> Option<u32> {
    STATUS_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
}

pub fn status_name(code: u32) -> String {
    match code {
        ESME_ROK => "ESME_ROK".into(),
        ESME_RINVCMDLEN => "ESME_RINVCMDLEN".into(),
        ESME_RINVCMDID => "ESME_RINVCMDID".into(),
        ESME_RINVBNDSTS => "ESME_RINVBNDSTS".into(),
        ESME_RALYBND => "ESME_RALYBND".into(),
        other => STATUS_NAMES
            .iter()
            .find(|(_, c)| *c == other)
            .map(|(n, _)| n.to_string())
            .unwrap_or_else(|| format!("0x{other:08X}")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    pub command_id: u32,
    pub status: u32,
    pub sequence: u32,
    pub body: Vec<u8>,
}

impl Pdu {
    pub fn new(command_id: u32, status: u32, sequence: u32, body: Vec<u8>) -> Self {
        Self {
            command_id,
            status,
            sequence,
            body,
        }
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.body.len());
        out.extend_from_slice(&((HEADER_LEN + self.body.len()) as u32).to_be_bytes());
        out.extend_from_slice(&self.command_id.to_be_bytes());
        out.extend_from_slice(&self.status.to_be_bytes());
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

/// Read one PDU. `None` on a clean EOF between PDUs; the first byte may wait `idle`.
pub async fn read_pdu<R: AsyncRead + Unpin>(r: &mut R, idle: Duration) -> Result<Option<Pdu>> {
    let mut len = [0u8; 4];
    match tokio::time::timeout(idle, r.read(&mut len[..1])).await {
        Ok(Ok(0)) => return Ok(None),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => bail!("SMPP peer idle for {}s", idle.as_secs()),
    }
    tokio::time::timeout(IO_TIMEOUT, async {
        r.read_exact(&mut len[1..]).await?;
        let len = u32::from_be_bytes(len) as usize;
        ensure!(
            (HEADER_LEN..=MAX_PDU).contains(&len),
            "SMPP command_length {len} outside {HEADER_LEN}..={MAX_PDU}"
        );
        let mut rest = vec![0u8; len - 4];
        r.read_exact(&mut rest).await?;
        Ok(Some(Pdu {
            command_id: u32::from_be_bytes(rest[0..4].try_into()?),
            status: u32::from_be_bytes(rest[4..8].try_into()?),
            sequence: u32::from_be_bytes(rest[8..12].try_into()?),
            body: rest[12..].to_vec(),
        }))
    })
    .await
    .context("SMPP read deadline")?
}

pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
    pub fn rest(&self) -> &'a [u8] {
        self.buf
    }
    pub fn u8(&mut self) -> Result<u8> {
        ensure!(!self.buf.is_empty(), "SMPP PDU ends inside a field");
        let b = self.buf[0];
        self.buf = &self.buf[1..];
        Ok(b)
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.buf.len() >= n, "SMPP PDU ends inside a field");
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    /// A C-Octet string of at most `max` octets including its NUL.
    pub fn cstring(&mut self, max: usize) -> Result<String> {
        let end = self
            .buf
            .iter()
            .take(max)
            .position(|b| *b == 0)
            .context("SMPP C-Octet string unterminated or too long")?;
        let s = String::from_utf8_lossy(&self.buf[..end]).to_string();
        self.buf = &self.buf[end + 1..];
        Ok(s)
    }
    /// TLVs to the end of the body: (tag, value).
    pub fn tlvs(&mut self) -> Result<Vec<(u16, Vec<u8>)>> {
        let mut out = Vec::new();
        while !self.buf.is_empty() {
            let tag = u16::from_be_bytes(self.bytes(2)?.try_into()?);
            let len = u16::from_be_bytes(self.bytes(2)?.try_into()?) as usize;
            out.push((tag, self.bytes(len)?.to_vec()));
        }
        Ok(out)
    }
}

pub fn put_cstring(out: &mut Vec<u8>, s: &str, max: usize) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&bytes[..bytes.len().min(max - 1)]);
    out.push(0);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bind {
    pub system_id: String,
    pub password: String,
    pub system_type: String,
    pub interface_version: u8,
    pub address_range: String,
}

pub fn parse_bind(body: &[u8]) -> Result<Bind> {
    let mut r = Reader::new(body);
    let system_id = r.cstring(16)?;
    let password = r.cstring(9)?;
    let system_type = r.cstring(13)?;
    let interface_version = r.u8()?;
    r.u8()?;
    r.u8()?;
    let address_range = r.cstring(41)?;
    Ok(Bind {
        system_id,
        password,
        system_type,
        interface_version,
        address_range,
    })
}

pub fn encode_bind(b: &Bind) -> Vec<u8> {
    let mut out = Vec::new();
    put_cstring(&mut out, &b.system_id, 16);
    put_cstring(&mut out, &b.password, 9);
    put_cstring(&mut out, &b.system_type, 13);
    out.push(b.interface_version);
    out.push(0);
    out.push(0);
    put_cstring(&mut out, &b.address_range, 41);
    out
}

/// submit_sm and deliver_sm share one layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    pub service_type: String,
    pub source_ton: u8,
    pub source_npi: u8,
    pub source_addr: String,
    pub dest_ton: u8,
    pub dest_npi: u8,
    pub destination_addr: String,
    pub esm_class: u8,
    pub registered_delivery: u8,
    pub data_coding: u8,
    /// short_message, or the message_payload TLV when short_message is empty.
    pub payload: Vec<u8>,
    pub tlvs: Vec<(u16, Vec<u8>)>,
}

pub fn parse_message(body: &[u8]) -> Result<Message> {
    let mut r = Reader::new(body);
    let service_type = r.cstring(6)?;
    let source_ton = r.u8()?;
    let source_npi = r.u8()?;
    let source_addr = r.cstring(21)?;
    let dest_ton = r.u8()?;
    let dest_npi = r.u8()?;
    let destination_addr = r.cstring(21)?;
    let esm_class = r.u8()?;
    r.u8()?; // protocol_id
    r.u8()?; // priority_flag
    r.cstring(17)?; // schedule_delivery_time
    r.cstring(17)?; // validity_period
    let registered_delivery = r.u8()?;
    r.u8()?; // replace_if_present_flag
    let data_coding = r.u8()?;
    r.u8()?; // sm_default_msg_id
    let sm_length = r.u8()? as usize;
    ensure!(
        sm_length <= MAX_SHORT_MESSAGE,
        "sm_length {sm_length} exceeds 254"
    );
    let mut payload = r.bytes(sm_length)?.to_vec();
    let tlvs = r.tlvs()?;
    if let Some((_, p)) = tlvs.iter().find(|(t, _)| *t == TLV_MESSAGE_PAYLOAD) {
        ensure!(
            payload.is_empty(),
            "both short_message and message_payload are set"
        );
        payload = p.clone();
    }
    Ok(Message {
        service_type,
        source_ton,
        source_npi,
        source_addr,
        dest_ton,
        dest_npi,
        destination_addr,
        esm_class,
        registered_delivery,
        data_coding,
        payload,
        tlvs,
    })
}

pub fn encode_message(m: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    put_cstring(&mut out, &m.service_type, 6);
    out.push(m.source_ton);
    out.push(m.source_npi);
    put_cstring(&mut out, &m.source_addr, 21);
    out.push(m.dest_ton);
    out.push(m.dest_npi);
    put_cstring(&mut out, &m.destination_addr, 21);
    out.push(m.esm_class);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(0);
    out.push(m.registered_delivery);
    out.push(0);
    out.push(m.data_coding);
    out.push(0);
    let long = m.payload.len() > MAX_SHORT_MESSAGE;
    if long {
        out.push(0);
    } else {
        out.push(m.payload.len() as u8);
        out.extend_from_slice(&m.payload);
    }
    let mut tlvs = m.tlvs.clone();
    if long {
        tlvs.push((TLV_MESSAGE_PAYLOAD, m.payload.clone()));
    }
    for (tag, value) in tlvs {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(&value);
    }
    out
}

/// Decode message text by data_coding: UCS-2 (8) as UTF-16BE, Latin-1 (3) as Latin-1, and the
/// SMSC default (0) and IA5 (1) as their ASCII-compatible octets. `None` when the octets do
/// not decode (binary data_coding 2 or 4, or an odd UCS-2 length).
pub fn decode_text(data_coding: u8, payload: &[u8]) -> Option<String> {
    match data_coding {
        8 => {
            if !payload.len().is_multiple_of(2) {
                return None;
            }
            let units: Vec<u16> = payload
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&units).ok()
        }
        3 => Some(payload.iter().map(|b| *b as char).collect()),
        0 | 1 => std::str::from_utf8(payload).ok().map(str::to_string),
        _ => None,
    }
}

/// Encode text: ASCII as the SMSC default alphabet (0), anything else as UCS-2 (8).
pub fn encode_text(text: &str) -> (u8, Vec<u8>) {
    if text.is_ascii() {
        (0, text.as_bytes().to_vec())
    } else {
        (8, text.encode_utf16().flat_map(u16::to_be_bytes).collect())
    }
}

/// The SMPP 3.4 Appendix B delivery receipt text.
pub fn receipt_text(id: &str, stat: &str, submitted: &str, done: &str, text: &str) -> String {
    let err = if stat == "DELIVRD" { "000" } else { "001" };
    let dlvrd = if stat == "DELIVRD" { "001" } else { "000" };
    let excerpt: String = text.chars().take(20).collect();
    format!("id:{id} sub:001 dlvrd:{dlvrd} submit date:{submitted} done date:{done} stat:{stat} err:{err} text:{excerpt}")
}

/// message_state for a receipt's stat, as the message_state TLV carries it.
pub fn message_state(stat: &str) -> u8 {
    match stat {
        "DELIVRD" => 2,
        "EXPIRED" => 3,
        "DELETED" => 4,
        "UNDELIV" => 5,
        "ACCEPTD" => 6,
        "REJECTD" => 8,
        _ => 7,
    }
}

/// Fields of a delivery receipt, when `text` is one.
pub fn parse_receipt(text: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    if !text.starts_with("id:") {
        return None;
    }
    let mut out = serde_json::Map::new();
    let (head, body) = match text.split_once(" text:") {
        Some((h, b)) => (h, Some(b)),
        None => (text, None),
    };
    let mut key: Option<String> = None;
    for token in head.split(' ') {
        if let Some((k, v)) = token.split_once(':') {
            let k = match key.take() {
                Some(prefix) => format!("{prefix}_{k}"),
                None => k.to_string(),
            };
            out.insert(k, serde_json::Value::String(v.to_string()));
        } else {
            key = Some(token.to_string());
        }
    }
    if let Some(b) = body {
        out.insert("text".into(), serde_json::Value::String(b.to_string()));
    }
    Some(out)
}
