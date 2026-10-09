//! Steam server query (A2S) over UDP: request parsing, the challenge exchange, encoding and
//! decoding of A2S_INFO, A2S_PLAYER and A2S_RULES, and Source split packets.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

/// Largest datagram either side sends, as Source servers use.
pub const MAX_PACKET: usize = 1400;
/// Largest request the server accepts.
pub const MAX_REQUEST: usize = 1400;
/// Payload bytes per split packet, and the size field Source servers put in it.
pub const SPLIT_CHUNK: usize = 1248;
/// Largest logical response either side builds or reassembles.
pub const MAX_RESPONSE: usize = 64 * 1024;
/// Split packets one response may span.
pub const MAX_SPLIT_PACKETS: usize = 64;
/// How long a client waits for each reply datagram.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

pub const SINGLE: [u8; 4] = [0xFF; 4];
pub const SPLIT: [u8; 4] = [0xFE, 0xFF, 0xFF, 0xFF];
pub const INFO_QUERY: &[u8] = b"Source Engine Query\0";
pub const REQUEST_INFO: u8 = b'T';
pub const REQUEST_PLAYERS: u8 = b'U';
pub const REQUEST_RULES: u8 = b'V';
pub const RESPONSE_CHALLENGE: u8 = b'A';
pub const RESPONSE_INFO: u8 = b'I';
pub const RESPONSE_PLAYERS: u8 = b'D';
pub const RESPONSE_RULES: u8 = b'E';
/// The challenge a client sends to ask for one.
pub const NO_CHALLENGE: u32 = 0xFFFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Players,
    Rules,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Info => "info",
            Kind::Players => "players",
            Kind::Rules => "rules",
        }
    }
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "info" => Kind::Info,
            "players" => Kind::Players,
            "rules" => Kind::Rules,
            other => bail!("query must be info, players or rules, not {other}"),
        })
    }
    pub fn request_byte(self) -> u8 {
        match self {
            Kind::Info => REQUEST_INFO,
            Kind::Players => REQUEST_PLAYERS,
            Kind::Rules => REQUEST_RULES,
        }
    }
    pub fn response_byte(self) -> u8 {
        match self {
            Kind::Info => RESPONSE_INFO,
            Kind::Players => RESPONSE_PLAYERS,
            Kind::Rules => RESPONSE_RULES,
        }
    }
}

/// A parsed request: what was asked and the challenge it carried, if any.
pub fn parse_request(datagram: &[u8]) -> Result<(Kind, Option<u32>)> {
    ensure!(
        datagram.len() <= MAX_REQUEST,
        "A2S request exceeds {MAX_REQUEST} bytes"
    );
    ensure!(
        datagram.len() >= 5 && datagram[..4] == SINGLE,
        "not a single-packet A2S request"
    );
    let challenge = |bytes: &[u8]| -> Option<u32> {
        bytes
            .get(..4)
            .map(|c| u32::from_le_bytes(c.try_into().expect("four bytes")))
    };
    match datagram[4] {
        REQUEST_INFO => {
            let rest = &datagram[5..];
            ensure!(
                rest.starts_with(INFO_QUERY),
                "A2S_INFO without its query string"
            );
            Ok((Kind::Info, challenge(&rest[INFO_QUERY.len()..])))
        }
        REQUEST_PLAYERS => Ok((
            Kind::Players,
            Some(challenge(&datagram[5..]).context("A2S_PLAYER without a challenge")?),
        )),
        REQUEST_RULES => Ok((
            Kind::Rules,
            Some(challenge(&datagram[5..]).context("A2S_RULES without a challenge")?),
        )),
        other => bail!("unsupported A2S request {other:#04x}"),
    }
}

pub fn encode_request(kind: Kind, challenge: Option<u32>) -> Vec<u8> {
    let mut out = SINGLE.to_vec();
    out.push(kind.request_byte());
    if kind == Kind::Info {
        out.extend_from_slice(INFO_QUERY);
    }
    if let Some(challenge) = challenge {
        out.extend_from_slice(&challenge.to_le_bytes());
    }
    out
}

pub fn encode_challenge(challenge: u32) -> Vec<u8> {
    let mut out = SINGLE.to_vec();
    out.push(RESPONSE_CHALLENGE);
    out.extend_from_slice(&challenge.to_le_bytes());
    out
}

