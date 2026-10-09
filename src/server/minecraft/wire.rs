//! Minecraft Java Edition framing for the Server List Ping and the front of login: VarInt
//! length-prefixed packets, the handshake, status JSON, ping/pong, Login Start, the login
//! Disconnect, and the pre-1.7 legacy ping (`0xFE`) with its `0xFF` kick reply.
//!
//! Shared by the server and the client. Every length the peer announces is checked against a
//! bound before anything is allocated for it.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest packet a client may send the server (handshake, Login Start, ping are all tiny).
pub const MAX_SERVERBOUND_PACKET: usize = 2048;
/// Largest packet the client accepts: a status response is one String of at most 32767 chars.
pub const MAX_CLIENTBOUND_PACKET: usize = MAX_JSON_CHARS * 3 + 8;
/// A Java `String` field holds at most 32767 UTF-16 units; the status JSON and a disconnect
/// reason are both bounded by it.
pub const MAX_JSON_CHARS: usize = 32767;
/// Handshake server address, in characters.
pub const MAX_ADDRESS_CHARS: usize = 255;
/// Login Start player name, in characters.
pub const MAX_USERNAME_CHARS: usize = 16;
/// Players listed in a status sample.
pub const MAX_SAMPLE: usize = 64;
/// Deadline for each read and write, as the vanilla server's read timeout.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the legacy ping's optional tail (`01`, then `FA` + MC|PingHost) is waited for.
pub const LEGACY_TAIL_WAIT: Duration = Duration::from_millis(300);
/// Bytes of a 1.6 MC|PingHost payload accepted.
pub const MAX_LEGACY_PAYLOAD: usize = 7 + MAX_ADDRESS_CHARS * 2;
/// The version reported when the handler names none: 1.21.1.
pub const DEFAULT_VERSION_NAME: &str = "1.21.1";
pub const DEFAULT_PROTOCOL: i32 = 767;
/// Protocol a 1.6 legacy ping carries.
pub const LEGACY_PING_PROTOCOL: u8 = 74;

pub const STATE_STATUS: i32 = 1;
pub const STATE_LOGIN: i32 = 2;
pub const STATE_TRANSFER: i32 = 3;

pub fn put_varint(out: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if v & !0x7f == 0 {
            out.push(v as u8);
            return;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
}

/// Decode a VarInt from the front of `buf`: (value, bytes used).
pub fn get_varint(buf: &[u8]) -> Result<(i32, usize)> {
    let mut value = 0u32;
    for (i, byte) in buf.iter().take(5).enumerate() {
        value |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value as i32, i + 1));
        }
    }
    ensure!(buf.len() >= 5, "truncated VarInt");
    bail!("VarInt longer than 5 bytes")
}

pub fn put_string(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as i32);
    out.extend_from_slice(s.as_bytes());
}

/// A cursor over one packet's body.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
    pub fn remaining(&self) -> &'a [u8] {
        self.buf
    }
    pub fn varint(&mut self) -> Result<i32> {
        let (v, n) = get_varint(self.buf)?;
        self.buf = &self.buf[n..];
        Ok(v)
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.buf.len() >= n, "packet ends inside a field");
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.bytes(8)?.try_into()?))
    }
    /// A String field of at most `max_chars` characters (`max_chars * 3` bytes on the wire).
    pub fn string(&mut self, max_chars: usize) -> Result<String> {
        let len = self.varint()?;
        ensure!(
            len >= 0 && len as usize <= max_chars * 3,
            "string of {len} bytes exceeds {max_chars} characters"
        );
        let s = std::str::from_utf8(self.bytes(len as usize)?)
            .context("string is not UTF-8")?
            .to_string();
        ensure!(
            s.encode_utf16().count() <= max_chars,
            "string exceeds {max_chars} characters"
        );
        Ok(s)
    }
}

/// A complete packet (length prefix included) from an id and body.
pub fn frame(id: i32, body: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(body.len() + 5);
    put_varint(&mut inner, id);
    inner.extend_from_slice(body);
    let mut out = Vec::with_capacity(inner.len() + 5);
    put_varint(&mut out, inner.len() as i32);
    out.extend_from_slice(&inner);
    out
}

/// Read a VarInt byte by byte. `None` on a clean EOF before the first byte.
async fn read_varint<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<i32>> {
    let mut value = 0u32;
    for i in 0..5 {
        let byte = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if i == 0 && e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        value |= u32::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(Some(value as i32));
        }
    }
    bail!("VarInt longer than 5 bytes")
}

