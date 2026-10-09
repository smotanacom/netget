//! Source RCON framing (the protocol Minecraft's RCON also speaks): a little-endian i32
//! size, then id, type, a NUL-terminated body and an empty NUL-terminated string.
use anyhow::{bail, ensure, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Smallest legal size field: id, type and the two NULs.
pub const MIN_SIZE: usize = 10;
/// Largest packet a peer may send (size field), as Source servers enforce.
pub const MAX_PACKET: usize = 4096;
/// Body bytes per outgoing response packet.
pub const MAX_BODY_OUT: usize = MAX_PACKET - MIN_SIZE;
/// Largest command output the server sends, and the client accepts, in total.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Write and reply deadline.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub const SERVERDATA_AUTH: i32 = 3;
pub const SERVERDATA_AUTH_RESPONSE: i32 = 2;
pub const SERVERDATA_EXECCOMMAND: i32 = 2;
pub const SERVERDATA_RESPONSE_VALUE: i32 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub id: i32,
    pub kind: i32,
    pub body: Vec<u8>,
}

impl Packet {
    pub fn new(id: i32, kind: i32, body: impl Into<Vec<u8>>) -> Self {
        Self {
            id,
            kind,
            body: body.into(),
        }
    }
    pub fn encode(&self) -> Vec<u8> {
        let size = (8 + self.body.len() + 2) as i32;
        let mut out = Vec::with_capacity(size as usize + 4);
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.body);
        out.extend_from_slice(&[0, 0]);
        out
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Read one packet. `None` on a clean EOF before the size field; the size is checked against
/// `max` before the packet is read.
pub async fn read_packet<R: AsyncRead + Unpin>(
    reader: &mut R,
    deadline: Duration,
    max: usize,
) -> Result<Option<Packet>> {
    tokio::time::timeout(deadline, async {
        let mut size = [0u8; 4];
        match reader.read_exact(&mut size).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let size = i32::from_le_bytes(size);
        ensure!(
            size >= MIN_SIZE as i32,
            "RCON packet size {size} is below {MIN_SIZE}"
        );
        ensure!(
            size as usize <= max,
            "RCON packet size {size} exceeds {max}"
        );
        let mut rest = vec![0u8; size as usize];
        reader.read_exact(&mut rest).await?;
        let id = i32::from_le_bytes(rest[0..4].try_into()?);
        let kind = i32::from_le_bytes(rest[4..8].try_into()?);
        let mut body = rest[8..].to_vec();
        // The body and the trailing empty string each end in NUL; be lenient about the second.
        if body.last() != Some(&0) {
            bail!("RCON packet body is not NUL-terminated");
        }
        while body.last() == Some(&0) {
            body.pop();
        }
        Ok(Some(Packet { id, kind, body }))
    })
    .await
    .context("RCON read deadline")?
}

/// Split command output into response bodies of at most `MAX_BODY_OUT` bytes, at least one.
pub fn chunks(output: &[u8]) -> Vec<Vec<u8>> {
    if output.is_empty() {
        return vec![Vec::new()];
    }
    output.chunks(MAX_BODY_OUT).map(<[u8]>::to_vec).collect()
}

/// RCON bodies are text; strip NUL, which would end a body early on the wire.
pub fn body_from_text(text: &str) -> Vec<u8> {
    text.bytes().filter(|b| *b != 0).collect()
}

/// Constant-time comparison for the configured password.
pub fn same_secret(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