fn cstring(out: &mut Vec<u8>, text: &str) {
    out.extend(text.bytes().filter(|b| *b != 0));
    out.push(0);
}

fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key).filter(|x| !x.is_null())
}

fn text(v: &Value, key: &str, default: &str, limit: usize) -> Result<String> {
    let value = match field(v, key) {
        None => default.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => bail!("{key} must be a string"),
    };
    ensure!(value.len() <= limit, "{key} exceeds {limit} bytes");
    Ok(value)
}

fn number(v: &Value, key: &str, default: u64, max: u64) -> Result<u64> {
    match field(v, key) {
        None => Ok(default),
        Some(n) => {
            let n = n
                .as_u64()
                .with_context(|| format!("{key} must be a non-negative integer"))?;
            ensure!(n <= max, "{key} must be at most {max}");
            Ok(n)
        }
    }
}

fn flag(v: &Value, key: &str) -> Result<bool> {
    match field(v, key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => bail!("{key} must be a boolean"),
    }
}

/// A2S_INFO payload from a handler's `a2s_info` answer (Source format, protocol 17).
pub fn encode_info(v: &Value) -> Result<Vec<u8>> {
    let mut out = SINGLE.to_vec();
    out.push(RESPONSE_INFO);
    out.push(17);
    cstring(&mut out, &text(v, "name", "", 256)?);
    cstring(&mut out, &text(v, "map", "", 128)?);
    cstring(&mut out, &text(v, "folder", "", 128)?);
    cstring(&mut out, &text(v, "game", "", 128)?);
    out.extend_from_slice(&(number(v, "app_id", 0, u16::MAX as u64)? as u16).to_le_bytes());
    out.push(number(v, "players", 0, 255)? as u8);
    out.push(number(v, "max_players", 0, 255)? as u8);
    out.push(number(v, "bots", 0, 255)? as u8);
    out.push(match text(v, "server_type", "dedicated", 16)?.as_str() {
        "dedicated" => b'd',
        "listen" => b'l',
        "proxy" => b'p',
        other => bail!("server_type must be dedicated, listen or proxy, not {other}"),
    });
    out.push(match text(v, "environment", "linux", 16)?.as_str() {
        "linux" => b'l',
        "windows" => b'w',
        "mac" => b'm',
        other => bail!("environment must be linux, windows or mac, not {other}"),
    });
    out.push(flag(v, "private")? as u8);
    out.push(flag(v, "vac")? as u8);
    cstring(&mut out, &text(v, "version", "1.0.0.0", 64)?);
    let port = field(v, "port")
        .map(|_| number(v, "port", 0, u16::MAX as u64))
        .transpose()?;
    let steam_id = field(v, "steam_id")
        .map(|_| number(v, "steam_id", 0, u64::MAX))
        .transpose()?;
    let keywords = field(v, "keywords")
        .map(|_| text(v, "keywords", "", 512))
        .transpose()?;
    let game_id = field(v, "game_id")
        .map(|_| number(v, "game_id", 0, u64::MAX))
        .transpose()?;
    let edf = (if port.is_some() { 0x80 } else { 0 })
        | (if steam_id.is_some() { 0x10 } else { 0 })
        | (if keywords.is_some() { 0x20 } else { 0 })
        | (if game_id.is_some() { 0x01 } else { 0 });
    if edf != 0 {
        out.push(edf);
        if let Some(port) = port {
            out.extend_from_slice(&(port as u16).to_le_bytes());
        }
        if let Some(steam_id) = steam_id {
            out.extend_from_slice(&steam_id.to_le_bytes());
        }
        if let Some(keywords) = keywords {
            cstring(&mut out, &keywords);
        }
        if let Some(game_id) = game_id {
            out.extend_from_slice(&game_id.to_le_bytes());
        }
    }
    Ok(out)
}