/// Read one packet's bytes after its length prefix; the announced length is checked against
/// `max` before any of it is read. `None` on a clean EOF between packets.
pub async fn read_raw<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
    deadline: Duration,
) -> Result<Option<Vec<u8>>> {
    tokio::time::timeout(deadline, async {
        let Some(len) = read_varint(reader).await? else {
            return Ok(None);
        };
        ensure!(len > 0, "packet length {len} is not positive");
        ensure!(len as usize <= max, "packet of {len} bytes exceeds {max}");
        let mut buf = vec![0u8; len as usize];
        reader.read_exact(&mut buf).await?;
        Ok(Some(buf))
    })
    .await
    .context("Minecraft read deadline")?
}

/// Split an uncompressed packet into its id and body.
pub fn split_id(buf: &[u8]) -> Result<(i32, Vec<u8>)> {
    let (id, n) = get_varint(buf)?;
    Ok((id, buf[n..].to_vec()))
}

/// Read one uncompressed packet's id and body. `None` on a clean EOF between packets.
pub async fn read_packet<R: AsyncRead + Unpin>(
    reader: &mut R,
    max: usize,
    deadline: Duration,
) -> Result<Option<(i32, Vec<u8>)>> {
    match read_raw(reader, max, deadline).await? {
        Some(buf) => Ok(Some(split_id(&buf)?)),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub protocol_version: i32,
    pub server_address: String,
    pub server_port: u16,
    pub next_state: i32,
}

pub fn parse_handshake(body: &[u8]) -> Result<Handshake> {
    let mut r = Reader::new(body);
    let protocol_version = r.varint()?;
    let server_address = r.string(MAX_ADDRESS_CHARS)?;
    let server_port = r.u16()?;
    let next_state = r.varint()?;
    ensure!(
        matches!(next_state, STATE_STATUS | STATE_LOGIN | STATE_TRANSFER),
        "handshake asks for unknown state {next_state}"
    );
    Ok(Handshake {
        protocol_version,
        server_address,
        server_port,
        next_state,
    })
}

pub fn encode_handshake(h: &Handshake) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(&mut body, h.protocol_version);
    put_string(&mut body, &h.server_address);
    body.extend_from_slice(&h.server_port.to_be_bytes());
    put_varint(&mut body, h.next_state);
    frame(0x00, &body)
}

/// Login Start: the name, and the UUID when the version's layout carries one. The layout
/// changed four times; the trailing bytes are read by shape rather than by version number.
pub fn parse_login_start(body: &[u8]) -> Result<(String, Option<String>)> {
    let mut r = Reader::new(body);
    let name = r.string(MAX_USERNAME_CHARS)?;
    ensure!(!name.is_empty(), "empty player name");
    let rest = r.remaining();
    let uuid = match rest.len() {
        16 => Some(format_uuid(rest)),
        17 if rest[0] == 1 => Some(format_uuid(&rest[1..])),
        _ => None,
    };
    Ok((name, uuid))
}

/// Login Start as a client of `protocol` sends it.
pub fn encode_login_start(protocol: i32, name: &str, uuid: &[u8; 16]) -> Vec<u8> {
    let mut body = Vec::new();
    put_string(&mut body, name);
    match protocol {
        // 1.20.2+: the UUID, always present.
        764.. => body.extend_from_slice(uuid),
        // 1.19.3 – 1.20.1: optional UUID.
        761..=763 => {
            body.push(1);
            body.extend_from_slice(uuid);
        }
        // 1.19.1/1.19.2: no signature data, no UUID.
        760 => body.extend_from_slice(&[0, 0]),
        // 1.19: no signature data.
        759 => body.push(0),
        _ => {}
    }
    frame(0x00, &body)
}

pub fn format_uuid(b: &[u8]) -> String {
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    ensure!(
        hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()),
        "uuid must be 32 hex digits, optionally hyphenated"
    );
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}

