//! DICOM upper layer (PS3.8 §9): A-ASSOCIATE-RQ/AC/RJ, P-DATA-TF with presentation data
//! values, A-RELEASE, A-ABORT; and DIMSE command sets (PS3.7), which are always Implicit VR
//! Little Endian.
use super::dataset::{self, IMPLICIT_LE};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const APPLICATION_CONTEXT: &str = "1.2.840.10008.3.1.1.1";
pub const VERIFICATION: &str = "1.2.840.10008.1.1";
pub const PATIENT_ROOT_FIND: &str = "1.2.840.10008.5.1.4.1.2.1.1";
pub const STUDY_ROOT_FIND: &str = "1.2.840.10008.5.1.4.1.2.2.1";
pub const STORAGE_PREFIX: &str = "1.2.840.10008.5.1.4.1.1.";
pub const IMPLEMENTATION_CLASS: &str = "1.2.826.0.1.3680043.10.1408.1";
pub const IMPLEMENTATION_VERSION: &str = "NETGET_1";
/// Largest PDU NetGet accepts and announces (its maximum receive length).
pub const MAX_PDU: u32 = 1 << 20;

pub const C_STORE_RQ: u16 = 0x0001;
pub const C_FIND_RQ: u16 = 0x0020;
pub const C_ECHO_RQ: u16 = 0x0030;
pub const C_CANCEL_RQ: u16 = 0x0FFF;
pub const NO_DATASET: u16 = 0x0101;

#[derive(Debug, Clone, PartialEq)]
pub struct PresentationContext {
    pub id: u8,
    pub abstract_syntax: String,
    pub transfer_syntaxes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssociateRq {
    pub called: String,
    pub calling: String,
    pub contexts: Vec<PresentationContext>,
    pub max_pdu: u32,
    pub implementation: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pdu {
    AssociateRq(AssociateRq),
    /// (context id, result, transfer syntax), peer max PDU.
    AssociateAc(Vec<(u8, u8, String)>, u32),
    AssociateRj {
        result: u8,
        source: u8,
        reason: u8,
    },
    /// (context id, flags, bytes).
    Data(Vec<(u8, u8, Vec<u8>)>),
    ReleaseRq,
    ReleaseRp,
    Abort {
        source: u8,
        reason: u8,
    },
}

fn ae(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).trim().to_owned()
}

fn uid(raw: &[u8]) -> Result<String> {
    let s = std::str::from_utf8(raw)?
        .trim_end_matches(['\0', ' '])
        .to_owned();
    ensure!(
        !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
        "invalid UID"
    );
    Ok(s)
}

pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Pdu>> {
    let mut head = [0u8; 6];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes([head[2], head[3], head[4], head[5]]);
    ensure!(len <= MAX_PDU + 6, "PDU of {len} bytes exceeds the maximum");
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(Some(decode(head[0], &body)?))
}

fn items(b: &[u8]) -> Result<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        ensure!(i + 4 <= b.len(), "truncated item");
        let t = b[i];
        let l = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let body = b
            .get(i + 4..i + 4 + l)
            .context("item longer than its PDU")?;
        out.push((t, body));
        i += 4 + l;
    }
    Ok(out)
}