/// A2S_PLAYER payload: index, name, score (i32) and duration (f32 seconds) per player.
pub fn encode_players(v: &Value) -> Result<Vec<u8>> {
    let players = field(v, "players")
        .map(|p| p.as_array().context("players must be an array"))
        .transpose()?
        .cloned()
        .unwrap_or_default();
    ensure!(players.len() <= 255, "at most 255 players");
    let mut out = SINGLE.to_vec();
    out.push(RESPONSE_PLAYERS);
    out.push(players.len() as u8);
    for (index, player) in players.iter().enumerate() {
        out.push(index as u8);
        cstring(&mut out, &text(player, "name", "", 128)?);
        let score = match field(player, "score") {
            None => 0,
            Some(s) => s
                .as_i64()
                .filter(|s| i32::try_from(*s).is_ok())
                .context("score must be a 32-bit integer")? as i32,
        };
        out.extend_from_slice(&score.to_le_bytes());
        let duration = match field(player, "duration_secs") {
            None => 0.0,
            Some(d) => {
                d.as_f64()
                    .filter(|d| d.is_finite() && *d >= 0.0)
                    .context("duration_secs must be a non-negative number")? as f32
            }
        };
        out.extend_from_slice(&duration.to_le_bytes());
    }
    Ok(out)
}

/// A2S_RULES payload: a count and name/value string pairs, in the handler's order.
pub fn encode_rules(v: &Value) -> Result<Vec<u8>> {
    let rules = field(v, "rules")
        .map(|r| {
            r.as_object()
                .context("rules must be an object of name to value")
        })
        .transpose()?
        .cloned()
        .unwrap_or_default();
    ensure!(rules.len() <= 4096, "at most 4096 rules");
    let mut out = SINGLE.to_vec();
    out.push(RESPONSE_RULES);
    out.extend_from_slice(&(rules.len() as u16).to_le_bytes());
    for (name, value) in &rules {
        cstring(&mut out, name);
        cstring(
            &mut out,
            value
                .as_str()
                .with_context(|| format!("rule {name} must be a string"))?,
        );
    }
    Ok(out)
}

/// Split a logical response into datagrams: one single packet, or Source split packets.
pub fn packetize(payload: &[u8], id: u32) -> Result<Vec<Vec<u8>>> {
    ensure!(
        payload.len() <= MAX_RESPONSE,
        "A2S response exceeds {MAX_RESPONSE} bytes"
    );
    if payload.len() <= MAX_PACKET {
        return Ok(vec![payload.to_vec()]);
    }
    let chunks: Vec<&[u8]> = payload.chunks(SPLIT_CHUNK).collect();
    ensure!(
        chunks.len() <= MAX_SPLIT_PACKETS,
        "A2S response needs more than {MAX_SPLIT_PACKETS} packets"
    );
    let id = id & 0x7FFF_FFFF;
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(number, chunk)| {
            let mut out = SPLIT.to_vec();
            out.extend_from_slice(&id.to_le_bytes());
            out.push(chunks.len() as u8);
            out.push(number as u8);
            out.extend_from_slice(&(SPLIT_CHUNK as u16).to_le_bytes());
            out.extend_from_slice(chunk);
            out
        })
        .collect())
}

/// Reassembly of split packets on the client side.
#[derive(Default)]
pub struct Reassembly {
    id: Option<u32>,
    total: usize,
    parts: Vec<Option<Vec<u8>>>,
}

impl Reassembly {
    /// Feed one datagram; `Some(payload)` once the logical response is complete.
    pub fn feed(&mut self, datagram: &[u8]) -> Result<Option<Vec<u8>>> {
        ensure!(datagram.len() >= 5, "A2S datagram too short");
        if datagram[..4] == SINGLE {
            return Ok(Some(datagram.to_vec()));
        }
        ensure!(datagram[..4] == SPLIT, "A2S datagram has an unknown header");
        ensure!(datagram.len() >= 12, "A2S split packet too short");
        let id = u32::from_le_bytes(datagram[4..8].try_into()?);
        ensure!(
            id & 0x8000_0000 == 0,
            "compressed A2S responses are not supported"
        );
        let total = datagram[8] as usize;
        let number = datagram[9] as usize;
        ensure!(
            (1..=MAX_SPLIT_PACKETS).contains(&total) && number < total,
            "A2S split packet numbering out of range"
        );
        match self.id {
            None => {
                self.id = Some(id);
                self.total = total;
                self.parts = vec![None; total];
            }
            Some(current) => ensure!(
                current == id && self.total == total,
                "A2S split packets from two responses"
            ),
        }
        self.parts[number] = Some(datagram[12..].to_vec());
        if self.parts.iter().all(Option::is_some) {
            let payload: Vec<u8> = self.parts.iter().flatten().flatten().copied().collect();
            ensure!(
                payload.len() <= MAX_RESPONSE,
                "A2S response exceeds {MAX_RESPONSE} bytes"
            );
            ensure!(
                payload.len() >= 5 && payload[..4] == SINGLE,
                "reassembled A2S response lacks its header"
            );
            return Ok(Some(payload));
        }
        Ok(None)
    }
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8> {
        let b = *self.data.get(self.at).context("A2S response truncated")?;
        self.at += 1;
        Ok(b)
    }
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self
            .data
            .get(self.at..self.at + N)
            .context("A2S response truncated")?;
        self.at += N;
        Ok(slice.try_into()?)
    }
    fn cstring(&mut self) -> Result<String> {
        let rest = self.data.get(self.at..).context("A2S response truncated")?;
        let end = rest
            .iter()
            .position(|b| *b == 0)
            .context("A2S string not terminated")?;
        self.at += end + 1;
        Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
    }
    fn more(&self) -> bool {
        self.at < self.data.len()
    }
}

