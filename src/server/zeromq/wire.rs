//! ZMTP 3.1 (ZeroMQ's wire protocol, RFC 37) with the NULL mechanism: the 64-byte greeting,
//! the READY handshake with Socket-Type and Identity metadata, multipart frames, and the
//! PING/PONG, SUBSCRIBE/CANCEL and ERROR commands. Shared by the server and the client.
use anyhow::{bail, ensure, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const GREETING_LEN: usize = 64;
/// Bytes in one frame, and in one whole multipart message.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// Frames in one multipart message.
pub const MAX_FRAMES: usize = 64;
/// Bytes in one command (READY metadata, SUBSCRIBE topics, PING contexts).
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
/// Deadline for the greeting and READY exchange, and for each write.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const MORE: u8 = 0x01;
const LONG: u8 = 0x02;
const COMMAND: u8 = 0x04;

/// The greeting NetGet sends: ZMTP 3.1, NULL mechanism, as-server false (NULL has no roles).
pub fn greeting() -> [u8; GREETING_LEN] {
    let mut g = [0u8; GREETING_LEN];
    g[0] = 0xFF;
    g[9] = 0x7F;
    g[10] = 3;
    g[11] = 1;
    g[12..16].copy_from_slice(b"NULL");
    g
}

/// Check a peer's greeting; returns its (major, minor) version.
pub fn check_greeting(g: &[u8; GREETING_LEN]) -> Result<(u8, u8)> {
    ensure!(g[0] == 0xFF && g[9] & 0x01 == 0x01, "not a ZMTP 3 greeting");
    ensure!(
        g[10] >= 3,
        "ZMTP {}.{} is not supported (3.0 or later)",
        g[10],
        g[11]
    );
    let mechanism: Vec<u8> = g[12..32].iter().copied().take_while(|b| *b != 0).collect();
    ensure!(
        mechanism == b"NULL",
        "security mechanism {:?} is not supported (NULL only)",
        String::from_utf8_lossy(&mechanism)
    );
    Ok((g[10], g[11]))
}

/// One frame on the wire.
pub fn encode_frame(body: &[u8], more: bool, command: bool) -> Vec<u8> {
    let mut flags = 0u8;
    if more {
        flags |= MORE;
    }
    if command {
        flags |= COMMAND;
    }
    let mut out = Vec::with_capacity(body.len() + 9);
    if body.len() > 255 {
        out.push(flags | LONG);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    } else {
        out.push(flags);
        out.push(body.len() as u8);
    }
    out.extend_from_slice(body);
    out
}

/// A multipart message: every frame but the last carries MORE.
pub fn encode_message(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        out.extend(encode_frame(frame, i + 1 < frames.len(), false));
    }
    if frames.is_empty() {
        out.extend(encode_frame(&[], false, false));
    }
    out
}

/// A command frame: name, then its body.
pub fn encode_command(name: &str, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(1 + name.len() + body.len());
    b.push(name.len() as u8);
    b.extend_from_slice(name.as_bytes());
    b.extend_from_slice(body);
    encode_frame(&b, false, true)
}

/// READY with Socket-Type and, when given, Identity.
pub fn ready(socket_type: &str, identity: Option<&[u8]>) -> Vec<u8> {
    let mut props = Vec::new();
    let mut put = |name: &str, value: &[u8]| {
        props.push(name.len() as u8);
        props.extend_from_slice(name.as_bytes());
        props.extend_from_slice(&(value.len() as u32).to_be_bytes());
        props.extend_from_slice(value);
    };
    put("Socket-Type", socket_type.as_bytes());
    if let Some(id) = identity {
        put("Identity", id);
    }
    encode_command("READY", &props)
}

/// ERROR with a reason (at most 255 bytes).
pub fn error(reason: &str) -> Vec<u8> {
    let reason = &reason.as_bytes()[..reason.len().min(255)];
    let mut body = vec![reason.len() as u8];
    body.extend_from_slice(reason);
    encode_command("ERROR", &body)
}

/// Parse READY metadata into (name, value) pairs.
pub fn parse_metadata(mut body: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let n = body[0] as usize;
        ensure!(body.len() >= 1 + n + 4, "truncated READY property");
        let name = String::from_utf8_lossy(&body[1..1 + n]).to_string();
        let len = u32::from_be_bytes(body[1 + n..5 + n].try_into()?) as usize;
        ensure!(body.len() >= 5 + n + len, "truncated READY property value");
        out.push((name, body[5 + n..5 + n + len].to_vec()));
        body = &body[5 + n + len..];
    }
    Ok(out)
}

