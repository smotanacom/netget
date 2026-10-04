//! MQTT-SN v1.2 packets: the 1- or 3-byte length header, every message type, flags, the three
//! topic id types and forwarder encapsulation (section 5.5).
use anyhow::{bail, ensure, Context, Result};

pub const PROTOCOL_ID: u8 = 0x01;
/// Return codes (section 5.3.10).
pub const ACCEPTED: u8 = 0x00;
pub const CONGESTION: u8 = 0x01;
pub const INVALID_TOPIC_ID: u8 = 0x02;
pub const NOT_SUPPORTED: u8 = 0x03;

pub fn return_code_name(rc: u8) -> &'static str {
    match rc {
        ACCEPTED => "accepted",
        CONGESTION => "congestion",
        INVALID_TOPIC_ID => "invalid_topic_id",
        NOT_SUPPORTED => "not_supported",
        _ => "unknown",
    }
}

pub fn return_code(name: &str) -> Option<u8> {
    [CONGESTION, INVALID_TOPIC_ID, NOT_SUPPORTED]
        .into_iter()
        .find(|rc| return_code_name(*rc) == name)
}

/// QoS as carried in the flags: 0, 1, 2 or -1 (publish without a connection).
pub type Qos = i8;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Flags {
    pub dup: bool,
    pub qos: Qos,
    pub retain: bool,
    pub will: bool,
    pub clean_session: bool,
    /// 0 normal topic id or name, 1 predefined id, 2 short name.
    pub topic_id_type: u8,
}

impl Flags {
    pub fn decode(b: u8) -> Self {
        Self {
            dup: b & 0x80 != 0,
            qos: match (b >> 5) & 0x03 {
                0 => 0,
                1 => 1,
                2 => 2,
                _ => -1,
            },
            retain: b & 0x10 != 0,
            will: b & 0x08 != 0,
            clean_session: b & 0x04 != 0,
            topic_id_type: b & 0x03,
        }
    }
    pub fn encode(&self) -> u8 {
        let q = match self.qos {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 3,
        };
        (u8::from(self.dup) << 7)
            | (q << 5)
            | (u8::from(self.retain) << 4)
            | (u8::from(self.will) << 3)
            | (u8::from(self.clean_session) << 2)
            | (self.topic_id_type & 0x03)
    }
}

pub const TOPIC_NORMAL: u8 = 0;
pub const TOPIC_PREDEFINED: u8 = 1;
pub const TOPIC_SHORT: u8 = 2;

#[derive(Clone, Debug, PartialEq)]
pub enum Packet {
    Advertise {
        gw_id: u8,
        duration: u16,
    },
    SearchGw {
        radius: u8,
    },
    GwInfo {
        gw_id: u8,
        gw_add: Vec<u8>,
    },
    Connect {
        flags: Flags,
        duration: u16,
        client_id: String,
    },
    ConnAck {
        rc: u8,
    },
    WillTopicReq,
    WillTopic {
        flags: Flags,
        topic: String,
    },
    WillMsgReq,
    WillMsg {
        msg: Vec<u8>,
    },
    Register {
        topic_id: u16,
        msg_id: u16,
        topic: String,
    },
    RegAck {
        topic_id: u16,
        msg_id: u16,
        rc: u8,
    },
    /// `topic` is the topic id, the predefined id, or the two short-name bytes, per the flags.
    Publish {
        flags: Flags,
        topic: u16,
        msg_id: u16,
        data: Vec<u8>,
    },
    PubAck {
        topic_id: u16,
        msg_id: u16,
        rc: u8,
    },
    PubComp {
        msg_id: u16,
    },
    PubRec {
        msg_id: u16,
    },
    PubRel {
        msg_id: u16,
    },
    /// A topic name (normal), or an id / short name in `topic_id`.
    Subscribe {
        flags: Flags,
        msg_id: u16,
        topic_name: Option<String>,
        topic_id: u16,
    },
    SubAck {
        flags: Flags,
        topic_id: u16,
        msg_id: u16,
        rc: u8,
    },
    Unsubscribe {
        flags: Flags,
        msg_id: u16,
        topic_name: Option<String>,
        topic_id: u16,
    },
    UnsubAck {
        msg_id: u16,
    },
    PingReq {
        client_id: Option<String>,
    },
    PingResp,
    Disconnect {
        duration: Option<u16>,
    },
    WillTopicUpd {
        flags: Flags,
        topic: String,
    },
    WillTopicResp {
        rc: u8,
    },
    WillMsgUpd {
        msg: Vec<u8>,
    },
    WillMsgResp {
        rc: u8,
    },
}