/// Decode a complete response payload (after reassembly) into event data.
pub fn decode_response(kind: Kind, payload: &[u8]) -> Result<Value> {
    ensure!(
        payload.len() >= 5 && payload[..4] == SINGLE,
        "A2S response lacks its header"
    );
    ensure!(
        payload[4] == kind.response_byte(),
        "A2S answered {:#04x} to a {} query",
        payload[4],
        kind.name()
    );
    let mut r = Reader {
        data: &payload[5..],
        at: 0,
    };
    Ok(match kind {
        Kind::Info => {
            let protocol = r.u8()?;
            let name = r.cstring()?;
            let map = r.cstring()?;
            let folder = r.cstring()?;
            let game = r.cstring()?;
            let app_id = u16::from_le_bytes(r.bytes()?);
            let players = r.u8()?;
            let max_players = r.u8()?;
            let bots = r.u8()?;
            let server_type = match r.u8()? {
                b'd' => "dedicated",
                b'l' => "listen",
                b'p' => "proxy",
                _ => "unknown",
            };
            let environment = match r.u8()? {
                b'l' => "linux",
                b'w' => "windows",
                b'm' | b'o' => "mac",
                _ => "unknown",
            };
            let private = r.u8()? != 0;
            let vac = r.u8()? != 0;
            let version = r.cstring()?;
            let mut info = json!({"protocol": protocol, "name": name, "map": map, "folder": folder, "game": game, "app_id": app_id, "players": players, "max_players": max_players, "bots": bots, "server_type": server_type, "environment": environment, "private": private, "vac": vac, "version": version});
            if r.more() {
                let edf = r.u8()?;
                if edf & 0x80 != 0 {
                    info["port"] = json!(u16::from_le_bytes(r.bytes()?));
                }
                if edf & 0x10 != 0 {
                    info["steam_id"] = json!(u64::from_le_bytes(r.bytes()?));
                }
                if edf & 0x40 != 0 {
                    info["sourcetv_port"] = json!(u16::from_le_bytes(r.bytes()?));
                    info["sourcetv_name"] = json!(r.cstring()?);
                }
                if edf & 0x20 != 0 {
                    info["keywords"] = json!(r.cstring()?);
                }
                if edf & 0x01 != 0 {
                    info["game_id"] = json!(u64::from_le_bytes(r.bytes()?));
                }
            }
            info
        }
        Kind::Players => {
            let count = r.u8()?;
            let mut players = Vec::new();
            for _ in 0..count {
                let index = r.u8()?;
                let name = r.cstring()?;
                let score = i32::from_le_bytes(r.bytes()?);
                let duration = f32::from_le_bytes(r.bytes()?);
                players.push(json!({"index": index, "name": name, "score": score, "duration_secs": duration}));
            }
            json!(players)
        }
        Kind::Rules => {
            let count = u16::from_le_bytes(r.bytes()?);
            let mut rules = Map::new();
            for _ in 0..count {
                let name = r.cstring()?;
                let value = r.cstring()?;
                rules.insert(name, Value::String(value));
            }
            Value::Object(rules)
        }
    })
}