/// Whether two socket types may talk (ZMTP 3.1's compatibility table).
pub fn compatible(ours: &str, theirs: &str) -> bool {
    matches!(
        (ours, theirs),
        ("REQ", "REP" | "ROUTER")
            | ("REP", "REQ" | "DEALER")
            | ("DEALER", "REP" | "DEALER" | "ROUTER")
            | ("ROUTER", "REQ" | "DEALER" | "ROUTER")
            | ("PUB", "SUB" | "XSUB")
            | ("XPUB", "SUB" | "XSUB")
            | ("SUB", "PUB" | "XPUB")
            | ("XSUB", "PUB" | "XPUB")
            | ("PUSH", "PULL")
            | ("PULL", "PUSH")
            | ("PAIR", "PAIR")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    Message(Vec<Vec<u8>>),
    /// A command other than READY: its name and body.
    Command(String, Vec<u8>),
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<(u8, Vec<u8>)> {
    let flags = r.read_u8().await?;
    ensure!(
        flags & !(MORE | LONG | COMMAND) == 0,
        "reserved ZMTP frame flags 0x{flags:02x}"
    );
    let size = if flags & LONG != 0 {
        r.read_u64().await?
    } else {
        u64::from(r.read_u8().await?)
    };
    ensure!(
        size <= max as u64,
        "ZMTP frame of {size} bytes exceeds {max}"
    );
    let mut body = vec![0u8; size as usize];
    r.read_exact(&mut body).await?;
    Ok((flags, body))
}

/// Read one whole message or command. `None` on a clean EOF between them; the first byte may
/// wait `idle`, the rest of the message must follow within `HANDSHAKE_TIMEOUT` per frame.
pub async fn read_incoming<R: AsyncRead + Unpin>(
    r: &mut R,
    idle: Duration,
) -> Result<Option<Incoming>> {
    let mut first = [0u8; 1];
    match tokio::time::timeout(idle, r.read(&mut first)).await {
        Ok(Ok(0)) => return Ok(None),
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => bail!("ZMTP peer idle for {}s", idle.as_secs()),
    }
    let mut chained = (&first[..]).chain(&mut *r);
    let mut frames = Vec::new();
    let mut total = 0usize;
    loop {
        let (flags, body) = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            read_frame(&mut chained, MAX_MESSAGE_BYTES - total),
        )
        .await
        .context("ZMTP frame deadline")??;
        if flags & COMMAND != 0 {
            ensure!(frames.is_empty(), "ZMTP command inside a multipart message");
            ensure!(body.len() <= MAX_COMMAND_BYTES, "ZMTP command too large");
            ensure!(
                !body.is_empty() && body.len() > body[0] as usize,
                "truncated ZMTP command"
            );
            let n = body[0] as usize;
            let name = String::from_utf8_lossy(&body[1..1 + n]).to_string();
            return Ok(Some(Incoming::Command(name, body[1 + n..].to_vec())));
        }
        total += body.len();
        frames.push(body);
        ensure!(
            frames.len() <= MAX_FRAMES,
            "ZMTP message of more than {MAX_FRAMES} frames"
        );
        if flags & MORE == 0 {
            return Ok(Some(Incoming::Message(frames)));
        }
    }
}

/// The greeting and READY exchange. Returns the peer's (socket type, identity).
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    socket_type: &str,
    identity: Option<&[u8]>,
) -> Result<(String, Vec<u8>)> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        stream.write_all(&greeting()).await?;
        let mut theirs = [0u8; GREETING_LEN];
        stream.read_exact(&mut theirs).await?;
        if let Err(e) = check_greeting(&theirs) {
            let _ = stream.write_all(&error(&e.to_string())).await;
            return Err(e);
        }
        stream.write_all(&ready(socket_type, identity)).await?;
        let (flags, body) = read_frame(stream, MAX_COMMAND_BYTES).await?;
        ensure!(
            flags & COMMAND != 0,
            "ZMTP peer sent a message before READY"
        );
        ensure!(body.len() > 5 && body[0] == 5, "malformed ZMTP command");
        match &body[1..6] {
            b"READY" => {}
            b"ERROR" => bail!(
                "ZMTP peer refused: {}",
                String::from_utf8_lossy(&body[7.min(body.len())..])
            ),
            other => bail!(
                "ZMTP peer sent {} before READY",
                String::from_utf8_lossy(other)
            ),
        }
        let props = parse_metadata(&body[6..])?;
        let mut peer_type = String::new();
        let mut peer_identity = Vec::new();
        for (name, value) in props {
            match name.to_ascii_lowercase().as_str() {
                "socket-type" => peer_type = String::from_utf8_lossy(&value).to_string(),
                "identity" => peer_identity = value,
                _ => {}
            }
        }
        if !compatible(socket_type, &peer_type) {
            let reason = format!("{socket_type} cannot talk to {peer_type}");
            let _ = stream.write_all(&error(&reason)).await;
            bail!(reason);
        }
        Ok((peer_type, peer_identity))
    })
    .await
    .context("ZMTP handshake deadline")?
}

/// Frames as event text: each UTF-8 frame as itself, and hex for the whole message when any
/// frame is not UTF-8.
pub fn frames_to_text(frames: &[Vec<u8>]) -> (Vec<String>, &'static str) {
    if frames.iter().all(|f| std::str::from_utf8(f).is_ok()) {
        (
            frames
                .iter()
                .map(|f| String::from_utf8_lossy(f).to_string())
                .collect(),
            "utf8",
        )
    } else {
        (
            frames
                .iter()
                .map(|f| f.iter().map(|b| format!("{b:02x}")).collect())
                .collect(),
            "hex",
        )
    }
}

/// Frames from an action, by its declared encoding.
pub fn frames_from_text(
    frames: &[serde_json::Value],
    encoding: Option<&str>,
) -> Result<Vec<Vec<u8>>> {
    ensure!(frames.len() <= MAX_FRAMES, "at most {MAX_FRAMES} frames");
    let mut out = Vec::with_capacity(frames.len());
    let mut total = 0;
    for f in frames {
        let text = f.as_str().context("each frame must be a string")?;
        let bytes = match encoding.unwrap_or("utf8") {
            "utf8" => text.as_bytes().to_vec(),
            "hex" => {
                ensure!(
                    text.is_ascii() && text.len().is_multiple_of(2),
                    "hex frames must be an even number of hex digits"
                );
                (0..text.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&text[i..i + 2], 16).context("invalid hex frame"))
                    .collect::<Result<Vec<u8>>>()?
            }
            other => bail!("encoding must be utf8 or hex, not {other}"),
        };
        total += bytes.len();
        out.push(bytes);
    }
    ensure!(
        total <= MAX_MESSAGE_BYTES,
        "message exceeds {MAX_MESSAGE_BYTES} bytes"
    );
    Ok(out)
}