/// The status JSON for a `minecraft_status` action; `client_protocol` is unused unless the
/// handler asks to echo it (`protocol: "echo"`).
pub fn status_json(answer: &Value, client_protocol: i32) -> Result<String> {
    let name = answer["version_name"]
        .as_str()
        .unwrap_or(DEFAULT_VERSION_NAME);
    let protocol = match &answer["protocol"] {
        Value::Null => DEFAULT_PROTOCOL as i64,
        Value::String(s) if s == "echo" => client_protocol as i64,
        v => v
            .as_i64()
            .context("protocol must be a number or \"echo\"")?,
    };
    let max = answer["max_players"].as_i64().unwrap_or(20);
    let online = answer["online_players"].as_i64().unwrap_or(0);
    let mut players = json!({"max": max, "online": online});
    if let Some(sample) = answer["sample"].as_array() {
        ensure!(
            sample.len() <= MAX_SAMPLE,
            "sample lists more than {MAX_SAMPLE} players"
        );
        let mut out = Vec::new();
        for p in sample {
            let player = p["name"]
                .as_str()
                .context("each sample entry needs a name")?;
            let id = match p["id"].as_str() {
                Some(id) => format_uuid(&parse_uuid(id)?),
                None => "00000000-0000-0000-0000-000000000000".to_string(),
            };
            out.push(json!({"name": player, "id": id}));
        }
        players["sample"] = Value::Array(out);
    }
    let mut status = json!({
        "version": {"name": name, "protocol": protocol},
        "players": players,
        "description": {"text": answer["motd"].as_str().unwrap_or("")},
    });
    if let Some(flag) = answer["enforces_secure_chat"].as_bool() {
        status["enforcesSecureChat"] = Value::Bool(flag);
    }
    let text = status.to_string();
    ensure!(
        text.encode_utf16().count() <= MAX_JSON_CHARS,
        "status JSON exceeds {MAX_JSON_CHARS} characters"
    );
    Ok(text)
}

/// A login Disconnect reason as the JSON text component the client renders.
pub fn disconnect_json(reason: &str) -> Result<String> {
    let text = json!({"text": reason}).to_string();
    ensure!(
        text.encode_utf16().count() <= MAX_JSON_CHARS,
        "disconnect reason exceeds {MAX_JSON_CHARS} characters"
    );
    Ok(text)
}

/// Flatten a chat component (a string, an object with text/translate/extra, or an array) to
/// plain text. Nesting beyond 32 levels is cut off rather than followed.
pub fn plain_text(component: &Value) -> String {
    fn walk(v: &Value, depth: usize, out: &mut String) {
        if depth > 32 {
            return;
        }
        match v {
            Value::String(s) => out.push_str(s),
            Value::Array(items) => items.iter().for_each(|i| walk(i, depth + 1, out)),
            Value::Object(map) => {
                if let Some(t) = map.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                } else if let Some(t) = map.get("translate").and_then(Value::as_str) {
                    out.push_str(t);
                }
                if let Some(extra) = map.get("extra") {
                    walk(extra, depth + 1, out);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(component, 0, &mut out);
    out
}

/// What a legacy (`0xFE`) ping carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyPing {
    /// `0xFE 0x01` (1.4 and later) rather than a bare `0xFE` (Beta 1.8 – 1.3).
    pub modern_reply: bool,
    /// From a 1.6 MC|PingHost payload, when one was sent.
    pub protocol_version: Option<u8>,
    pub server_address: Option<String>,
    pub server_port: Option<u32>,
}

/// Read the rest of a legacy ping after its `0xFE`: the optional `0x01`, then the optional
/// 1.6 `0xFA` MC|PingHost plugin message. Each tail is waited for briefly, as the vanilla
/// server does, because older clients send nothing more.
pub async fn read_legacy_ping<R: AsyncRead + Unpin>(reader: &mut R) -> Result<LegacyPing> {
    let mut ping = LegacyPing {
        modern_reply: false,
        protocol_version: None,
        server_address: None,
        server_port: None,
    };
    match tokio::time::timeout(LEGACY_TAIL_WAIT, reader.read_u8()).await {
        Ok(Ok(0x01)) => ping.modern_reply = true,
        Ok(Ok(other)) => bail!("legacy ping followed by 0x{other:02x}"),
        Ok(Err(_)) | Err(_) => return Ok(ping),
    }
    match tokio::time::timeout(LEGACY_TAIL_WAIT, reader.read_u8()).await {
        Ok(Ok(0xFA)) => {}
        Ok(Ok(other)) => bail!("legacy ping tail starts with 0x{other:02x}"),
        Ok(Err(_)) | Err(_) => return Ok(ping),
    }
    // Some clients (mcstatus among them) send `FE 01 FA` and nothing more; answer them too.
    let first = match tokio::time::timeout(LEGACY_TAIL_WAIT, reader.read_u8()).await {
        Ok(Ok(b)) => b,
        Ok(Err(_)) | Err(_) => return Ok(ping),
    };
    tokio::time::timeout(IO_TIMEOUT, async {
        let channel_len = u16::from_be_bytes([first, reader.read_u8().await?]) as usize;
        ensure!(
            channel_len == 11,
            "legacy plugin channel is not MC|PingHost"
        );
        let mut channel = vec![0u8; channel_len * 2];
        reader.read_exact(&mut channel).await?;
        ensure!(
            utf16be(&channel)? == "MC|PingHost",
            "legacy plugin channel is not MC|PingHost"
        );
        let len = reader.read_u16().await? as usize;
        ensure!(
            (7..=MAX_LEGACY_PAYLOAD).contains(&len),
            "MC|PingHost payload of {len} bytes"
        );
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).await?;
        let mut r = Reader::new(&payload);
        let protocol = r.bytes(1)?[0];
        let host_len = r.u16()? as usize;
        ensure!(host_len <= MAX_ADDRESS_CHARS, "MC|PingHost host too long");
        let host = utf16be(r.bytes(host_len * 2)?)?;
        let port = u32::from_be_bytes(r.bytes(4)?.try_into()?);
        ping.protocol_version = Some(protocol);
        ping.server_address = Some(host);
        ping.server_port = Some(port);
        Ok(ping)
    })
    .await
    .context("legacy ping deadline")?
}

fn utf16be(bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len().is_multiple_of(2), "odd UTF-16 length");
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).context("invalid UTF-16")
}