pub fn decode(kind: u8, b: &[u8]) -> Result<Pdu> {
    Ok(match kind {
        0x01 | 0x02 => {
            ensure!(b.len() >= 68, "short association PDU");
            let mut contexts = Vec::new();
            let mut accepted = Vec::new();
            let mut max_pdu = 0;
            let mut implementation = None;
            let mut app_ok = false;
            for (t, body) in items(&b[68..])? {
                match t {
                    0x10 => {
                        app_ok = std::str::from_utf8(body)
                            .map(|s| s.trim_end_matches('\0') == APPLICATION_CONTEXT)
                            .unwrap_or(false)
                    }
                    0x20 if kind == 0x01 => {
                        ensure!(body.len() >= 4, "short presentation context");
                        let mut abs = None;
                        let mut ts = Vec::new();
                        for (st, sb) in items(&body[4..])? {
                            match st {
                                0x30 => abs = Some(uid(sb)?),
                                0x40 => ts.push(uid(sb)?),
                                _ => {}
                            }
                        }
                        contexts.push(PresentationContext {
                            id: body[0],
                            abstract_syntax: abs
                                .context("presentation context without abstract syntax")?,
                            transfer_syntaxes: ts,
                        });
                    }
                    0x21 if kind == 0x02 => {
                        ensure!(body.len() >= 4, "short presentation context");
                        let ts = items(&body[4..])?
                            .into_iter()
                            .find(|(st, _)| *st == 0x40)
                            .map(|(_, sb)| uid(sb))
                            .transpose()?
                            .unwrap_or_default();
                        accepted.push((body[0], body[2], ts));
                    }
                    0x50 => {
                        for (st, sb) in items(body)? {
                            match st {
                                0x51 if sb.len() == 4 => {
                                    max_pdu = u32::from_be_bytes([sb[0], sb[1], sb[2], sb[3]])
                                }
                                0x52 => implementation = uid(sb).ok(),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            ensure!(app_ok, "unsupported application context");
            if kind == 0x01 {
                ensure!(contexts.len() <= 128, "too many presentation contexts");
                Pdu::AssociateRq(AssociateRq {
                    called: ae(&b[4..20]),
                    calling: ae(&b[20..36]),
                    contexts,
                    max_pdu,
                    implementation,
                })
            } else {
                Pdu::AssociateAc(accepted, max_pdu)
            }
        }
        0x03 => {
            ensure!(b.len() >= 4, "short A-ASSOCIATE-RJ");
            Pdu::AssociateRj {
                result: b[1],
                source: b[2],
                reason: b[3],
            }
        }
        0x04 => {
            let mut pdvs = Vec::new();
            let mut i = 0;
            while i < b.len() {
                ensure!(i + 6 <= b.len(), "truncated PDV");
                let l = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
                ensure!(l >= 2 && i + 4 + l <= b.len(), "PDV longer than its PDU");
                pdvs.push((b[i + 4], b[i + 5], b[i + 6..i + 4 + l].to_vec()));
                i += 4 + l;
            }
            Pdu::Data(pdvs)
        }
        0x05 => Pdu::ReleaseRq,
        0x06 => Pdu::ReleaseRp,
        0x07 => {
            ensure!(b.len() >= 4, "short A-ABORT");
            Pdu::Abort {
                source: b[2],
                reason: b[3],
            }
        }
        other => bail!("unknown PDU type {other:#04x}"),
    })
}

fn item(t: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![t, 0];
    v.extend_from_slice(&(body.len() as u16).to_be_bytes());
    v.extend_from_slice(body);
    v
}

fn ae_field(s: &str) -> [u8; 16] {
    let mut f = [b' '; 16];
    for (i, b) in s.bytes().take(16).enumerate() {
        f[i] = b;
    }
    f
}

fn user_info(max_pdu: u32) -> Vec<u8> {
    let mut ui = item(0x51, &max_pdu.to_be_bytes());
    ui.extend(item(0x52, IMPLEMENTATION_CLASS.as_bytes()));
    ui.extend(item(0x55, IMPLEMENTATION_VERSION.as_bytes()));
    item(0x50, &ui)
}

fn frame(kind: u8, body: Vec<u8>) -> Vec<u8> {
    let mut v = vec![kind, 0];
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend(body);
    v
}

fn assoc_head(called: &str, calling: &str) -> Vec<u8> {
    let mut b = vec![0, 1, 0, 0];
    b.extend_from_slice(&ae_field(called));
    b.extend_from_slice(&ae_field(calling));
    b.extend_from_slice(&[0u8; 32]);
    b.extend(item(0x10, APPLICATION_CONTEXT.as_bytes()));
    b
}

pub fn encode(p: &Pdu, called: &str, calling: &str) -> Vec<u8> {
    match p {
        Pdu::AssociateRq(rq) => {
            let mut b = assoc_head(&rq.called, &rq.calling);
            for c in &rq.contexts {
                let mut pc = vec![c.id, 0, 0, 0];
                pc.extend(item(0x30, c.abstract_syntax.as_bytes()));
                for ts in &c.transfer_syntaxes {
                    pc.extend(item(0x40, ts.as_bytes()));
                }
                b.extend(item(0x20, &pc));
            }
            b.extend(user_info(MAX_PDU));
            frame(0x01, b)
        }
        Pdu::AssociateAc(results, _) => {
            let mut b = assoc_head(called, calling);
            for (id, result, ts) in results {
                let mut pc = vec![*id, 0, *result, 0];
                pc.extend(item(0x40, ts.as_bytes()));
                b.extend(item(0x21, &pc));
            }
            b.extend(user_info(MAX_PDU));
            frame(0x02, b)
        }
        Pdu::AssociateRj {
            result,
            source,
            reason,
        } => frame(0x03, vec![0, *result, *source, *reason]),
        Pdu::Data(pdvs) => {
            let mut b = Vec::new();
            for (id, flags, bytes) in pdvs {
                b.extend_from_slice(&((bytes.len() + 2) as u32).to_be_bytes());
                b.push(*id);
                b.push(*flags);
                b.extend_from_slice(bytes);
            }
            frame(0x04, b)
        }
        Pdu::ReleaseRq => frame(0x05, vec![0; 4]),
        Pdu::ReleaseRp => frame(0x06, vec![0; 4]),
        Pdu::Abort { source, reason } => frame(0x07, vec![0, 0, *source, *reason]),
    }
}

/// P-DATA-TF PDUs carrying one message part (command or dataset), fragmented to fit the
/// peer's maximum PDU length.
pub fn data_pdus(context: u8, command: bool, bytes: &[u8], peer_max: u32) -> Vec<Vec<u8>> {
    let max = if peer_max == 0 {
        MAX_PDU
    } else {
        peer_max.min(MAX_PDU)
    } as usize;
    let chunk = max.saturating_sub(6).max(256);
    let mut out = Vec::new();
    let pieces: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[][..]]
    } else {
        bytes.chunks(chunk).collect()
    };
    let n = pieces.len();
    for (i, piece) in pieces.into_iter().enumerate() {
        let flags = (command as u8) | if i + 1 == n { 0x02 } else { 0 };
        out.push(encode(
            &Pdu::Data(vec![(context, flags, piece.to_vec())]),
            "",
            "",
        ));
    }
    out
}

/// A command set (group 0000) with its group length, Implicit VR Little Endian.
pub fn command(fields: &[(u32, Value)]) -> Vec<u8> {
    let mut m = Map::new();
    for (tag, v) in fields {
        m.insert(dataset::tag_key(*tag), v.clone());
    }
    let body = dataset::encode(&m, IMPLICIT_LE).expect("command sets encode");
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&4u32.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

pub fn us(v: u16) -> Value {
    json!({"vr": "US", "Value": [v]})
}

pub fn ui(v: &str) -> Value {
    json!({"vr": "UI", "Value": [v]})
}

/// A received DIMSE message: its command set and, when the command says one follows, the
/// dataset bytes, both reassembled from fragments.
#[derive(Default, Debug)]
pub struct Assembly {
    pub context: u8,
    pub command: Vec<u8>,
    pub command_done: bool,
    pub dataset: Vec<u8>,
    pub dataset_done: bool,
}

pub struct Message {
    pub context: u8,
    pub command: Map<String, Value>,
    pub dataset: Option<Vec<u8>>,
}

impl Assembly {
    /// Feed PDVs; returns a message once the command and (if announced) the dataset are whole.
    pub fn feed(&mut self, pdvs: Vec<(u8, u8, Vec<u8>)>) -> Result<Option<Message>> {
        for (ctx, flags, bytes) in pdvs {
            self.context = ctx;
            let is_command = flags & 1 == 1;
            let last = flags & 2 == 2;
            if is_command {
                ensure!(
                    !self.command_done,
                    "command fragment after the command ended"
                );
                ensure!(
                    self.command.len() + bytes.len() <= 64 * 1024,
                    "command set too large"
                );
                self.command.extend(bytes);
                self.command_done = last;
            } else {
                ensure!(self.command_done, "dataset before its command");
                ensure!(
                    self.dataset.len() + bytes.len() <= dataset::MAX_DATASET,
                    "dataset over 16 MiB"
                );
                self.dataset.extend(bytes);
                self.dataset_done = last;
            }
        }
        if !self.command_done {
            return Ok(None);
        }
        let cmd = dataset::decode(&self.command, IMPLICIT_LE)?;
        let has_dataset = cmd
            .get("00000800")
            .and_then(|v| v["Value"][0].as_u64())
            .is_some_and(|t| t != NO_DATASET as u64);
        if has_dataset && !self.dataset_done {
            return Ok(None);
        }
        let done = std::mem::take(self);
        Ok(Some(Message {
            context: done.context,
            command: cmd,
            dataset: has_dataset.then_some(done.dataset),
        }))
    }
}

pub fn field_u16(cmd: &Map<String, Value>, tag: u32) -> Option<u16> {
    cmd.get(&dataset::tag_key(tag))
        .and_then(|v| v["Value"][0].as_u64())
        .map(|v| v as u16)
}

pub fn field_str(cmd: &Map<String, Value>, tag: u32) -> Option<String> {
    dataset::text(cmd, tag)
}
