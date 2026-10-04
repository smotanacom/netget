//! Engine.IO v4 and Socket.IO v5 packet codecs, shared by the server and the client.
use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

pub const EIO_VERSION: &str = "4";
/// Engine.IO v4 separates packets in a polling payload with the record separator.
pub const SEPARATOR: char = '\u{1e}';
pub const MAX_PAYLOAD: usize = 1_000_000;
pub const MAX_PACKETS: usize = 256;
pub const MAX_EVENT_NAME: usize = 128;
pub const MAX_ARGS: usize = 16;

/// Engine.IO packet types.
#[derive(Debug, Clone, PartialEq)]
pub enum Eio {
    Open(String),
    Close,
    Ping(String),
    Pong(String),
    Message(String),
    Upgrade,
    Noop,
}

impl Eio {
    pub fn decode(s: &str) -> Result<Self> {
        let mut chars = s.chars();
        let kind = chars.next().context("empty Engine.IO packet")?;
        let rest = chars.as_str().to_owned();
        Ok(match kind {
            '0' => Eio::Open(rest),
            '1' => Eio::Close,
            '2' => Eio::Ping(rest),
            '3' => Eio::Pong(rest),
            '4' => Eio::Message(rest),
            '5' => Eio::Upgrade,
            '6' => Eio::Noop,
            'b' => bail!("binary Engine.IO packets are not supported"),
            other => bail!("unknown Engine.IO packet type {other:?}"),
        })
    }

    pub fn encode(&self) -> String {
        match self {
            Eio::Open(d) => format!("0{d}"),
            Eio::Close => "1".into(),
            Eio::Ping(d) => format!("2{d}"),
            Eio::Pong(d) => format!("3{d}"),
            Eio::Message(d) => format!("4{d}"),
            Eio::Upgrade => "5".into(),
            Eio::Noop => "6".into(),
        }
    }
}

/// Split a polling payload into packets (Engine.IO v4: record separator).
pub fn split_payload(body: &str) -> Result<Vec<Eio>> {
    ensure!(
        body.len() <= MAX_PAYLOAD,
        "payload over {MAX_PAYLOAD} bytes"
    );
    let parts: Vec<&str> = body.split(SEPARATOR).collect();
    ensure!(
        parts.len() <= MAX_PACKETS,
        "more than {MAX_PACKETS} packets in one payload"
    );
    parts.into_iter().map(Eio::decode).collect()
}

pub fn join_payload(packets: &[String]) -> String {
    packets.join(&SEPARATOR.to_string())
}

/// Socket.IO packet types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Connect = 0,
    Disconnect = 1,
    Event = 2,
    Ack = 3,
    ConnectError = 4,
    BinaryEvent = 5,
    BinaryAck = 6,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sio {
    pub kind: Kind,
    pub nsp: String,
    pub id: Option<u64>,
    pub data: Option<Value>,
}

impl Sio {
    pub fn new(kind: Kind, nsp: &str, id: Option<u64>, data: Option<Value>) -> Self {
        Self {
            kind,
            nsp: nsp.to_owned(),
            id,
            data,
        }
    }

    /// `<type>[<attachments>-][<nsp>,][<id>][<json>]`.
    pub fn decode(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        let kind = match b.first().copied() {
            Some(b'0') => Kind::Connect,
            Some(b'1') => Kind::Disconnect,
            Some(b'2') => Kind::Event,
            Some(b'3') => Kind::Ack,
            Some(b'4') => Kind::ConnectError,
            Some(b'5') => Kind::BinaryEvent,
            Some(b'6') => Kind::BinaryAck,
            _ => bail!("unknown Socket.IO packet type"),
        };
        ensure!(
            !matches!(kind, Kind::BinaryEvent | Kind::BinaryAck),
            "binary Socket.IO packets are not supported"
        );
        let mut i = 1;
        let mut nsp = "/".to_owned();
        if b.get(i) == Some(&b'/') {
            let end = s[i..].find(',').map(|e| e + i).unwrap_or(s.len());
            nsp = s[i..end].to_owned();
            ensure!(
                namespace_ok(&nsp),
                "namespace must be / followed by letters, digits, -, _ or ."
            );
            i = (end + 1).min(s.len());
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let id = if i > start {
            ensure!(i - start <= 15, "acknowledgement id too long");
            Some(s[start..i].parse::<u64>()?)
        } else {
            None
        };
        let data = if i < b.len() {
            let v: Value =
                serde_json::from_str(&s[i..]).context("Socket.IO payload is not JSON")?;
            ensure!(
                crate::utils::json_budget::within_budget(&v, MAX_PAYLOAD, 20_000, 32),
                "Socket.IO payload exceeds the bounds"
            );
            Some(v)
        } else {
            None
        };
        Ok(Self {
            kind,
            nsp,
            id,
            data,
        })
    }

    pub fn encode(&self) -> String {
        let mut out = (self.kind as u8).to_string();
        if self.nsp != "/" {
            out.push_str(&self.nsp);
            out.push(',');
        }
        if let Some(id) = self.id {
            out.push_str(&id.to_string());
        }
        if let Some(d) = &self.data {
            out.push_str(&d.to_string());
        }
        out
    }

    /// An EVENT's name and arguments.
    pub fn event(&self) -> Result<(String, Vec<Value>)> {
        let list = self
            .data
            .as_ref()
            .and_then(Value::as_array)
            .context("an event payload is an array")?;
        let name = list
            .first()
            .and_then(Value::as_str)
            .context("an event starts with its name")?;
        ensure!(
            !name.is_empty() && name.len() <= MAX_EVENT_NAME,
            "event name must be 1..{MAX_EVENT_NAME} characters"
        );
        ensure!(list.len() - 1 <= MAX_ARGS, "at most {MAX_ARGS} arguments");
        Ok((name.to_owned(), list[1..].to_vec()))
    }
}

/// Event names Socket.IO reserves on the wire.
pub fn reserved(name: &str) -> bool {
    matches!(
        name,
        "connect"
            | "connect_error"
            | "disconnect"
            | "disconnecting"
            | "newListener"
            | "removeListener"
    )
}

pub fn namespace_ok(nsp: &str) -> bool {
    nsp.starts_with('/')
        && nsp.len() <= 128
        && nsp
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
}

/// A random session / socket id (20 URL-safe characters, as Engine.IO uses).
pub fn new_id() -> String {
    use base64::Engine;
    let raw: [u8; 15] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}