/// The 1.6 client's legacy ping: `FE 01 FA` + MC|PingHost naming `host:port`.
pub fn encode_legacy_ping(host: &str, port: u16) -> Vec<u8> {
    let mut out = vec![0xFE, 0x01, 0xFA];
    out.extend_from_slice(&11u16.to_be_bytes());
    out.extend("MC|PingHost".encode_utf16().flat_map(u16::to_be_bytes));
    let host16: Vec<u8> = host.encode_utf16().flat_map(u16::to_be_bytes).collect();
    out.extend_from_slice(&((7 + host16.len()) as u16).to_be_bytes());
    out.push(LEGACY_PING_PROTOCOL);
    out.extend_from_slice(&((host16.len() / 2) as u16).to_be_bytes());
    out.extend_from_slice(&host16);
    out.extend_from_slice(&u32::from(port).to_be_bytes());
    out
}

/// The `0xFF` kick packet answering a legacy ping. 1.4+ clients get
/// `§1\0protocol\0version\0motd\0online\0max`; a bare `0xFE` gets `motd§online§max`, where
/// `§` is the separator and is therefore removed from the MOTD.
pub fn encode_legacy_reply(ping: &LegacyPing, answer: &Value) -> Result<Vec<u8>> {
    let motd = answer["motd"].as_str().unwrap_or("");
    let online = answer["online_players"].as_i64().unwrap_or(0);
    let max = answer["max_players"].as_i64().unwrap_or(20);
    let text = if ping.modern_reply {
        let protocol = match &answer["protocol"] {
            Value::Number(n) => n.to_string(),
            _ => ping
                .protocol_version
                .unwrap_or(LEGACY_PING_PROTOCOL)
                .to_string(),
        };
        let name = answer["version_name"].as_str().unwrap_or("1.6.4");
        format!("§1\0{protocol}\0{name}\0{motd}\0{online}\0{max}")
    } else {
        format!("{}§{online}§{max}", motd.replace('§', ""))
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    ensure!(units.len() <= MAX_JSON_CHARS, "legacy reply too long");
    let mut out = vec![0xFF];
    out.extend_from_slice(&(units.len() as u16).to_be_bytes());
    out.extend(units.iter().flat_map(|u| u.to_be_bytes()));
    Ok(out)
}

/// Decode a `0xFF` kick reply (after the `0xFF` and the u16 length) into status fields.
pub fn decode_legacy_reply(text_utf16: &[u8]) -> Result<Value> {
    let text = utf16be(text_utf16)?;
    if let Some(rest) = text.strip_prefix("§1\0") {
        let f: Vec<&str> = rest.split('\0').collect();
        ensure!(f.len() == 5, "legacy reply has {} fields", f.len());
        return Ok(json!({
            "protocol": f[0].parse::<i64>().ok(),
            "version_name": f[1],
            "motd": f[2],
            "online_players": f[3].parse::<i64>().ok(),
            "max_players": f[4].parse::<i64>().ok(),
        }));
    }
    let f: Vec<&str> = text.rsplitn(3, '§').collect();
    ensure!(f.len() == 3, "legacy reply is not motd§online§max");
    Ok(json!({
        "protocol": null,
        "version_name": null,
        "motd": f[2],
        "online_players": f[1].parse::<i64>().ok(),
        "max_players": f[0].parse::<i64>().ok(),
    }))
}