impl Packet {
    pub fn name(&self) -> &'static str {
        match self {
            Packet::Advertise { .. } => "ADVERTISE",
            Packet::SearchGw { .. } => "SEARCHGW",
            Packet::GwInfo { .. } => "GWINFO",
            Packet::Connect { .. } => "CONNECT",
            Packet::ConnAck { .. } => "CONNACK",
            Packet::WillTopicReq => "WILLTOPICREQ",
            Packet::WillTopic { .. } => "WILLTOPIC",
            Packet::WillMsgReq => "WILLMSGREQ",
            Packet::WillMsg { .. } => "WILLMSG",
            Packet::Register { .. } => "REGISTER",
            Packet::RegAck { .. } => "REGACK",
            Packet::Publish { .. } => "PUBLISH",
            Packet::PubAck { .. } => "PUBACK",
            Packet::PubComp { .. } => "PUBCOMP",
            Packet::PubRec { .. } => "PUBREC",
            Packet::PubRel { .. } => "PUBREL",
            Packet::Subscribe { .. } => "SUBSCRIBE",
            Packet::SubAck { .. } => "SUBACK",
            Packet::Unsubscribe { .. } => "UNSUBSCRIBE",
            Packet::UnsubAck { .. } => "UNSUBACK",
            Packet::PingReq { .. } => "PINGREQ",
            Packet::PingResp => "PINGRESP",
            Packet::Disconnect { .. } => "DISCONNECT",
            Packet::WillTopicUpd { .. } => "WILLTOPICUPD",
            Packet::WillTopicResp { .. } => "WILLTOPICRESP",
            Packet::WillMsgUpd { .. } => "WILLMSGUPD",
            Packet::WillMsgResp { .. } => "WILLMSGRESP",
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> Result<u16> {
    ensure!(b.len() >= i + 2, "the message is truncated");
    Ok(u16::from_be_bytes([b[i], b[i + 1]]))
}

fn text(b: &[u8]) -> Result<String> {
    String::from_utf8(b.to_vec()).context("a topic or client id is not UTF-8")
}

/// The message at the front of a datagram: (body after the header, message type, total length).
fn header(d: &[u8]) -> Result<(usize, u8, usize)> {
    ensure!(d.len() >= 2, "the datagram is shorter than a header");
    let (len, at) = if d[0] == 0x01 {
        ensure!(d.len() >= 4, "the 3-byte length is truncated");
        (u16::from_be_bytes([d[1], d[2]]) as usize, 3)
    } else {
        (d[0] as usize, 1)
    };
    ensure!(
        len > at && len <= d.len(),
        "the length field {len} does not fit the datagram"
    );
    Ok((at + 1, d[at], len))
}

/// A forwarder-encapsulated message's wireless node id and the message inside.
pub fn unwrap_forwarder(d: &[u8]) -> Result<Option<(Vec<u8>, &[u8])>> {
    let (body, kind, len) = header(d)?;
    if kind != 0xFE {
        return Ok(None);
    }
    ensure!(len > body, "the encapsulation has no control byte");
    Ok(Some((d[body + 1..len].to_vec(), &d[len..])))
}

pub fn wrap_forwarder(node: &[u8], inner: &[u8]) -> Vec<u8> {
    let mut out = vec![(3 + node.len()) as u8, 0xFE, 0x00];
    out.extend(node);
    out.extend(inner);
    out
}

pub fn decode(d: &[u8]) -> Result<Packet> {
    let (at, kind, len) = header(d)?;
    ensure!(
        len == d.len(),
        "the datagram carries {} bytes after the message",
        d.len() - len
    );
    let b = &d[at..len];
    let need = |n: usize| -> Result<()> {
        ensure!(b.len() >= n, "the {kind:#04x} message is truncated");
        Ok(())
    };
    Ok(match kind {
        0x00 => {
            need(3)?;
            Packet::Advertise {
                gw_id: b[0],
                duration: u16_at(b, 1)?,
            }
        }
        0x01 => {
            need(1)?;
            Packet::SearchGw { radius: b[0] }
        }
        0x02 => {
            need(1)?;
            Packet::GwInfo {
                gw_id: b[0],
                gw_add: b[1..].to_vec(),
            }
        }
        0x04 => {
            need(4)?;
            ensure!(
                b[1] == PROTOCOL_ID,
                "protocol id {:#04x} is not MQTT-SN 1.2",
                b[1]
            );
            let client_id = text(&b[4..])?;
            ensure!(
                !client_id.is_empty() && client_id.len() <= 23,
                "the client id is 1 to 23 characters"
            );
            Packet::Connect {
                flags: Flags::decode(b[0]),
                duration: u16_at(b, 2)?,
                client_id,
            }
        }
        0x05 => {
            need(1)?;
            Packet::ConnAck { rc: b[0] }
        }
        0x06 => Packet::WillTopicReq,
        0x07 => {
            if b.is_empty() {
                Packet::WillTopic {
                    flags: Flags::default(),
                    topic: String::new(),
                }
            } else {
                Packet::WillTopic {
                    flags: Flags::decode(b[0]),
                    topic: text(&b[1..])?,
                }
            }
        }
        0x08 => Packet::WillMsgReq,
        0x09 => Packet::WillMsg { msg: b.to_vec() },
        0x0A => {
            need(4)?;
            Packet::Register {
                topic_id: u16_at(b, 0)?,
                msg_id: u16_at(b, 2)?,
                topic: text(&b[4..])?,
            }
        }
        0x0B => {
            need(5)?;
            Packet::RegAck {
                topic_id: u16_at(b, 0)?,
                msg_id: u16_at(b, 2)?,
                rc: b[4],
            }
        }
        0x0C => {
            need(5)?;
            Packet::Publish {
                flags: Flags::decode(b[0]),
                topic: u16_at(b, 1)?,
                msg_id: u16_at(b, 3)?,
                data: b[5..].to_vec(),
            }
        }
        0x0D => {
            need(5)?;
            Packet::PubAck {
                topic_id: u16_at(b, 0)?,
                msg_id: u16_at(b, 2)?,
                rc: b[4],
            }
        }
        0x0E => Packet::PubComp {
            msg_id: u16_at(b, 0)?,
        },
        0x0F => Packet::PubRec {
            msg_id: u16_at(b, 0)?,
        },
        0x10 => Packet::PubRel {
            msg_id: u16_at(b, 0)?,
        },
        0x12 | 0x14 => {
            need(3)?;
            let flags = Flags::decode(b[0]);
            let msg_id = u16_at(b, 1)?;
            let (topic_name, topic_id) = if flags.topic_id_type == TOPIC_NORMAL {
                let t = text(&b[3..])?;
                ensure!(!t.is_empty(), "the topic name is empty");
                (Some(t), 0)
            } else {
                (None, u16_at(b, 3)?)
            };
            if kind == 0x12 {
                Packet::Subscribe {
                    flags,
                    msg_id,
                    topic_name,
                    topic_id,
                }
            } else {
                Packet::Unsubscribe {
                    flags,
                    msg_id,
                    topic_name,
                    topic_id,
                }
            }
        }
        0x13 => {
            need(6)?;
            Packet::SubAck {
                flags: Flags::decode(b[0]),
                topic_id: u16_at(b, 1)?,
                msg_id: u16_at(b, 3)?,
                rc: b[5],
            }
        }
        0x15 => Packet::UnsubAck {
            msg_id: u16_at(b, 0)?,
        },
        0x16 => Packet::PingReq {
            client_id: if b.is_empty() { None } else { Some(text(b)?) },
        },
        0x17 => Packet::PingResp,
        0x18 => Packet::Disconnect {
            duration: if b.is_empty() {
                None
            } else {
                Some(u16_at(b, 0)?)
            },
        },
        0x1A => {
            if b.is_empty() {
                Packet::WillTopicUpd {
                    flags: Flags::default(),
                    topic: String::new(),
                }
            } else {
                Packet::WillTopicUpd {
                    flags: Flags::decode(b[0]),
                    topic: text(&b[1..])?,
                }
            }
        }
        0x1B => {
            need(1)?;
            Packet::WillTopicResp { rc: b[0] }
        }
        0x1C => Packet::WillMsgUpd { msg: b.to_vec() },
        0x1D => {
            need(1)?;
            Packet::WillMsgResp { rc: b[0] }
        }
        other => bail!("message type {other:#04x} is not MQTT-SN"),
    })
}

pub fn encode(p: &Packet) -> Vec<u8> {
    let mut b: Vec<u8> = Vec::new();
    let kind: u8 = match p {
        Packet::Advertise { gw_id, duration } => {
            b.push(*gw_id);
            b.extend(duration.to_be_bytes());
            0x00
        }
        Packet::SearchGw { radius } => {
            b.push(*radius);
            0x01
        }
        Packet::GwInfo { gw_id, gw_add } => {
            b.push(*gw_id);
            b.extend(gw_add);
            0x02
        }
        Packet::Connect {
            flags,
            duration,
            client_id,
        } => {
            b.push(flags.encode());
            b.push(PROTOCOL_ID);
            b.extend(duration.to_be_bytes());
            b.extend(client_id.as_bytes());
            0x04
        }
        Packet::ConnAck { rc } => {
            b.push(*rc);
            0x05
        }
        Packet::WillTopicReq => 0x06,
        Packet::WillTopic { flags, topic } => {
            if !topic.is_empty() {
                b.push(flags.encode());
                b.extend(topic.as_bytes());
            }
            0x07
        }
        Packet::WillMsgReq => 0x08,
        Packet::WillMsg { msg } => {
            b.extend(msg);
            0x09
        }
        Packet::Register {
            topic_id,
            msg_id,
            topic,
        } => {
            b.extend(topic_id.to_be_bytes());
            b.extend(msg_id.to_be_bytes());
            b.extend(topic.as_bytes());
            0x0A
        }
        Packet::RegAck {
            topic_id,
            msg_id,
            rc,
        } => {
            b.extend(topic_id.to_be_bytes());
            b.extend(msg_id.to_be_bytes());
            b.push(*rc);
            0x0B
        }
        Packet::Publish {
            flags,
            topic,
            msg_id,
            data,
        } => {
            b.push(flags.encode());
            b.extend(topic.to_be_bytes());
            b.extend(msg_id.to_be_bytes());
            b.extend(data);
            0x0C
        }
        Packet::PubAck {
            topic_id,
            msg_id,
            rc,
        } => {
            b.extend(topic_id.to_be_bytes());
            b.extend(msg_id.to_be_bytes());
            b.push(*rc);
            0x0D
        }
        Packet::PubComp { msg_id } => {
            b.extend(msg_id.to_be_bytes());
            0x0E
        }
        Packet::PubRec { msg_id } => {
            b.extend(msg_id.to_be_bytes());
            0x0F
        }
        Packet::PubRel { msg_id } => {
            b.extend(msg_id.to_be_bytes());
            0x10
        }
        Packet::Subscribe {
            flags,
            msg_id,
            topic_name,
            topic_id,
        }
        | Packet::Unsubscribe {
            flags,
            msg_id,
            topic_name,
            topic_id,
        } => {
            b.push(flags.encode());
            b.extend(msg_id.to_be_bytes());
            match topic_name {
                Some(t) if flags.topic_id_type == TOPIC_NORMAL => b.extend(t.as_bytes()),
                _ => b.extend(topic_id.to_be_bytes()),
            }
            if matches!(p, Packet::Subscribe { .. }) {
                0x12
            } else {
                0x14
            }
        }
        Packet::SubAck {
            flags,
            topic_id,
            msg_id,
            rc,
        } => {
            b.push(flags.encode());
            b.extend(topic_id.to_be_bytes());
            b.extend(msg_id.to_be_bytes());
            b.push(*rc);
            0x13
        }
        Packet::UnsubAck { msg_id } => {
            b.extend(msg_id.to_be_bytes());
            0x15
        }
        Packet::PingReq { client_id } => {
            if let Some(c) = client_id {
                b.extend(c.as_bytes());
            }
            0x16
        }
        Packet::PingResp => 0x17,
        Packet::Disconnect { duration } => {
            if let Some(d) = duration {
                b.extend(d.to_be_bytes());
            }
            0x18
        }
        Packet::WillTopicUpd { flags, topic } => {
            if !topic.is_empty() {
                b.push(flags.encode());
                b.extend(topic.as_bytes());
            }
            0x1A
        }
        Packet::WillTopicResp { rc } => {
            b.push(*rc);
            0x1B
        }
        Packet::WillMsgUpd { msg } => {
            b.extend(msg);
            0x1C
        }
        Packet::WillMsgResp { rc } => {
            b.push(*rc);
            0x1D
        }
    };
    let mut out = if b.len() + 2 <= 255 {
        vec![(b.len() + 2) as u8]
    } else {
        let mut h = vec![0x01];
        h.extend(((b.len() + 4) as u16).to_be_bytes());
        h
    };
    out.push(kind);
    out.extend(b);
    out
}

/// A short topic name's two bytes as a u16.
pub fn short_topic(name: &str) -> Option<u16> {
    let b = name.as_bytes();
    (b.len() == 2 && !name.contains(['+', '#'])).then(|| u16::from_be_bytes([b[0], b[1]]))
}

pub fn short_name(id: u16) -> String {
    String::from_utf8_lossy(&id.to_be_bytes()).into_owned()
}

/// MQTT topic filter matching ('+' one level, '#' the rest); wildcards at the first level do
/// not match topics starting with '$'.
pub fn matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && filter.starts_with(['#', '+']) {
        return false;
    }
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub fn valid_filter(filter: &str) -> bool {
    !filter.is_empty()
        && filter.len() <= 1024
        && filter.split('/').enumerate().all(|(i, level)| {
            (level == "#" && i == filter.split('/').count() - 1)
                || level == "+"
                || !level.contains(['+', '#'])
        })
}

pub fn valid_topic(topic: &str) -> bool {
    !topic.is_empty() && topic.len() <= 1024 && !topic.contains(['+', '#', '\0'])
}
